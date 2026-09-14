//! The `transaction` resource: one pooled connection pinned between `BEGIN`
//! and `COMMIT` or `ROLLBACK`.

use std::sync::Arc;

use tokio::sync::Mutex;
use wasmtime::component::{Accessor, FutureReader, Resource, StreamReader};

use crate::engine::ctx::{ActiveCtx, SharedCtx};

use super::bindings::betty_blocks::retrieval::store::{HostTransaction, HostTransactionWithStore};
use super::bindings::betty_blocks::retrieval::types;
use super::bindings::conversions::row_to_values;
use super::bindings::wasmcloud::postgres::types::Row;
use super::params::{self, Bound};
use super::store::{QueryResult, checkout, plugin};
use super::{BettyRetrieval, errors};

const CANCELLED: &str = "a statement was cancelled before it finished";

/// What a component holds for an open transaction.
pub struct TxHandle {
    conn: Arc<Mutex<TxConn>>,
}

struct TxConn {
    /// `None` once `commit` or the resource's drop has ended the transaction.
    client: Option<deadpool_postgres::Object>,
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
    // No ROLLBACK here: that needs a spawned task, which a shutting-down
    // runtime can drop unpolled, pooling the connection mid-transaction.
    fn drop(&mut self) {
        self.detach();
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

/// Check out a connection and open a transaction on it.
pub(crate) async fn begin(plugin: &BettyRetrieval) -> Result<TxHandle, types::Error> {
    let client = checkout(plugin).await?;
    // Built before `BEGIN` is sent, so a call cancelled awaiting the reply
    // leaves `TxConn`'s drop to close the possibly open transaction.
    let conn = TxConn {
        client: Some(client),
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

/// Every row, read before returning: rows streamed lazily would keep the
/// pinned connection busy, and a `commit` would wait behind them.
async fn collect_query(
    plugin: &BettyRetrieval,
    conn: &Mutex<TxConn>,
    sql: &str,
    params: Vec<types::Param>,
) -> Result<(Vec<String>, Vec<Row>), types::Error> {
    let bound = params::resolve(plugin.embedder(), params).await?;
    let mut conn = conn.lock().await;
    let (client, aborted) = conn.statement_parts()?;
    let stmt = prepare_checked(client, aborted, sql, &bound).await?;
    let guard = StatementGuard::arm(aborted);
    let rows = guard.finish(client.query(&stmt, &params::as_sql(&bound)).await)?;
    let columns = stmt
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    let rows = rows
        .iter()
        .map(|row| row_to_values(row).map_err(errors::value_conversion))
        .collect::<Result<_, _>>()?;
    Ok((columns, rows))
}

async fn run_execute(
    plugin: &BettyRetrieval,
    conn: &Mutex<TxConn>,
    sql: &str,
    params: Vec<types::Param>,
) -> Result<u64, types::Error> {
    let bound = params::resolve(plugin.embedder(), params).await?;
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
        let plugin = plugin(accessor)?;
        let conn = conn(accessor, &tx)?;
        let (columns, rows) = match collect_query(&plugin, &conn, &sql, params).await {
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
        let plugin = plugin(accessor)?;
        let conn = conn(accessor, &tx)?;
        Ok(run_execute(&plugin, &conn, &sql, params).await)
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
