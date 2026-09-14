//! Configuration for the betty-retrieval host plugin: where the database and
//! the embedding model are, and how the connection pool and HNSW search are
//! tuned. Defaults match the native provider's.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, bail};
use url::Url;

/// How the plugin connects to Postgres, tunes HNSW search, and finds the
/// granite embedding model on disk.
#[derive(Clone, Debug)]
pub struct BettyRetrievalConfig {
    pub database_url: String,
    pub model_config: PathBuf,
    pub pool_size: usize,
    pub connect_timeout: Duration,
    pub ef_search: u32,
    pub max_scan_tuples: u32,
    pub embed_threads: usize,
}

impl BettyRetrievalConfig {
    /// `database_url` and `model_config` with the native provider's defaults
    /// for everything else: pool 8, 10s connect timeout, `ef_search` 200,
    /// `max_scan_tuples` 20000, 4 embed threads.
    pub fn new(database_url: String, model_config: PathBuf) -> Self {
        Self {
            database_url,
            model_config,
            pool_size: 8,
            connect_timeout: Duration::from_secs(10),
            ef_search: 200,
            max_scan_tuples: 20_000,
            embed_threads: 4,
        }
    }

    /// Refuses a zero pool size, connect timeout, embed thread count or
    /// `ef_search`, and a `database_url` that is not `postgres`/`postgresql`.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.pool_size == 0 {
            bail!("pool_size must be greater than zero");
        }
        if self.connect_timeout.is_zero() {
            bail!("connect_timeout must be greater than zero");
        }
        if self.embed_threads == 0 {
            bail!("embed_threads must be greater than zero");
        }
        if self.ef_search == 0 {
            bail!("ef_search must be greater than zero");
        }
        let url = Url::parse(&self.database_url)
            .with_context(|| format!("database_url {:?} is not a valid url", self.database_url))?;
        if !matches!(url.scheme(), "postgres" | "postgresql") {
            bail!(
                "database_url must use the postgres or postgresql scheme, got {:?}",
                url.scheme()
            );
        }
        Ok(())
    }

    /// The session GUCs the native provider sets on every pooled connection
    /// (`context-provider/src/store.rs:750-760`), with this config's
    /// `ef_search` and `max_scan_tuples` substituted.
    pub(crate) fn session_setup_sql(&self) -> String {
        let ef_search = self.ef_search;
        let max_scan_tuples = self.max_scan_tuples;
        format!(
            "SELECT '[1]'::vector;\n\
             SET search_path = public;\n\
             SET hnsw.ef_search = {ef_search};\n\
             SET hnsw.iterative_scan = 'strict_order';\n\
             SET hnsw.max_scan_tuples = {max_scan_tuples};"
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn config() -> BettyRetrievalConfig {
        BettyRetrievalConfig::new(
            "postgres://genius:genius@127.0.0.1:55433/genius_retrieval".to_string(),
            PathBuf::from("models/granite-embedding-107m-multilingual.json"),
        )
    }

    #[test]
    fn new_fills_in_the_native_providers_defaults() {
        let cfg = config();
        assert_eq!(cfg.pool_size, 8);
        assert_eq!(cfg.connect_timeout, Duration::from_secs(10));
        assert_eq!(cfg.ef_search, 200);
        assert_eq!(cfg.max_scan_tuples, 20_000);
        assert_eq!(cfg.embed_threads, 4);
    }

    #[test]
    fn validate_accepts_the_defaults() {
        config().validate().expect("the defaults are valid");
    }

    #[test]
    fn validate_refuses_a_zero_pool_size() {
        let mut cfg = config();
        cfg.pool_size = 0;
        let err = cfg
            .validate()
            .expect_err("a zero pool size must be refused")
            .to_string();
        assert!(err.contains("pool_size"), "{err}");
    }

    #[test]
    fn validate_refuses_a_zero_connect_timeout() {
        let mut cfg = config();
        cfg.connect_timeout = Duration::ZERO;
        let err = cfg
            .validate()
            .expect_err("a zero connect timeout must be refused")
            .to_string();
        assert!(err.contains("connect_timeout"), "{err}");
    }

    #[test]
    fn validate_refuses_a_zero_embed_thread_count() {
        let mut cfg = config();
        cfg.embed_threads = 0;
        let err = cfg
            .validate()
            .expect_err("zero embed threads must be refused")
            .to_string();
        assert!(err.contains("embed_threads"), "{err}");
    }

    #[test]
    fn validate_refuses_a_zero_ef_search() {
        let mut cfg = config();
        cfg.ef_search = 0;
        let err = cfg
            .validate()
            .expect_err("a zero ef_search must be refused")
            .to_string();
        assert!(err.contains("ef_search"), "{err}");
    }

    #[test]
    fn validate_refuses_a_non_postgres_scheme() {
        let mut cfg = config();
        cfg.database_url = "mysql://genius:genius@127.0.0.1/genius_retrieval".to_string();
        let err = cfg
            .validate()
            .expect_err("a non-postgres scheme must be refused")
            .to_string();
        assert!(err.contains("database_url"), "{err}");
    }

    #[test]
    fn session_setup_sql_substitutes_ef_search_and_max_scan_tuples() {
        let mut cfg = config();
        cfg.ef_search = 321;
        cfg.max_scan_tuples = 9_999;
        assert_eq!(
            cfg.session_setup_sql(),
            "SELECT '[1]'::vector;\n\
             SET search_path = public;\n\
             SET hnsw.ef_search = 321;\n\
             SET hnsw.iterative_scan = 'strict_order';\n\
             SET hnsw.max_scan_tuples = 9999;"
        );
    }
}
