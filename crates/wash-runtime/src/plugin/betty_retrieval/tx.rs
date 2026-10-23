//! The `transaction` resource: one pooled connection pinned between `BEGIN`
//! and `COMMIT` or `ROLLBACK`.

use std::sync::Arc;

use futures::TryStreamExt as _;
use tokio::sync::{Mutex, OwnedSemaphorePermit};
use wasmtime::component::{Accessor, FutureReader, Resource, StreamReader};

use crate::engine::ctx::{ActiveCtx, SharedCtx};

use super::bindings::betty_blocks::retrieval::store::{HostTransaction, HostTransactionWithStore};
use super::bindings::betty_blocks::retrieval::types;
use super::bindings::conversions::row_to_values;
use super::bindings::wasmcloud::postgres::types::Row;
use super::cancel::Canceller;
use super::cost;
use super::params::{self, Bound};
use super::store::{QueryResult, caller, checkout};
use super::{BettyRetrieval, errors};

const CANCELLED: &str = "a statement was cancelled before it finished";

/// What a component holds for an open transaction.
pub struct TxHandle {
    conn: Arc<Mutex<TxConn>>,
}

struct TxConn {
    /// `None` once `commit` or the resource's drop has ended the transaction.
    client: Option<deadpool_postgres::Object>,
    /// One of the workload's permits, held for as long as the transaction;
    /// `None` once handed to the canceller.
    share: Option<OwnedSemaphorePermit>,
    canceller: Canceller,
    /// Why the transaction can no longer commit: the first database error, or
    /// a statement cancelled before it finished.
    aborted: Option<String>,
}

impl TxConn {
    fn client(&self) -> Result<&deadpool_postgres::Object, types::Error> {
        self.client.as_ref().ok_or_else(finished)
    }

    /// The pinned connection, and where a statement run on it records why the
    /// transaction can no longer commit.
    fn statement_parts(
        &mut self,
    ) -> Result<(&deadpool_postgres::Object, &mut Option<String>), types::Error> {
        let client = self.client.as_ref().ok_or_else(finished)?;
        Ok((client, &mut self.aborted))
    }

    /// Back to the pool, once the server has ended the transaction.
    fn release(&mut self) {
        drop(self.client.take());
    }

    /// Closed instead of pooled; Postgres aborts an open transaction when its
    /// session ends.
    fn detach(&mut self) {
        if let Some(client) = self.client.take() {
            drop(deadpool_postgres::Object::take(client));
        }
    }
}

impl Drop for TxConn {
    // Dropped still holding its connection, the transaction may be in the
    // middle of a statement: the canceller asks the server to cancel it, then
    // closes the connection, which ends the transaction. No ROLLBACK here: the
    // canceller's task, which a shutting-down runtime can drop unpolled, never
    // pools the connection.
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            self.canceller.close(client, self.share.take());
        }
    }
}

fn finished() -> types::Error {
    errors::connection_failed("this transaction has already finished".to_string())
}

/// Keeps the first reason only; every later statement fails because of it.
fn record_abort(aborted: &mut Option<String>, reason: &str) {
    if aborted.is_none() {
        *aborted = Some(reason.to_string());
    }
}

/// Marks the transaction aborted if dropped before [`Self::finish`].
///
/// A statement future dropped mid-flight may already have reached the server,
/// and whether it applied is unknown, so the transaction must not commit.
struct StatementGuard<'a> {
    aborted: &'a mut Option<String>,
    armed: bool,
}

impl<'a> StatementGuard<'a> {
    fn arm(aborted: &'a mut Option<String>) -> Self {
        Self {
            aborted,
            armed: true,
        }
    }

    /// Disarm, keeping `reason` as why the transaction can no longer commit.
    fn refuse(mut self, reason: &str) {
        self.armed = false;
        record_abort(self.aborted, reason);
    }

    /// Disarm, keeping a database error as the reason the transaction can no
    /// longer commit.
    fn finish<T>(mut self, outcome: Result<T, tokio_postgres::Error>) -> Result<T, types::Error> {
        self.armed = false;
        match outcome {
            Ok(value) => Ok(value),
            Err(e) => {
                if let Some(db) = e.as_db_error() {
                    record_abort(self.aborted, db.message());
                }
                Err(errors::postgres(&e))
            }
        }
    }
}

impl Drop for StatementGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            record_abort(self.aborted, CANCELLED);
        }
    }
}

/// Take one of `workload`'s permits, check out a connection and open a
/// transaction on it.
pub(crate) async fn begin(
    plugin: &BettyRetrieval,
    workload: &Arc<str>,
) -> Result<TxHandle, types::Error> {
    let canceller = plugin
        .canceller()
        .map_err(|e| errors::connection_failed(format!("{e:#}")))?
        .clone();
    let share = plugin.shares().acquire(workload).await?;
    let client = checkout(plugin).await?;
    // Built before `BEGIN` is sent, so a call cancelled awaiting the reply
    // leaves `TxConn`'s drop to close the possibly open transaction.
    let conn = TxConn {
        client: Some(client),
        share: Some(share),
        canceller,
        aborted: None,
    };
    conn.client()?
        .batch_execute("BEGIN")
        .await
        .map_err(|e| errors::postgres(&e))?;
    Ok(TxHandle {
        conn: Arc::new(Mutex::new(conn)),
    })
}

fn conn<U>(
    accessor: &Accessor<U, SharedCtx>,
    tx: &Resource<TxHandle>,
) -> wasmtime::Result<Arc<Mutex<TxConn>>> {
    accessor.with(|mut access| wasmtime::Result::Ok(Arc::clone(&access.get().table.get(tx)?.conn)))
}

/// Prepare `sql` under a guard, then refuse it before it runs if its
/// placeholders and the bound parameters differ in number.
async fn prepare_checked(
    client: &deadpool_postgres::Object,
    aborted: &mut Option<String>,
    sql: &str,
    bound: &[Bound],
) -> Result<tokio_postgres::Statement, types::Error> {
    let guard = StatementGuard::arm(aborted);
    let stmt = guard.finish(client.prepare(sql).await)?;
    params::check_count(stmt.params().len(), bound.len())?;
    Ok(stmt)
}

/// The most rows one transaction query may read, whatever their bytes.
pub(crate) const TRANSACTION_QUERY_MAX_ROWS: usize = 1_000_000;

/// Why [`read_capped`] stopped.
#[derive(Debug)]
enum ReadError {
    Database(tokio_postgres::Error),
    /// The rows came to more than the cap.
    TooLarge,
    /// More than [`TRANSACTION_QUERY_MAX_ROWS`] rows.
    TooManyRows,
}

/// What one transaction query's rows have cost so far, against its caps.
struct Budget {
    max_bytes: usize,
    bytes: usize,
    rows: usize,
}

impl Budget {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            bytes: 0,
            rows: 0,
        }
    }

    /// Count one more row, which costs `cost` decoded (see [`cost::row_cost`]).
    fn charge(&mut self, cost: usize) -> Result<(), ReadError> {
        self.rows += 1;
        self.bytes = self.bytes.saturating_add(cost);
        if self.rows > TRANSACTION_QUERY_MAX_ROWS {
            Err(ReadError::TooManyRows)
        } else if self.bytes > self.max_bytes {
            Err(ReadError::TooLarge)
        } else {
            Ok(())
        }
    }
}

/// The rows of a query, decoded for the guest, and the first column that would
/// not decode.
struct Collected {
    rows: Vec<Row>,
    /// Reading goes on past a value that will not decode, so the statement
    /// finishes and the transaction stays usable; the rows are not kept.
    conversion_error: Option<String>,
}

/// Every row of `stmt`, decoded as it arrives, until they cost more than
/// `max_bytes` or number more than [`TRANSACTION_QUERY_MAX_ROWS`]. A row costs
/// what it decodes to (see [`cost::row_cost`]), which is checked before it is
/// decoded, so rows of NULLs count too, and so does a `text[]` of empty
/// strings. The row as it came off the wire is dropped once decoded: only the
/// decoded rows, which the budget counts, are held.
async fn read_capped(
    client: &deadpool_postgres::Object,
    stmt: &tokio_postgres::Statement,
    bound: &[Bound],
    max_bytes: usize,
) -> Result<Collected, ReadError> {
    let stream = client
        .query_raw(stmt, params::as_sql(bound))
        .await
        .map_err(ReadError::Database)?;
    let mut stream = std::pin::pin!(stream);
    let mut collected = Collected {
        rows: Vec::new(),
        conversion_error: None,
    };
    let mut budget = Budget::new(max_bytes);
    while let Some(row) = stream.try_next().await.map_err(ReadError::Database)? {
        budget.charge(cost::row_cost(&row))?;
        if collected.conversion_error.is_some() {
            continue;
        }
        match row_to_values(&row) {
            Ok(values) => collected.rows.push(values),
            Err(e) => collected.conversion_error = Some(e),
        }
    }
    Ok(collected)
}

/// Every row, read before returning: rows streamed lazily would keep the
/// pinned connection busy, and a `commit` would wait behind them. So the rows
/// are capped, by what they cost the host and by their number: a result over
/// either cap fails the query, and the transaction can no longer commit,
/// since a statement such as `UPDATE ... RETURNING` may have changed rows the
/// guest never saw.
async fn collect_query(
    plugin: &BettyRetrieval,
    workload: &Arc<str>,
    conn: &Mutex<TxConn>,
    sql: &str,
    params: Vec<types::Param>,
) -> Result<(Vec<String>, Vec<Row>), types::Error> {
    let bound = params::resolve(plugin.embedder(), plugin.embed_gate(), workload, params).await?;
    let mut conn = conn.lock().await;
    let (client, aborted) = conn.statement_parts()?;
    let stmt = prepare_checked(client, aborted, sql, &bound).await?;
    let guard = StatementGuard::arm(aborted);
    let max_bytes = plugin.transaction_query_max_bytes();
    let collected = match read_capped(client, &stmt, &bound, max_bytes).await {
        Ok(collected) => collected,
        Err(ReadError::Database(e)) => return guard.finish(Err(e)),
        Err(ReadError::TooLarge) => {
            let reason = format!(
                "the query's rows are larger than {max_bytes} bytes, the limit for one \
                 transaction query; select fewer rows, or read them in pieces"
            );
            guard.refuse(&reason);
            return Err(errors::other(reason));
        }
        Err(ReadError::TooManyRows) => {
            let reason = format!(
                "the query has more than {TRANSACTION_QUERY_MAX_ROWS} rows, the limit for one \
                 transaction query; select fewer rows, or read them in pieces"
            );
            guard.refuse(&reason);
            return Err(errors::other(reason));
        }
    };
    let collected = guard.finish(Ok::<_, tokio_postgres::Error>(collected))?;
    if let Some(e) = collected.conversion_error {
        return Err(errors::value_conversion(e));
    }
    let columns = stmt
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    Ok((columns, collected.rows))
}

async fn run_execute(
    plugin: &BettyRetrieval,
    workload: &Arc<str>,
    conn: &Mutex<TxConn>,
    sql: &str,
    params: Vec<types::Param>,
) -> Result<u64, types::Error> {
    let bound = params::resolve(plugin.embedder(), plugin.embed_gate(), workload, params).await?;
    let mut conn = conn.lock().await;
    let (client, aborted) = conn.statement_parts()?;
    let stmt = prepare_checked(client, aborted, sql, &bound).await?;
    let guard = StatementGuard::arm(aborted);
    guard.finish(client.execute(&stmt, &params::as_sql(&bound)).await)
}

async fn run_batch(conn: &Mutex<TxConn>, sql: &str) -> Result<(), types::Error> {
    let mut conn = conn.lock().await;
    let (client, aborted) = conn.statement_parts()?;
    let guard = StatementGuard::arm(aborted);
    guard.finish(client.batch_execute(sql).await)
}

/// `COMMIT`, unless an earlier statement aborted the transaction: then
/// `ROLLBACK`, and say nothing was committed.
async fn commit(conn: &Mutex<TxConn>) -> Result<(), types::Error> {
    let mut conn = conn.lock().await;
    let client = conn.client()?;
    if let Some(first_error) = conn.aborted.clone() {
        match client.batch_execute("ROLLBACK").await {
            Ok(()) => conn.release(),
            Err(_) => conn.detach(),
        }
        return Err(errors::aborted_transaction(first_error));
    }
    match client.batch_execute("COMMIT").await {
        Ok(()) => {
            conn.release();
            Ok(())
        }
        Err(e) => {
            conn.detach();
            Err(errors::postgres(&e))
        }
    }
}

/// A transaction dropped without `commit`. Its connection returns to the pool
/// only if the server confirmed the `ROLLBACK`.
async fn roll_back(conn: &Mutex<TxConn>) {
    let mut conn = conn.lock().await;
    let Ok(client) = conn.client() else {
        return;
    };
    match client.batch_execute("ROLLBACK").await {
        Ok(()) => conn.release(),
        Err(_) => conn.detach(),
    }
}

impl HostTransaction for ActiveCtx<'_> {}

impl<U> HostTransactionWithStore<U> for SharedCtx {
    async fn query(
        accessor: &Accessor<U, Self>,
        tx: Resource<TxHandle>,
        sql: String,
        params: Vec<types::Param>,
    ) -> wasmtime::Result<Result<QueryResult, types::Error>> {
        let (plugin, workload) = caller(accessor)?;
        let conn = conn(accessor, &tx)?;
        let (columns, rows) = match collect_query(&plugin, &workload, &conn, &sql, params).await {
            Ok(collected) => collected,
            Err(e) => return Ok(Err(e)),
        };
        accessor.with(|mut access| {
            let rows = StreamReader::new(&mut access, rows)?;
            let done = FutureReader::new(
                &mut access,
                std::future::ready(Ok::<_, wasmtime::Error>(Ok::<(), types::Error>(()))),
            )?;
            wasmtime::Result::Ok(Ok((columns, rows, done)))
        })
    }

    async fn execute(
        accessor: &Accessor<U, Self>,
        tx: Resource<TxHandle>,
        sql: String,
        params: Vec<types::Param>,
    ) -> wasmtime::Result<Result<u64, types::Error>> {
        let (plugin, workload) = caller(accessor)?;
        let conn = conn(accessor, &tx)?;
        Ok(run_execute(&plugin, &workload, &conn, &sql, params).await)
    }

    async fn batch(
        accessor: &Accessor<U, Self>,
        tx: Resource<TxHandle>,
        sql: String,
    ) -> wasmtime::Result<Result<(), types::Error>> {
        let conn = conn(accessor, &tx)?;
        Ok(run_batch(&conn, &sql).await)
    }

    async fn commit(
        accessor: &Accessor<U, Self>,
        tx: Resource<TxHandle>,
    ) -> wasmtime::Result<Result<(), types::Error>> {
        let handle = accessor.with(|mut access| access.get().table.delete(tx))?;
        Ok(commit(&handle.conn).await)
    }

    async fn drop(accessor: &Accessor<U, Self>, tx: Resource<TxHandle>) -> wasmtime::Result<()> {
        let handle = accessor.with(|mut access| access.get().table.delete(tx))?;
        roll_back(&handle.conn).await;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_statement_dropped_before_it_finishes_aborts_the_transaction() {
        let mut aborted = None;
        drop(StatementGuard::arm(&mut aborted));
        assert_eq!(
            aborted.as_deref(),
            Some("a statement was cancelled before it finished")
        );
    }

    #[test]
    fn a_statement_that_finished_leaves_the_transaction_committable() {
        let mut aborted = None;
        let outcome =
            StatementGuard::arm(&mut aborted).finish(Ok::<_, tokio_postgres::Error>(7_u64));
        assert!(matches!(outcome, Ok(7)));
        assert_eq!(aborted, None);
    }

    /// Rows of one NULL each used to cost nothing, so no number of them ever
    /// reached the cap.
    #[test]
    fn rows_of_nulls_reach_the_cap() {
        let mut budget = Budget::new(1024);
        let null_row = cost::cost(1, 0);
        let fit = 1024 / null_row;
        for _ in 0..fit {
            budget.charge(null_row).expect("within the cap");
        }
        assert!(matches!(budget.charge(null_row), Err(ReadError::TooLarge)));
    }

    #[test]
    fn more_rows_than_the_row_cap_are_refused_whatever_their_bytes() {
        let mut budget = Budget::new(usize::MAX);
        for _ in 0..TRANSACTION_QUERY_MAX_ROWS {
            budget.charge(0).expect("within the row cap");
        }
        assert!(matches!(budget.charge(0), Err(ReadError::TooManyRows)));
    }

    /// `SELECT array_fill(''::text, ARRAY[15000000])` is 60 MB on the wire,
    /// under the 64 MiB cap, and 360 MB decoded.
    #[test]
    fn a_row_that_is_small_on_the_wire_and_large_decoded_reaches_the_cap() {
        let count = 15_000_000_usize;
        let wire_only = cost::cost(1, 20 + 4 * count);
        let mut budget = Budget::new(64 * 1024 * 1024);
        budget
            .charge(wire_only)
            .expect("counted by its wire bytes it fits");
        let mut raw = vec![0_u8; 20 + 4 * count];
        raw[0..4].copy_from_slice(&1_i32.to_be_bytes());
        raw[8..12].copy_from_slice(&25_u32.to_be_bytes());
        raw[12..16].copy_from_slice(&i32::try_from(count).unwrap().to_be_bytes());
        raw[16..20].copy_from_slice(&1_i32.to_be_bytes());
        let decoded = cost::cost(
            1,
            cost::column_bytes(&tokio_postgres::types::Type::TEXT_ARRAY, &raw),
        );
        assert!(decoded >= count * size_of::<String>());
        let mut budget = Budget::new(64 * 1024 * 1024);
        assert!(matches!(budget.charge(decoded), Err(ReadError::TooLarge)));
    }

    #[test]
    fn only_the_first_abort_reason_is_kept() {
        let mut aborted = None;
        record_abort(
            &mut aborted,
            "duplicate key value violates unique constraint",
        );
        record_abort(
            &mut aborted,
            "current transaction is aborted, commands ignored until end of transaction block",
        );
        drop(StatementGuard::arm(&mut aborted));
        assert_eq!(
            aborted.as_deref(),
            Some("duplicate key value violates unique constraint")
        );
    }
}
