//! The betty-retrieval host plugin against a real Postgres and a real wasm
//! guest. The `retrieval-store-p3` fixture runs one check per request path
//! through `betty-blocks:retrieval@0.1.0` and answers with what it observed;
//! each test starts its own host, requests one path and asserts on the answer.
//!
//! The plugin is built around a `FakeEmbedder`, with a pool of one connection
//! and a five-second wait (see `common::retrieval`): the statements one request
//! runs share a Postgres session, and a connection the plugin fails to return
//! shows up as `pool-exhausted` inside the request. The one test that watches a
//! transaction from a second session gets two connections.
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
    start_retrieval_workload, test_database,
};

/// Start a host whose plugin embeds with `embedder` from a pool of `pool_size`
/// connections, and return the fixture's answer to `path`, which must come
/// back 200.
async fn answer_from_pool(
    database_url: &str,
    embedder: Arc<FakeEmbedder>,
    pool_size: usize,
    path: &str,
) -> Result<String> {
    let (addr, _host) = start_retrieval_workload(database_url, embedder, pool_size).await?;
    let (status, body) = req(&reqwest::Client::new(), &addr, HOST_HEADER, path).await?;
    ensure!(status.is_success(), "{path} answered {status}: {body}");
    Ok(body)
}

/// [`answer_from_pool`] with one connection.
async fn answer(database_url: &str, embedder: Arc<FakeEmbedder>, path: &str) -> Result<String> {
    answer_from_pool(database_url, embedder, 1, path).await
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
async fn an_abandoned_query_stream_returns_its_connection_to_the_pool() -> Result<()> {
    assert_eq!(ask("/abandoned-stream").await?, "same-session");
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
    let db = test_database().await?;
    // A snapshot still held here is one `REINDEX INDEX CONCURRENTLY` on another
    // session waits out, for as long as the transaction stays open.
    assert_eq!(
        answer_from_pool(&db.url, fake_embedder(), 2, "/batch-closes-portal").await?,
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
