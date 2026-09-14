//! The betty-retrieval host plugin against a real Postgres and a real wasm
//! guest. The `retrieval-store-p3` fixture runs one check per request path
//! through `betty-blocks:retrieval@0.1.0` and answers with what it observed;
//! each test starts its own host, requests one path and asserts on the answer.
//!
//! The plugin is built around a `FakeEmbedder`, with a pool of one connection
//! and a two-second wait (see `common::retrieval`): the statements one request
//! runs share a Postgres session, and a connection the plugin fails to return
//! shows up as `pool-exhausted` inside the request.
//!
//! Every test is `#[ignore]`d, as each needs a database with pgvector:
//! `BETTY_RETRIEVAL_TEST_DATABASE_URL`, or Docker for a
//! `pgvector/pgvector:pg17` container. The file compiles only once
//! `cargo xtask build-fixtures` has staged the fixture.
#![cfg(feature = "betty-retrieval")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use anyhow::{Result, ensure};
use genius_embed::FakeEmbedder;

mod common;
use common::req;
use common::retrieval::{
    DIMENSION, HOST_HEADER, ScratchTable, fake_embedder, fake_space, start_retrieval_workload,
    test_database,
};

/// Start a host whose plugin embeds with `embedder`, and return the fixture's
/// answer to `path`, which must come back 200.
async fn answer(database_url: &str, embedder: Arc<FakeEmbedder>, path: &str) -> Result<String> {
    let (addr, _host) = start_retrieval_workload(database_url, embedder).await?;
    let (status, body) = req(&reqwest::Client::new(), &addr, HOST_HEADER, path).await?;
    ensure!(status.is_success(), "{path} answered {status}: {body}");
    Ok(body)
}

/// [`answer`] on the test database, with an embedder nothing else counts.
async fn ask(path: &str) -> Result<String> {
    let db = test_database().await?;
    answer(&db.url, fake_embedder(), path).await
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
    let outcome = async {
        let path = format!("/commit-persists?table={}", table.name);
        let body = answer(&db.url, fake_embedder(), &path).await?;
        // Read on the table's own connection: the plugin's one session would
        // see the transaction's row whether or not COMMIT was sent.
        anyhow::Ok((body, table.values().await?))
    }
    .await;
    table.drop_table().await?;
    let (body, values) = outcome?;
    assert_eq!(body, "committed");
    assert_eq!(values, ["kept"]);
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn dropping_a_transaction_rolls_it_back() -> Result<()> {
    let db = test_database().await?;
    let table = ScratchTable::create(&db.url, "rollback").await?;
    let path = format!("/drop-rolls-back?table={}", table.name);
    let body = answer(&db.url, fake_embedder(), &path).await;
    table.drop_table().await?;
    assert_eq!(
        body?, "rows=0",
        "the next statement on the transaction's connection must not see its insert"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pgvector database; run with `-- --ignored`"]
async fn commit_after_a_failed_statement_reports_25p02() -> Result<()> {
    assert_eq!(
        ask("/commit-after-failure").await?,
        "code=25P02 detail=division by zero"
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
        "code=25P02 detail=a statement was cancelled before it finished"
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
async fn an_undecodable_column_is_value_conversion_failed_naming_the_cause() -> Result<()> {
    let body = ask("/undecodable-column").await?;
    assert!(
        body.starts_with("value-conversion-failed=column 0: "),
        "{body}"
    );
    assert!(body.contains("unsupported type [vector]"), "{body}");
    Ok(())
}
