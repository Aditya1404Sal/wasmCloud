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
//!   - `/array-for-scalar` — an `int4-array` value bound to a scalar
//!     `$1::int4`, then the next statement on the same pool.
//!   - `/undecodable-column` — a column no `pg-value` can hold.
//!   - `/batch-closes-portal` — another session's view of a transaction's
//!     snapshot after a query and then a `batch` on it.
//!   - `/execute-rows-affected?table=` — what `execute` answers for an insert
//!     of three rows and then an update of two.
//!   - `/large-transaction-query` — a transaction's query of 5000 rows,
//!     committed before a row is read, then read.
//!   - `/geometric-columns` — `path`, `polygon`, `path[]` and `polygon[]`
//!     columns, read off the pool and again in a transaction.
//!   - `/empty-lists` — empty lists bound to array placeholders, then two
//!     lists the host must still refuse.
//!   - `/lseg-and-line-columns` — an `lseg` and an `lseg[]` column, then a
//!     `line` column, which no `pg-value` can hold, then the next statement.
//!   - `/aborted-batch` — a `batch` that fails after its own `BEGIN`, then the
//!     next statement on the pool.
//!   - `/batch-left-open?table=` — a `batch` that opens a transaction and
//!     inserts, then an auto-committed insert; the test reads which persisted.
//!   - `/changed-session-settings` — a `batch` of `SET`s, then the settings the
//!     next statement on the pool runs under.
//!   - `/cancelled-auto-commit` — which session serves the statement after an
//!     auto-committed statement dropped mid-flight.
//!   - `/slow-statement` — a three-second statement, then the next statement
//!     on the pool. The test sets a shorter statement timeout.
//!   - `/idle-transaction` — a statement on a transaction left idle for two
//!     seconds, then the next statement on the pool. The test sets a shorter
//!     idle timeout.
//!   - `/wrong-scalar-type` — an `int8` bound to a `float8` placeholder, then
//!     the next statement on the pool.
//!   - `/oversized-transaction-query` — a transaction's query of about 10 KB,
//!     then its `commit`. The test sets a 1 KiB cap.
//!   - `/unread-rows` — a query's completion awaited before its rows are read,
//!     then the next statement on the pool. The test sets a 1 s time limit.
//!   - `/cancel-loop` — statements that sleep, each dropped mid-flight (a
//!     cancelled `execute`, or a query stream dropped after its first row),
//!     and after each one how many sessions still run such a statement.
//!   - `/null-rows-transaction-query` — a transaction's query of 100 rows that
//!     hold only a NULL, then its `commit`. The test sets a 1 KiB cap.
//!   - `/oversized-row` — a query whose one row holds 64 MiB, then the next
//!     statement on the pool.
//!
//! The test runs the plugin with a pool of one connection, so the statements a
//! route runs share one Postgres session unless the plugin replaced it; two for
//! `/batch-closes-portal`, whose transaction a second session watches.

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
        "/array-for-scalar" => array_for_scalar().await,
        "/undecodable-column" => undecodable_column().await,
        "/batch-closes-portal" => batch_closes_portal().await,
        "/execute-rows-affected" => execute_rows_affected(&table(query)?).await,
        "/large-transaction-query" => large_transaction_query().await,
        "/geometric-columns" => geometric_columns().await,
        "/empty-lists" => empty_lists().await,
        "/lseg-and-line-columns" => lseg_and_line_columns().await,
        "/aborted-batch" => aborted_batch().await,
        "/batch-left-open" => batch_left_open(&table(query)?).await,
        "/changed-session-settings" => changed_session_settings().await,
        "/cancelled-auto-commit" => cancelled_auto_commit().await,
        "/slow-statement" => slow_statement().await,
        "/idle-transaction" => idle_transaction().await,
        "/wrong-scalar-type" => wrong_scalar_type().await,
        "/oversized-transaction-query" => oversized_transaction_query().await,
        "/unread-rows" => unread_rows().await,
        "/cancel-loop" => cancel_loop().await,
        "/null-rows-transaction-query" => null_rows_transaction_query().await,
        "/oversized-row" => oversized_row().await,
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

async fn array_for_scalar() -> Result<String, String> {
    let refused = match store::execute(
        "SELECT $1::int4".to_string(),
        vec![Param::Value(PgValue::Int4Array(vec![1, 2]))],
    )
    .await
    {
        Err(Error::Postgres(PgError::InvalidParams(message))) => message,
        other => return Err(format!("the statement returned {other:?}")),
    };
    let after = integer(only(&rows("SELECT 1", Vec::new()).await?)?)?;
    Ok(format!("invalid-params={refused} then={after}"))
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

/// The query runs on the extended protocol, whose portal outlives it with its
/// snapshot; a `batch` runs on the simple protocol, which closes that portal.
async fn batch_closes_portal() -> Result<String, String> {
    let tx = begin().await?;
    let pid = integer(only(
        &transaction_rows(&tx, "SELECT pg_backend_pid()").await?,
    )?)?;
    tx.batch("SELECT 1".to_string()).await.map_err(debug)?;
    let observed = rows(
        "SELECT state, backend_xmin IS NULL FROM pg_stat_activity WHERE pid = $1::int8",
        vec![Param::Value(PgValue::Int8(pid))],
    )
    .await?;
    Transaction::commit(tx).await.map_err(debug)?;
    match observed.as_slice() {
        [row] => match row.as_slice() {
            [PgValue::Text(state), PgValue::Bool(released)] => {
                let snapshot = if *released { "released" } else { "held" };
                Ok(format!("state={state} snapshot={snapshot}"))
            }
            _ => Err(format!("expected a state and a bool, got {row:?}")),
        },
        _ => Err(format!(
            "expected one pg_stat_activity row, got {observed:?}"
        )),
    }
}

async fn execute_rows_affected(table: &str) -> Result<String, String> {
    let inserted = store::execute(
        format!("INSERT INTO {table} (v) VALUES ($1), ($2), ($3)"),
        vec![text("a"), text("b"), text("c")],
    )
    .await
    .map_err(debug)?;
    let updated = store::execute(
        format!("UPDATE {table} SET v = v || '!' WHERE v <> $1"),
        vec![text("a")],
    )
    .await
    .map_err(debug)?;
    Ok(format!("inserted={inserted} updated={updated}"))
}

/// More rows than any buffer between the statement and the guest holds.
const LARGE_QUERY_ROWS: u32 = 5000;

/// How long the commit after that query may take.
const COMMIT_WITHIN_NS: u64 = 5_000_000_000;

/// Committed before a row is read: rows still coming off the transaction's
/// connection would keep the commit waiting behind them.
async fn large_transaction_query() -> Result<String, String> {
    let tx = begin().await?;
    let (_columns, mut stream, completion) = tx
        .query(
            format!("SELECT g FROM generate_series(1, {LARGE_QUERY_ROWS}) AS g"),
            Vec::new(),
        )
        .await
        .map_err(debug)?;
    let commit = Box::pin(Transaction::commit(tx));
    let timer = Box::pin(monotonic_clock::wait_for(COMMIT_WITHIN_NS));
    match select(commit, timer).await {
        Either::Left((committed, _)) => committed.map_err(debug)?,
        Either::Right(_) => {
            return Err(format!("the commit took over {COMMIT_WITHIN_NS} ns"));
        }
    }
    let (mut count, mut last) = (0, 0);
    while let Some(row) = stream.next().await {
        count += 1;
        last = integer(row.first().ok_or("a row has no columns")?)?;
    }
    completion.await.map_err(debug)?;
    Ok(format!("committed rows={count} last={last}"))
}

/// A closed path, an open one, a polygon, and an array of each. 512 is among
/// the coordinates: its first bytes read as a point count are `i32::MIN`.
const GEOMETRIC_SQL: &str = "SELECT '((0,0),(1,0),(1,1))'::path, \
            '[(0,0),(512,2.5)]'::path, \
            '((0,0),(4,0),(4,-3))'::polygon, \
            ARRAY['((0,0),(1,1))'::path, '[(5,5),(6,6),(7,7)]'::path], \
            ARRAY['((0,0),(1,0),(1,1))'::polygon, '((2,2),(3,2),(3,3),(2,3))'::polygon]";

/// What each geometric column decoded to, read off the pool and then inside a
/// transaction: the two paths decode rows in different places on the host.
async fn geometric_columns() -> Result<String, String> {
    let pooled = geometric_row(&rows(GEOMETRIC_SQL, Vec::new()).await?)?;
    let tx = begin().await?;
    let in_transaction = geometric_row(&transaction_rows(&tx, GEOMETRIC_SQL).await?)?;
    Transaction::commit(tx).await.map_err(debug)?;
    if pooled != in_transaction {
        return Err(format!(
            "the pool read {pooled}, the transaction {in_transaction}"
        ));
    }
    Ok(pooled)
}

fn geometric_row(rows: &[Vec<PgValue>]) -> Result<String, String> {
    match rows {
        [row] => match row.as_slice() {
            [PgValue::Path(closed), PgValue::Path(open), PgValue::Polygon(polygon), PgValue::PathArray(paths), PgValue::PolygonArray(polygons)] => {
                Ok(format!(
                    "path={} open-path={} polygon={} path-array={} polygon-array={}",
                    points(closed),
                    points(open),
                    points(polygon),
                    point_lists(paths),
                    point_lists(polygons)
                ))
            }
            _ => Err(format!(
                "expected two paths, a polygon and two arrays, got {row:?}"
            )),
        },
        _ => Err(format!("expected one row, got {rows:?}")),
    }
}

type Float = (u64, i16, i8);

/// The `f64` a `hashable-f64` (mantissa, exponent, sign) stands for.
fn float((mantissa, exponent, sign): Float) -> f64 {
    f64::from(sign) * (mantissa as f64) * 2f64.powi(i32::from(exponent))
}

fn points(points: &[(Float, Float)]) -> String {
    let points: Vec<String> = points
        .iter()
        .map(|(x, y)| format!("({},{})", float(*x), float(*y)))
        .collect();
    format!("[{}]", points.join(","))
}

fn point_lists(lists: &[Vec<(Float, Float)>]) -> String {
    let lists: Vec<String> = lists.iter().map(|list| points(list)).collect();
    format!("[{}]", lists.join(","))
}

/// An empty list is an empty array whatever its members would have been, so
/// the host can bind it wherever the placeholder is an array. It must still
/// refuse an empty list for a scalar, and a `path[]` that has a path in it.
async fn empty_lists() -> Result<String, String> {
    let mut answer = Vec::new();
    for (name, cast, value) in [
        ("path-array", "path[]", PgValue::PathArray(Vec::new())),
        (
            "polygon-array",
            "polygon[]",
            PgValue::PolygonArray(Vec::new()),
        ),
        (
            "int2-vector-array",
            "int2[]",
            PgValue::Int2VectorArray(Vec::new()),
        ),
        ("int4-array", "int4[]", PgValue::Int4Array(Vec::new())),
        ("text-array", "text[]", PgValue::TextArray(Vec::new())),
    ] {
        let rows = rows(
            &format!("SELECT cardinality($1::{cast})"),
            vec![Param::Value(value)],
        )
        .await
        .map_err(|e| format!("{name}: {e}"))?;
        answer.push(format!("{name}={}", integer(only(&rows)?)?));
    }
    let point = ((0, 0, 1), (0, 0, 1));
    for (name, cast, value) in [
        ("empty-for-scalar", "int4", PgValue::Int4Array(Vec::new())),
        (
            "path-array-of-one",
            "path[]",
            PgValue::PathArray(vec![vec![point]]),
        ),
    ] {
        match store::execute(format!("SELECT $1::{cast}"), vec![Param::Value(value)]).await {
            Err(Error::Postgres(PgError::InvalidParams(_))) => {
                answer.push(format!("{name}=invalid-params"));
            }
            other => return Err(format!("{name}: the statement returned {other:?}")),
        }
    }
    // The pool's one connection serves this, so nothing above panicked the host.
    let after = integer(only(&rows("SELECT 1", Vec::new()).await?)?)?;
    answer.push(format!("then={after}"));
    Ok(answer.join(" "))
}

/// What an `lseg` and an `lseg[]` column decoded to, then how a `line` column
/// failed. 512 is among the coordinates for the same reason as above.
async fn lseg_and_line_columns() -> Result<String, String> {
    let decoded = rows(
        "SELECT '[(1,2),(3,4)]'::lseg, ARRAY['[(512,0),(-1.5,1)]'::lseg, '[(0,0),(1,1)]'::lseg]",
        Vec::new(),
    )
    .await?;
    let segments = match decoded.as_slice() {
        [row] => match row.as_slice() {
            [PgValue::Lseg(one), PgValue::LsegArray(many)] => {
                let many: Vec<String> = many.iter().map(segment).collect();
                format!("lseg={} lseg-array=[{}]", segment(one), many.join(","))
            }
            _ => return Err(format!("expected an lseg and an array, got {row:?}")),
        },
        _ => return Err(format!("expected one row, got {decoded:?}")),
    };
    let (_columns, mut stream, completion) =
        store::query("SELECT '{1,-1,0}'::line".to_string(), Vec::new())
            .await
            .map_err(debug)?;
    while stream.next().await.is_some() {}
    let line = match completion.await {
        Err(Error::Postgres(PgError::ValueConversionFailed(message))) => message,
        other => return Err(format!("the line query completed with {other:?}")),
    };
    // The pool's one connection serves this, so the line did not panic the host.
    let after = integer(only(&rows("SELECT 1", Vec::new()).await?)?)?;
    Ok(format!(
        "{segments} line=value-conversion-failed={line} then={after}"
    ))
}

fn segment(((start_x, start_y), (end_x, end_y)): &((Float, Float), (Float, Float))) -> String {
    format!(
        "[({},{}),({},{})]",
        float(*start_x),
        float(*start_y),
        float(*end_x),
        float(*end_y)
    )
}

/// The batch's error stops Postgres before its `COMMIT`, which leaves the
/// session inside a failed transaction block.
async fn aborted_batch() -> Result<String, String> {
    let failed = match store::batch("BEGIN; SELECT 1/0; COMMIT;".to_string()).await {
        Err(Error::Postgres(PgError::QueryFailed(db))) => db.message,
        other => return Err(format!("the batch returned {other:?}")),
    };
    let after = integer(only(&rows("SELECT 1", Vec::new()).await?)?)?;
    Ok(format!("batch={failed} then={after}"))
}

/// The batch's transaction is never committed. An insert after it that joined
/// it would be lost with it.
async fn batch_left_open(table: &str) -> Result<String, String> {
    store::batch(format!(
        "BEGIN; INSERT INTO {table} (v) VALUES ('left-open');"
    ))
    .await
    .map_err(debug)?;
    store::execute(
        format!("INSERT INTO {table} (v) VALUES ($1)"),
        vec![text("auto-commit")],
    )
    .await
    .map_err(debug)?;
    Ok("inserted".to_string())
}

async fn changed_session_settings() -> Result<String, String> {
    store::batch("SET application_name = 'leaked'; SET search_path = pg_catalog;".to_string())
        .await
        .map_err(debug)?;
    let rows = rows(
        "SELECT current_setting('application_name'), current_setting('search_path')",
        Vec::new(),
    )
    .await?;
    match rows.as_slice() {
        [row] => match row.as_slice() {
            [PgValue::Text(application), PgValue::Text(search_path)] => Ok(format!(
                "application_name={application:?} search_path={search_path:?}"
            )),
            _ => Err(format!("expected two texts, got {row:?}")),
        },
        _ => Err(format!("expected one row, got {rows:?}")),
    }
}

async fn cancelled_auto_commit() -> Result<String, String> {
    let before = backend_pid().await?;
    let statement = Box::pin(store::execute("SELECT pg_sleep(2)".to_string(), Vec::new()));
    let timer = Box::pin(monotonic_clock::wait_for(CANCEL_AFTER_NS));
    // As in `cancelled_statement`: the losing call is dropped still in flight.
    if let Either::Left((finished, _)) = select(statement, timer).await {
        return Err(format!(
            "pg_sleep finished before the timer fired: {finished:?}"
        ));
    }
    sessions(before, backend_pid().await?)
}

async fn slow_statement() -> Result<String, String> {
    let code = match store::execute("SELECT pg_sleep(3)".to_string(), Vec::new()).await {
        Err(Error::Postgres(PgError::QueryFailed(db))) => db.code,
        other => return Err(format!("the statement returned {other:?}")),
    };
    let after = integer(only(&rows("SELECT 1", Vec::new()).await?)?)?;
    Ok(format!("code={code} then={after}"))
}

/// The server ends a session that sits in a transaction past the idle
/// timeout, which releases whatever the transaction held.
async fn idle_transaction() -> Result<String, String> {
    let tx = begin().await?;
    tx.execute("SELECT 1".to_string(), Vec::new())
        .await
        .map_err(debug)?;
    monotonic_clock::wait_for(2_000_000_000).await;
    let late = tx.execute("SELECT 1".to_string(), Vec::new()).await;
    drop(tx);
    if late.is_ok() {
        return Err("the idle transaction still ran a statement".to_string());
    }
    let after = integer(only(&rows("SELECT 1", Vec::new()).await?)?)?;
    Ok(format!("idle-statement=failed then={after}"))
}

/// Written as it is, the eight bytes of `int8(1)` would be stored as the
/// float8 4.9e-324.
async fn wrong_scalar_type() -> Result<String, String> {
    let refused = match store::execute(
        "SELECT $1::float8".to_string(),
        vec![Param::Value(PgValue::Int8(1))],
    )
    .await
    {
        Err(Error::Postgres(PgError::InvalidParams(message))) => message,
        other => return Err(format!("the statement returned {other:?}")),
    };
    let after = integer(only(&rows("SELECT 1", Vec::new()).await?)?)?;
    Ok(format!("invalid-params={refused} then={after}"))
}

async fn oversized_transaction_query() -> Result<String, String> {
    let tx = begin().await?;
    let refused = match tx
        .query(
            "SELECT repeat('x', 100) FROM generate_series(1, 100)".to_string(),
            Vec::new(),
        )
        .await
    {
        Err(Error::Postgres(PgError::Other(message))) => message,
        Err(other) => return Err(format!("the query failed with {other:?}")),
        Ok(_) => return Err("the query answered its rows".to_string()),
    };
    Ok(format!(
        "query={refused} commit={}",
        failed_commit(tx).await?
    ))
}

/// Rows of nothing but a NULL bring no value bytes, which a cap that counted
/// only those let through however many there were.
async fn null_rows_transaction_query() -> Result<String, String> {
    let tx = begin().await?;
    let refused = match tx
        .query(
            "SELECT NULL FROM generate_series(1, 100)".to_string(),
            Vec::new(),
        )
        .await
    {
        Err(Error::Postgres(PgError::Other(message))) => message,
        Err(other) => return Err(format!("the query failed with {other:?}")),
        Ok(_) => return Err("the query answered its rows".to_string()),
    };
    Ok(format!(
        "query={refused} commit={}",
        failed_commit(tx).await?
    ))
}

/// One row of 64 MiB: the host would hold it, and as many more as wait for
/// the guest, before the guest read any.
async fn oversized_row() -> Result<String, String> {
    let (_columns, mut stream, completion) = store::query(
        "SELECT repeat('x', 64 * 1024 * 1024)".to_string(),
        Vec::new(),
    )
    .await
    .map_err(debug)?;
    let mut delivered = 0;
    while stream.next().await.is_some() {
        delivered += 1;
    }
    let outcome = match completion.await {
        Err(Error::Postgres(PgError::Other(message))) => message,
        other => {
            return Err(format!(
                "the completion answered {other:?} after {delivered} rows"
            ))
        }
    };
    let after = integer(only(&rows("SELECT 1", Vec::new()).await?)?)?;
    Ok(format!("rows={delivered} done={outcome} then={after}"))
}

/// Awaits the completion first: the rows past the stream's buffer then wait
/// for a reader that never comes.
async fn unread_rows() -> Result<String, String> {
    let (_columns, stream, completion) = store::query(
        "SELECT g FROM generate_series(1, 5000) AS g".to_string(),
        Vec::new(),
    )
    .await
    .map_err(debug)?;
    let outcome = match completion.await {
        Err(Error::Postgres(PgError::Other(message))) => message,
        other => return Err(format!("the completion answered {other:?}")),
    };
    drop(stream);
    let after = integer(only(&rows("SELECT 1", Vec::new()).await?)?)?;
    Ok(format!("done={outcome} then={after}"))
}

/// Marks the statements `/cancel-loop` drops, so that sessions still running
/// one can be counted.
const CANCEL_LOOP_MARKER: &str = "betty-it-cancel-loop";

/// How many statements `/cancel-loop` drops.
const CANCEL_LOOP_ROUNDS: usize = 6;

/// Each round drops a statement that would run for half a minute: an
/// `execute` cancelled once the timer fires, or, every other round, a query
/// stream dropped after its first row while the server still has rows to
/// make. Then it counts the sessions that are still running such a
/// statement: a dropped statement the server goes on running holds a session
/// outside the pool and the workload's share.
async fn cancel_loop() -> Result<String, String> {
    let mut running = Vec::new();
    for round in 0..CANCEL_LOOP_ROUNDS {
        if round % 2 == 0 {
            let statement = Box::pin(store::execute(
                format!("SELECT pg_sleep(30) /* {CANCEL_LOOP_MARKER} */"),
                Vec::new(),
            ));
            let timer = Box::pin(monotonic_clock::wait_for(CANCEL_AFTER_NS));
            if let Either::Left((finished, _)) = select(statement, timer).await {
                return Err(format!(
                    "pg_sleep finished before the timer fired: {finished:?}"
                ));
            }
        } else {
            let (_columns, mut stream, completion) = store::query(
                // Each row fills Postgres's 8 KB send buffer, so it is sent as
                // soon as it is made rather than with the next 8 KB.
                format!(
                    "SELECT repeat('x', 8192) || pg_sleep(0.05)::text \
                     FROM generate_series(1, 600) /* {CANCEL_LOOP_MARKER} */"
                ),
                Vec::new(),
            )
            .await
            .map_err(debug)?;
            stream.next().await.ok_or("the query streamed no rows")?;
            drop(stream);
            drop(completion);
        }
        let still = rows(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE state = 'active' AND strpos(query, $1) > 0 AND pid <> pg_backend_pid()",
            vec![text(CANCEL_LOOP_MARKER)],
        )
        .await?;
        running.push(integer(only(&still)?)?.to_string());
    }
    Ok(format!("running={}", running.join(",")))
}

/// Commit `tx`, expecting the database error an aborted transaction reports.
async fn failed_commit(tx: Transaction) -> Result<String, String> {
    match Transaction::commit(tx).await {
        Err(Error::Postgres(PgError::QueryFailed(db))) => Ok(format!(
            "code={} message={:?} detail={:?}",
            db.code,
            db.message,
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

/// Every row of `sql` run on `tx`, once its completion reports success.
async fn transaction_rows(tx: &Transaction, sql: &str) -> Result<Vec<Vec<PgValue>>, String> {
    let (_columns, mut stream, completion) =
        tx.query(sql.to_string(), Vec::new()).await.map_err(debug)?;
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

/// The `table=` parameter: one of the test's tables, schema-qualified as
/// `public.betty_it_...`, so it resolves the same under any `search_path`.
fn table(query: &str) -> Result<String, String> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("table="))
        .filter(|name| {
            name.strip_prefix("public.betty_it_").is_some_and(|rest| {
                !rest.is_empty()
                    && rest
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
        })
        .map(str::to_string)
        .ok_or_else(|| format!("expected ?table=public.betty_it_..., got {query:?}"))
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
