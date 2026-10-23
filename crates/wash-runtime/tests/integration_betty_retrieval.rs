//! The betty-retrieval host plugin against a real Postgres and a real wasm
//! guest. The `retrieval-store-p3` fixture runs one check per request path
//! through `betty-blocks:retrieval@0.1.0` and answers with what it observed;
//! each test starts its own host, requests one path and asserts on the answer.
//!
//! The plugin is built around a `FakeEmbedder`, with a pool of one connection
//! and a five-second wait (see `common::retrieval`): the statements one request
//! runs share a Postgres session, and a connection the plugin fails to return
//! shows up as `pool-exhausted` inside the request. The one test that watches a
//! transaction from a second session gets two connections, and a workload
//! share of both.
//!
//! Every test but one is `#[ignore]`d, as each needs a database with pgvector:
//! `BETTY_RETRIEVAL_TEST_DATABASE_URL`, or Docker for a
//! `pgvector/pgvector:pg17` container. The one that runs by default is
//! `space_answers_while_the_database_is_down`, which needs no database. The
//! file compiles only once `cargo xtask build-fixtures` has staged the fixture.
#![cfg(feature = "betty-retrieval")]

use std::sync::Arc;

use anyhow::{Result, ensure};
use genius_embed::FakeEmbedder;

mod common;
use common::postgres::admin_client;
use common::req;
use common::retrieval::{
    DIMENSION, FreshDatabase, Gate, HOST_HEADER, ScratchTable, fake_embedder, fake_space,
    start_retrieval_workload, start_retrieval_workload_on, start_retrieval_workload_with,
    test_database,
};
use wash_runtime::plugin::betty_retrieval::BettyRetrievalConfig;

/// Start a host whose plugin embeds with `embedder` from a pool of one
/// connection, and return the fixture's answer to `path`, which must come back
/// 200.
async fn answer(database_url: &str, embedder: Arc<FakeEmbedder>, path: &str) -> Result<String> {
    let (addr, _host) = start_retrieval_workload(database_url, embedder, 1).await?;
    let (status, body) = req(&reqwest::Client::new(), &addr, HOST_HEADER, path).await?;
    ensure!(status.is_success(), "{path} answered {status}: {body}");
    Ok(body)
}

/// [`answer`] on the test database, with an embedder nothing else counts.
async fn ask(path: &str) -> Result<String> {
    let db = test_database().await?;
    answer(&db.url, fake_embedder(), path).await
}

/// `commit`'s answer for a transaction an earlier statement aborted: 25P02, the
/// plugin's fixed message, and that statement's failure as the detail.
fn aborted_commit(detail: &str) -> String {
    const MESSAGE: &str =
        "the transaction was aborted by an earlier failed statement; nothing was committed";
    format!("code=25P02 message={MESSAGE:?} detail={detail:?}")
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn embed_text_binds_a_vector_as_wide_as_the_embedder() -> Result<()> {
    assert_eq!(ask("/embed-text").await?, format!("dims={DIMENSION}"));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn embed_texts_keeps_input_order_and_embeds_a_repeated_text_once() -> Result<()> {
    let db = test_database().await?;
    let embedder = fake_embedder();
    let body = answer(&db.url, Arc::clone(&embedder), "/embed-texts").await?;
    assert_eq!(
        body, "order=alpha,beta,alpha,gamma",
        "each element of the array must be its own text's vector, in input order"
    );
    // One pass per distinct text in the list, plus one for each text bound on
    // its own: 3 + 3. Embedding the repeated alpha again would make 7.
    assert_eq!(embedder.calls(), 6);
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_null_value_binds_against_a_text_placeholder() -> Result<()> {
    assert_eq!(ask("/null").await?, "is-null=true");
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_committed_transaction_persists() -> Result<()> {
    let db = test_database().await?;
    let table = ScratchTable::create(&db.url, "commit").await?;
    let path = format!("/commit-persists?table={}", table.name);
    assert_eq!(answer(&db.url, fake_embedder(), &path).await?, "committed");
    // Read on the table's own connection: the plugin's one session would see
    // the transaction's row whether or not COMMIT was sent.
    assert_eq!(table.values().await?, ["kept"]);
    table.drop_table().await
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn dropping_a_transaction_rolls_it_back() -> Result<()> {
    let db = test_database().await?;
    let table = ScratchTable::create(&db.url, "rollback").await?;
    let path = format!("/drop-rolls-back?table={}", table.name);
    assert_eq!(
        answer(&db.url, fake_embedder(), &path).await?,
        "rows=0",
        "the next statement on the transaction's connection must not see its insert"
    );
    table.drop_table().await
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn commit_after_a_failed_statement_reports_25p02() -> Result<()> {
    assert_eq!(
        ask("/commit-after-failure").await?,
        aborted_commit("division by zero")
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn space_reports_the_plugins_embedding_space() -> Result<()> {
    let space = fake_space();
    assert_eq!(
        ask("/space").await?,
        format!(
            "space-id={} model-id={} dimension={}",
            space.space_id, space.model_id, space.dimension
        )
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_dropped_transaction_returns_its_connection_to_the_pool() -> Result<()> {
    assert_eq!(ask("/dropped-transaction").await?, "same-session");
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn an_abandoned_query_stream_closes_its_connection() -> Result<()> {
    // The server is still sending the rows nobody reads. Pooled, the connection
    // would hold its next borrower, any workload's, behind them.
    let answer = ask("/abandoned-stream").await?;
    assert!(answer.starts_with("new-session"), "{answer}");
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_cancelled_auto_commit_statement_closes_its_connection() -> Result<()> {
    // The server is still running the statement. Pooled, the connection would
    // hold its next borrower behind it.
    let answer = ask("/cancelled-auto-commit").await?;
    assert!(answer.starts_with("new-session"), "{answer}");
    Ok(())
}

/// Statements dropped mid-flight, one after another. Closing the connection
/// alone does not stop Postgres, which runs the statement on until it next
/// writes to the connection; until then its session sat outside the pool and
/// the workload's share, and a loop of cancels could fill `max_connections`.
/// Now the server is asked to cancel the statement, and the connection holds
/// its pool slot and its permit until the statement has ended.
#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_dropped_statement_has_ended_on_the_server_before_its_connection_is_replaced()
-> Result<()> {
    let db = test_database().await?;
    assert_eq!(
        answer(&db.url, fake_embedder(), "/cancel-loop").await?,
        "running=0,0,0,0,0,0",
        "after each dropped statement, no session may still be running one"
    );
    let still: i64 = admin_client(&db.url)
        .await?
        .query_one(
            "SELECT count(*) FROM pg_stat_activity WHERE state = 'active' \
             AND strpos(query, 'betty-it-cancel-loop') > 0 AND pid <> pg_backend_pid()",
            &[],
        )
        .await?
        .get(0);
    assert_eq!(
        still, 0,
        "a dropped statement is still running on the server"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_batch_that_fails_inside_its_own_transaction_does_not_break_the_next_statement()
-> Result<()> {
    assert_eq!(
        ask("/aborted-batch").await?,
        "batch=division by zero then=1",
        "the next borrower of the connection must not inherit the failed transaction block"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_transaction_a_batch_leaves_open_is_rolled_back_not_joined() -> Result<()> {
    let db = test_database().await?;
    let table = ScratchTable::create(&db.url, "left_open").await?;
    let path = format!("/batch-left-open?table={}", table.name);
    assert_eq!(answer(&db.url, fake_embedder(), &path).await?, "inserted");
    // Read on the table's own session: the auto-committed insert persisted, and
    // the one the batch left uncommitted did not.
    assert_eq!(table.values().await?, ["auto-commit"]);
    table.drop_table().await
}

/// [`answer`] on the test database, from a plugin whose config `configure`
/// changed last.
async fn ask_configured(configure: fn(&mut BettyRetrievalConfig), path: &str) -> Result<String> {
    let db = test_database().await?;
    let (addr, _host) =
        start_retrieval_workload_with(&db.url, fake_embedder(), 1, configure).await?;
    let (status, body) = req(&reqwest::Client::new(), &addr, HOST_HEADER, path).await?;
    ensure!(status.is_success(), "{path} answered {status}: {body}");
    Ok(body)
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_transaction_query_larger_than_the_cap_fails_and_keeps_the_transaction_from_committing()
-> Result<()> {
    let answer = ask_configured(
        |config| config.transaction_query_max_bytes = 1024,
        "/oversized-transaction-query",
    )
    .await?;
    let refused = "the query's rows are larger than 1024 bytes, the limit for one transaction \
                   query; select fewer rows, or read them in pieces";
    assert_eq!(
        answer,
        format!("query={refused} commit={}", aborted_commit(refused))
    );
    Ok(())
}

/// A row costs the host its values' bytes and the structures that hold them,
/// so rows of nothing but a NULL count too: the cap counted only the bytes,
/// and let any number of them through.
#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_transaction_query_of_null_rows_still_counts_against_the_cap() -> Result<()> {
    let answer = ask_configured(
        |config| config.transaction_query_max_bytes = 1024,
        "/null-rows-transaction-query",
    )
    .await?;
    let refused = "the query's rows are larger than 1024 bytes, the limit for one transaction \
                   query; select fewer rows, or read them in pieces";
    assert_eq!(
        answer,
        format!("query={refused} commit={}", aborted_commit(refused))
    );
    Ok(())
}

/// The host holds each row the server sends until the guest reads it. A row
/// over the most one row may be fails its query before the guest gets it,
/// and the next statement runs as usual.
#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_row_larger_than_one_row_may_be_fails_its_query() -> Result<()> {
    assert_eq!(
        ask("/oversized-row").await?,
        "rows=0 done=a row of the query is larger than 67108864 bytes, the most one row may \
         be; select less of it, or read it in pieces then=1"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_query_whose_rows_go_unread_is_ended_after_the_statement_time_limit() -> Result<()> {
    let answer = ask_configured(
        |config| config.statement_timeout = std::time::Duration::from_secs(1),
        "/unread-rows",
    )
    .await?;
    assert_eq!(
        answer,
        "done=no row of the query was read for 1s, so the host ended it; read the rows \
         before awaiting the result then=1"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_scalar_of_the_wrong_type_is_invalid_params_not_stored_wrong() -> Result<()> {
    let body = ask("/wrong-scalar-type").await?;
    assert!(
        body.starts_with("invalid-params=error serializing parameter 0: "),
        "{body}"
    );
    assert!(body.contains("the Postgres type `float8`"), "{body}");
    assert!(body.ends_with(" then=1"), "{body}");
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_host_whose_model_did_not_load_serves_sql_and_says_why_it_cannot_embed() -> Result<()> {
    let db = test_database().await?;
    let mut config = BettyRetrievalConfig::new(
        db.url.clone(),
        std::path::PathBuf::from("/nonexistent/models/missing.json"),
    );
    config.pool_size = 1;
    let plugin = wash_runtime::plugin::betty_retrieval::BettyRetrieval::new(config)?;
    let (addr, _host) = start_retrieval_workload_on(plugin).await?;
    let client = reqwest::Client::new();

    let (status, body) = req(&client, &addr, HOST_HEADER, "/null").await?;
    assert!(status.is_success(), "{status}: {body}");
    assert_eq!(
        body, "is-null=true",
        "a statement that embeds nothing still runs"
    );

    let (status, body) = req(&client, &addr, HOST_HEADER, "/embed-text").await?;
    assert!(status.is_success(), "{status}: {body}");
    assert!(body.starts_with("error: Error::Embed("), "{body}");
    assert!(body.contains("did not load"), "{body}");

    // `space` returns a record, not a result, so it cannot answer an error;
    // the guest's call traps instead of reporting a space that does not exist.
    let (status, body) = req(&client, &addr, HOST_HEADER, "/space").await?;
    assert!(!status.is_success(), "space must fail: {status}: {body}");
    Ok(())
}

/// The doc promises that statements work again once the database is back.
/// Terminating every session the plugin holds is what a database restart or
/// failover does to an established pool.
#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn statements_work_again_after_the_database_ends_every_pooled_session() -> Result<()> {
    let db = test_database().await?;
    let fresh = FreshDatabase::create(&db.url, "terminated").await?;
    let (addr, _host) = start_retrieval_workload(&fresh.url()?, fake_embedder(), 2).await?;
    let client = reqwest::Client::new();
    let (status, before) = req(&client, &addr, HOST_HEADER, "/embed-text").await?;
    ensure!(
        status.is_success(),
        "/embed-text answered {status}: {before}"
    );
    assert_eq!(before, format!("dims={DIMENSION}"));

    let ended = admin_client(&db.url)
        .await?
        .query(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = $1 AND pid <> pg_backend_pid()",
            &[&fresh.name()],
        )
        .await?;
    ensure!(!ended.is_empty(), "the plugin held no session to end");

    let (status, after) = req(&client, &addr, HOST_HEADER, "/embed-text").await?;
    ensure!(
        status.is_success(),
        "/embed-text answered {status}: {after}"
    );
    assert_eq!(
        after,
        format!("dims={DIMENSION}"),
        "the pool must replace the sessions the database ended"
    );
    fresh.drop_database().await
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_workload_cannot_hold_more_than_its_share_of_the_pool() -> Result<()> {
    // The pool has a second connection free, but this workload's share is one:
    // the rest is kept for the other applications on the host.
    let answer = ask_configured(
        |config| {
            config.pool_size = 2;
            config.connections_per_workload = Some(1);
        },
        "/second-begin",
    )
    .await?;
    assert_eq!(answer, "second-begin=pool-exhausted");
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_statement_past_the_statement_timeout_is_cancelled_by_the_server() -> Result<()> {
    let answer = ask_configured(
        |config| config.statement_timeout = std::time::Duration::from_secs(1),
        "/slow-statement",
    )
    .await?;
    // 57014 is query_canceled; the connection serves the next statement.
    assert_eq!(answer, "code=57014 then=1");
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_transaction_idle_past_the_idle_timeout_is_ended_by_the_server() -> Result<()> {
    let answer = ask_configured(
        |config| config.idle_in_transaction_timeout = std::time::Duration::from_secs(1),
        "/idle-transaction",
    )
    .await?;
    assert_eq!(answer, "idle-statement=failed then=1");
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn session_settings_a_batch_changes_do_not_reach_the_next_statement() -> Result<()> {
    assert_eq!(
        ask("/changed-session-settings").await?,
        r#"application_name="" search_path="public""#
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn begin_while_a_transaction_holds_the_only_connection_is_pool_exhausted() -> Result<()> {
    assert_eq!(ask("/second-begin").await?, "second-begin=pool-exhausted");
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_statement_cancelled_mid_flight_keeps_its_transaction_from_committing() -> Result<()> {
    assert_eq!(
        ask("/cancelled-statement").await?,
        aborted_commit("a statement was cancelled before it finished")
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_placeholder_count_mismatch_is_invalid_params() -> Result<()> {
    assert_eq!(
        ask("/placeholder-mismatch").await?,
        "invalid-params=statement expects 1 parameters but 0 were bound"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn an_array_bound_to_a_scalar_placeholder_is_invalid_params_not_a_panic() -> Result<()> {
    let body = ask("/array-for-scalar").await?;
    assert!(
        body.starts_with("invalid-params=error serializing parameter 0: "),
        "{body}"
    );
    assert!(body.contains("the Postgres type `int4`"), "{body}");
    // The pool's one connection served the next statement, so the refused one
    // did not panic the host mid-call.
    assert!(body.ends_with(" then=1"), "{body}");
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_batch_closes_the_portal_a_transactions_query_left_open() -> Result<()> {
    // A snapshot still held here is one `REINDEX INDEX CONCURRENTLY` on another
    // session waits out, for as long as the transaction stays open. The one
    // workload needs both connections: the transaction's and the watcher's.
    assert_eq!(
        ask_configured(
            |config| {
                config.pool_size = 2;
                config.connections_per_workload = Some(2);
            },
            "/batch-closes-portal",
        )
        .await?,
        "state=idle in transaction snapshot=released"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn execute_answers_the_rows_its_statement_affected() -> Result<()> {
    let db = test_database().await?;
    let table = ScratchTable::create(&db.url, "execute").await?;
    let path = format!("/execute-rows-affected?table={}", table.name);
    assert_eq!(
        answer(&db.url, fake_embedder(), &path).await?,
        "inserted=3 updated=2"
    );
    assert_eq!(table.values().await?, ["a", "b!", "c!"]);
    table.drop_table().await
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_large_transaction_query_does_not_hold_up_its_commit() -> Result<()> {
    assert_eq!(
        ask("/large-transaction-query").await?,
        "committed rows=5000 last=5000"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn an_undecodable_column_is_value_conversion_failed_naming_the_cause() -> Result<()> {
    let body = ask("/undecodable-column").await?;
    assert!(
        body.starts_with("value-conversion-failed=column 0: "),
        "{body}"
    );
    assert!(body.contains("unsupported type [vector]"), "{body}");
    Ok(())
}

/// `numeric` and `money` columns were read as text, so `1::numeric` came back
/// as `"\0\u{1}\0\0\0\0\0\0\0\u{1}"`. The money amount assumes the
/// server's `lc_monetary` has two places, as the test database's does.
#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn numeric_and_money_columns_decode_off_the_pool_and_in_a_transaction() -> Result<()> {
    assert_eq!(
        ask("/numeric-columns").await?,
        "one=1 scaled=123.45 sum=5050 money=12.34 list=[1.5,-2,NaN] bound=100000"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn path_and_polygon_columns_decode_off_the_pool_and_in_a_transaction() -> Result<()> {
    // A host that handed these to an array decoder panicked instead of
    // answering, on the pool's fetch task and inside a transaction's query.
    assert_eq!(
        ask("/geometric-columns").await?,
        "path=[(0,0),(1,0),(1,1)] open-path=[(0,0),(512,2.5)] polygon=[(0,0),(4,0),(4,-3)] \
         path-array=[[(0,0),(1,1)],[(5,5),(6,6),(7,7)]] \
         polygon-array=[[(0,0),(1,0),(1,1)],[(2,2),(3,2),(3,3),(2,3)]]"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn an_empty_list_binds_to_an_array_placeholder_and_unsafe_lists_stay_refused() -> Result<()> {
    assert_eq!(
        ask("/empty-lists").await?,
        "path-array=0 polygon-array=0 int2-vector-array=0 int4-array=0 text-array=0 \
         empty-for-scalar=invalid-params path-array-of-one=invalid-params then=1"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn an_lseg_column_decodes_and_a_line_column_is_refused_without_a_panic() -> Result<()> {
    let body = ask("/lseg-and-line-columns").await?;
    assert!(
        body.starts_with(
            "lseg=[(1,2),(3,4)] lseg-array=[[(512,0),(-1.5,1)],[(0,0),(1,1)]] \
             line=value-conversion-failed=column 0: "
        ),
        "{body}"
    );
    assert!(body.contains("line & line[] are not supported"), "{body}");
    assert!(body.ends_with(" then=1"), "{body}");
    Ok(())
}

/// A host comes up while its database is down, a statement then answers
/// `connection-failed`, and the same statement passes once the database is up,
/// with no host restart. The database starts without the vector extension, so
/// it is the plugin's first connection that created it.
#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_host_started_with_the_database_down_serves_it_once_it_is_up() -> Result<()> {
    let db = test_database().await?;
    let fresh = FreshDatabase::create(&db.url, "lazy").await?;
    let mut gate = Gate::closed()?;
    let (addr, _host) =
        start_retrieval_workload(&fresh.url_through(gate.addr())?, fake_embedder(), 1).await?;
    let client = reqwest::Client::new();

    let (status, down) = req(&client, &addr, HOST_HEADER, "/embed-text").await?;
    ensure!(status.is_success(), "/embed-text answered {status}: {down}");
    assert!(
        down.starts_with("error: Error::Postgres(Error::ConnectionFailed("),
        "with the database down a statement must be connection-failed, got {down}"
    );

    gate.open(fresh.server_address()?)?;
    let (status, up) = req(&client, &addr, HOST_HEADER, "/embed-text").await?;
    ensure!(status.is_success(), "/embed-text answered {status}: {up}");
    assert_eq!(up, format!("dims={DIMENSION}"));
    fresh.drop_database().await
}

/// The embedding space comes from the model, so a component can read it from a
/// host whose database never answers.
#[tokio::test]
async fn space_answers_while_the_database_is_down() -> Result<()> {
    let gate = Gate::closed()?;
    let url = format!("postgres://betty:betty@{}/never_connected", gate.addr());
    let (addr, _host) = start_retrieval_workload(&url, fake_embedder(), 1).await?;

    let (status, body) = req(&reqwest::Client::new(), &addr, HOST_HEADER, "/space").await?;
    ensure!(status.is_success(), "/space answered {status}: {body}");
    let space = fake_space();
    assert_eq!(
        body,
        format!(
            "space-id={} model-id={} dimension={}",
            space.space_id, space.model_id, space.dimension
        )
    );
    Ok(())
}

/// Hosts, or a host and a native provider, that start together on a NEW
/// database all find the extension missing and all try to create it. Without a
/// lock every session but one fails on `pg_extension_name_index`, because
/// `IF NOT EXISTS` does not see another session's uncommitted extension. Here
/// another session holds the plugin's lock with the extension created and not
/// yet committed; the plugin's first connection must wait for it and then find
/// the extension, not fail.
#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn the_first_connection_waits_for_another_session_creating_the_extension() -> Result<()> {
    let db = test_database().await?;
    let fresh = FreshDatabase::create(&db.url, "race").await?;

    let other = admin_client(&fresh.url()?).await?;
    other
        .batch_execute(
            "BEGIN; \
             SELECT pg_advisory_xact_lock(hashtext('genius-retrieval:create-extension-vector')); \
             CREATE EXTENSION vector;",
        )
        .await?;

    let (addr, _host) = start_retrieval_workload(&fresh.url()?, fake_embedder(), 1).await?;
    let statement = tokio::spawn(async move {
        req(&reqwest::Client::new(), &addr, HOST_HEADER, "/embed-text").await
    });

    // The plugin's session is in the new database and waits on a lock: the
    // other session's, whichever one that is.
    let watcher = admin_client(&db.url).await?;
    let mut waiting = false;
    for _ in 0..200 {
        let row = watcher
            .query_one(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = $1 AND wait_event_type = 'Lock'",
                &[&fresh.name()],
            )
            .await?;
        if row.get::<_, i64>(0) > 0 {
            waiting = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    ensure!(
        waiting,
        "the plugin's connection never waited on the other session"
    );

    other.batch_execute("COMMIT").await?;
    let (status, body) = statement.await??;
    ensure!(status.is_success(), "/embed-text answered {status}: {body}");
    assert_eq!(
        body,
        format!("dims={DIMENSION}"),
        "the plugin's first connection must wait for the other session, not fail beside it"
    );
    drop(other);
    fresh.drop_database().await
}

/// A session that takes the extension lock and never ends its transaction
/// must not hold a host's statements up for ever. The pool's own timeouts stop
/// at the connection: the wait for the lock happens after it, so the plugin
/// bounds that wait itself. The statement fails as `connection-failed`, and
/// one sent after the lock is free succeeds.
#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn a_first_connection_gives_up_on_an_extension_lock_never_released() -> Result<()> {
    let db = test_database().await?;
    let fresh = FreshDatabase::create(&db.url, "held").await?;

    let holder = admin_client(&fresh.url()?).await?;
    holder
        .batch_execute(
            "BEGIN; \
             SELECT pg_advisory_xact_lock(hashtext('genius-retrieval:create-extension-vector'));",
        )
        .await?;

    // `common::retrieval` gives the plugin a five-second connect timeout, and
    // `req` gives up after fifteen.
    let (addr, _host) = start_retrieval_workload(&fresh.url()?, fake_embedder(), 1).await?;
    let client = reqwest::Client::new();
    let (status, held) = req(&client, &addr, HOST_HEADER, "/embed-text").await?;
    ensure!(status.is_success(), "/embed-text answered {status}: {held}");
    assert!(
        held.starts_with("error: Error::Postgres(Error::ConnectionFailed("),
        "a lock that is never released must end as connection-failed, got {held}"
    );

    holder.batch_execute("ROLLBACK").await?;
    let (status, freed) = req(&client, &addr, HOST_HEADER, "/embed-text").await?;
    ensure!(
        status.is_success(),
        "/embed-text answered {status}: {freed}"
    );
    assert_eq!(freed, format!("dims={DIMENSION}"));
    fresh.drop_database().await
}
