//! Drives the `betty-blocks:retrieval@0.1.0` host plugin from a guest. Each
//! request path runs one check and answers with what the guest observed:
//!
//!   - `/embed-text` — an `embed-text` parameter, as `vector_dims` sees it.
//!   - `/embed-texts` — an `embed-texts` array unnested `WITH ORDINALITY`, each
//!     element matched against its own text embedded alone.
//!   - `/null` — a null `value` bound against `$1::text`.
//!   - `/commit-persists?table=` — an insert committed in a transaction; the
//!     test reads it back on a session of its own.
//!   - `/drop-rolls-back?table=` — an insert whose transaction is dropped,
//!     counted by the next statement.
//!   - `/commit-after-failure` — `commit` after a statement that failed.
//!   - `/space` — the plugin's embedding space.
//!   - `/dropped-transaction` — which session serves the statement after a
//!     dropped transaction.
//!   - `/abandoned-stream` — which session serves the statement after a query
//!     stream dropped at its first row.
//!   - `/second-begin` — `begin` while a transaction holds the only connection.
//!   - `/cancelled-statement` — `commit` after a statement dropped mid-flight.
//!   - `/placeholder-mismatch` — a statement bound with too few parameters.
//!   - `/undecodable-column` — a column no `pg-value` can hold.
//!
//! The test runs the plugin with a pool of one connection, so the statements a
//! route runs share one Postgres session unless the plugin replaced it.

mod bindings {
    wit_bindgen::generate!({ generate_all });
}

use bindings::betty_blocks::retrieval::store::{self, Transaction};
use bindings::betty_blocks::retrieval::types::{Error, Param, Role};
use bindings::exports::wasi::http::handler::Guest as Handler;
use bindings::wasi::clocks::monotonic_clock;
use bindings::wasi::http::types::{ErrorCode, Fields, Request, Response};
use bindings::wasmcloud::postgres::types::{Error as PgError, PgValue};
use futures::future::{select, Either};

/// Long enough for the raced `pg_sleep(2)` to be running on the server when
/// the timer fires, and far short of the sleep itself.
const CANCEL_AFTER_NS: u64 = 250_000_000;

struct Component;

impl Handler for Component {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let path = request.get_path_with_query().unwrap_or_default();
        let body = run(&path)
            .await
            .unwrap_or_else(|failure| format!("error: {failure}"));
        Ok(respond(body.into_bytes()))
    }
}

/// What the check at `path` observed, or why it could not run to the end.
async fn run(path: &str) -> Result<String, String> {
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    match route {
        "/embed-text" => embed_text().await,
        "/embed-texts" => embed_texts().await,
        "/null" => null_value().await,
        "/commit-persists" => commit_persists(&table(query)?).await,
        "/drop-rolls-back" => drop_rolls_back(&table(query)?).await,
        "/commit-after-failure" => commit_after_failure().await,
        "/space" => Ok(space()),
        "/dropped-transaction" => dropped_transaction().await,
        "/abandoned-stream" => abandoned_stream().await,
        "/second-begin" => second_begin().await,
        "/cancelled-statement" => cancelled_statement().await,
        "/placeholder-mismatch" => placeholder_mismatch().await,
        "/undecodable-column" => undecodable_column().await,
        other => Err(format!("no route {other}")),
    }
}

async fn embed_text() -> Result<String, String> {
    let rows = rows(
        "SELECT vector_dims($1::vector)",
        vec![Param::EmbedText((
            "which models hold orders".to_string(),
            Role::Query,
        ))],
    )
    .await?;
    Ok(format!("dims={}", integer(only(&rows)?)?))
}

/// Names each element of the array by the lone embedding it equals, in
/// ordinality order, so a reordered array cannot pass.
async fn embed_texts() -> Result<String, String> {
    let alone = |text: &str| Param::EmbedText((text.to_string(), Role::Passage));
    let texts = ["alpha", "beta", "alpha", "gamma"]
        .map(String::from)
        .to_vec();
    let rows = rows(
        "SELECT string_agg(CASE WHEN t.v = $2::vector THEN 'alpha' \
                                WHEN t.v = $3::vector THEN 'beta' \
                                WHEN t.v = $4::vector THEN 'gamma' \
                                ELSE '?' END, ',' ORDER BY t.ord) \
         FROM unnest($1::vector[]) WITH ORDINALITY AS t(v, ord)",
        vec![
            Param::EmbedTexts((texts, Role::Passage)),
            alone("alpha"),
            alone("beta"),
            alone("gamma"),
        ],
    )
    .await?;
    match only(&rows)? {
        PgValue::Text(order) => Ok(format!("order={order}")),
        other => Err(format!("expected text, got {other:?}")),
    }
}

async fn null_value() -> Result<String, String> {
    let rows = rows("SELECT $1::text IS NULL", vec![Param::Value(PgValue::Null)]).await?;
    match only(&rows)? {
        PgValue::Bool(is_null) => Ok(format!("is-null={is_null}")),
        other => Err(format!("expected a bool, got {other:?}")),
    }
}

async fn commit_persists(table: &str) -> Result<String, String> {
    let tx = begin().await?;
    tx.execute(
        format!("INSERT INTO {table} (v) VALUES ($1)"),
        vec![text("kept")],
    )
    .await
    .map_err(debug)?;
    Transaction::commit(tx).await.map_err(debug)?;
    Ok("committed".to_string())
}

/// Counted on the pool's only connection, the one the transaction held: had
/// the transaction been left open there, the count would see its own row.
async fn drop_rolls_back(table: &str) -> Result<String, String> {
    let tx = begin().await?;
    tx.execute(
        format!("INSERT INTO {table} (v) VALUES ($1)"),
        vec![text("gone")],
    )
    .await
    .map_err(debug)?;
    drop(tx);
    let rows = rows(&format!("SELECT count(*) FROM {table}"), Vec::new()).await?;
    Ok(format!("rows={}", integer(only(&rows)?)?))
}

async fn commit_after_failure() -> Result<String, String> {
    let tx = begin().await?;
    if let Ok(affected) = tx.execute("SELECT 1/0".to_string(), Vec::new()).await {
        return Err(format!("SELECT 1/0 succeeded, affecting {affected} rows"));
    }
    failed_commit(tx).await
}

fn space() -> String {
    let space = store::space();
    format!(
        "space-id={} model-id={} dimension={}",
        space.space_id, space.model_id, space.dimension
    )
}

/// A connection back in the pool serves the next statement on the same
/// session; one replaced instead shows a new backend, and one stranded makes
/// the next statement `pool-exhausted`.
async fn dropped_transaction() -> Result<String, String> {
    let before = backend_pid().await?;
    drop(begin().await?);
    sessions(before, backend_pid().await?)
}

/// 5000 rows overfill the host's row channel, so its fetch task is blocked
/// sending when the guest walks away.
async fn abandoned_stream() -> Result<String, String> {
    let (_columns, mut stream, completion) = store::query(
        "SELECT pg_backend_pid(), g FROM generate_series(1, 5000) AS g".to_string(),
        Vec::new(),
    )
    .await
    .map_err(debug)?;
    let first = stream.next().await.ok_or("the query streamed no rows")?;
    drop(stream);
    drop(completion);
    let before = integer(first.first().ok_or("the first row has no columns")?)?;
    sessions(before, backend_pid().await?)
}

async fn second_begin() -> Result<String, String> {
    let held = begin().await?;
    let second = store::begin().await;
    drop(held);
    match second {
        Err(Error::PoolExhausted) => Ok("second-begin=pool-exhausted".to_string()),
        other => Err(format!("the second begin returned {other:?}")),
    }
}

async fn cancelled_statement() -> Result<String, String> {
    let tx = begin().await?;
    let statement = Box::pin(tx.execute("SELECT pg_sleep(2)".to_string(), Vec::new()));
    let timer = Box::pin(monotonic_clock::wait_for(CANCEL_AFTER_NS));
    // The race's losing call is dropped at the end of this statement, still in
    // flight, which cancels it on the host.
    if let Either::Left((finished, _)) = select(statement, timer).await {
        return Err(format!(
            "pg_sleep finished before the timer fired: {finished:?}"
        ));
    }
    failed_commit(tx).await
}

async fn placeholder_mismatch() -> Result<String, String> {
    match store::query("SELECT $1::text".to_string(), Vec::new()).await {
        Err(Error::Postgres(PgError::InvalidParams(message))) => {
            Ok(format!("invalid-params={message}"))
        }
        Err(other) => Err(format!("the query failed with {other:?}")),
        Ok(_) => Err("the query ran with its placeholder unbound".to_string()),
    }
}

/// The statement starts, so the decode failure arrives on the completion
/// future rather than as the call's own error.
async fn undecodable_column() -> Result<String, String> {
    let (_columns, mut stream, completion) =
        store::query("SELECT '[1,2,3]'::vector".to_string(), Vec::new())
            .await
            .map_err(debug)?;
    while stream.next().await.is_some() {}
    match completion.await {
        Err(Error::Postgres(PgError::ValueConversionFailed(message))) => {
            Ok(format!("value-conversion-failed={message}"))
        }
        other => Err(format!("the query completed with {other:?}")),
    }
}

/// Commit `tx`, expecting the database error an aborted transaction reports.
async fn failed_commit(tx: Transaction) -> Result<String, String> {
    match Transaction::commit(tx).await {
        Err(Error::Postgres(PgError::QueryFailed(db))) => Ok(format!(
            "code={} detail={}",
            db.code,
            db.detail.unwrap_or_default()
        )),
        other => Err(format!("commit returned {other:?}")),
    }
}

/// Every row of an auto-committed `sql`, once its completion reports success.
async fn rows(sql: &str, params: Vec<Param>) -> Result<Vec<Vec<PgValue>>, String> {
    let (_columns, mut stream, completion) =
        store::query(sql.to_string(), params).await.map_err(debug)?;
    let mut rows = Vec::new();
    while let Some(row) = stream.next().await {
        rows.push(row);
    }
    completion.await.map_err(debug)?;
    Ok(rows)
}

async fn begin() -> Result<Transaction, String> {
    store::begin().await.map_err(debug)
}

async fn backend_pid() -> Result<i64, String> {
    integer(only(&rows("SELECT pg_backend_pid()", Vec::new()).await?)?)
}

fn sessions(before: i64, after: i64) -> Result<String, String> {
    if before == after {
        Ok("same-session".to_string())
    } else {
        Ok(format!("new-session: backend {before}, then {after}"))
    }
}

/// The only value of a one-row, one-column result.
fn only(rows: &[Vec<PgValue>]) -> Result<&PgValue, String> {
    match rows {
        [row] => match row.as_slice() {
            [value] => Ok(value),
            _ => Err(format!("expected one column, got {row:?}")),
        },
        _ => Err(format!("expected one row, got {rows:?}")),
    }
}

fn integer(value: &PgValue) -> Result<i64, String> {
    match value {
        PgValue::Int4(n) => Ok(i64::from(*n)),
        PgValue::Int8(n) => Ok(*n),
        other => Err(format!("expected an integer, got {other:?}")),
    }
}

fn text(value: &str) -> Param {
    Param::Value(PgValue::Text(value.to_string()))
}

/// The `table=` parameter, which must name one of the test's `betty_it_` tables.
fn table(query: &str) -> Result<String, String> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("table="))
        .filter(|name| {
            name.starts_with("betty_it_")
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
        .map(str::to_string)
        .ok_or_else(|| format!("expected ?table=betty_it_..., got {query:?}"))
}

fn debug(value: impl std::fmt::Debug) -> String {
    format!("{value:?}")
}

/// A response whose body is `body`, written once and closed.
fn respond(body: Vec<u8>) -> Response {
    let (mut tx, rx) = bindings::wit_stream::new::<u8>();
    let (trailers_tx, trailers_rx) = bindings::wit_future::new(|| todo!());
    wit_bindgen::spawn_local(async move {
        tx.write_all(body).await;
        drop(tx);
        let _ = trailers_tx.write(Ok(None)).await;
    });
    let (response, _result) = Response::new(Fields::new(), Some(rx), trailers_rx);
    response
}

bindings::export!(Component with_types_in bindings);
