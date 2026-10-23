//! Configuration for the betty-retrieval host plugin: where the database and
//! the embedding model are, and how the connection pool and HNSW search are
//! tuned. Defaults match the native provider's.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, bail};
use url::Url;

/// Which embedding model to run, and where to find it.
///
/// A `spec` is either a path to a model descriptor — how this has always
/// worked — or a model NAME looked up in `catalog` and fetched into `cache`
/// when the right bytes are not already there. Naming a model is what lets an
/// operator change models by changing the environment rather than by
/// rebuilding this host.
///
/// `Debug` runs `catalog` and `mirror` through `redact_url`: either can carry
/// a password in its userinfo or a token in its query string.
#[derive(Clone, PartialEq, Eq)]
pub struct ModelSelection {
    pub spec: String,
    pub catalog: Option<String>,
    pub cache: Option<PathBuf>,
    pub mirror: Option<String>,
}

impl std::fmt::Debug for ModelSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use genius_embed::resolve::redact_url;
        f.debug_struct("ModelSelection")
            .field("spec", &self.spec)
            .field("catalog", &self.catalog.as_deref().map(redact_url))
            .field("cache", &self.cache)
            .field("mirror", &self.mirror.as_deref().map(redact_url))
            .finish()
    }
}

impl ModelSelection {
    /// A descriptor already on disk: no catalog, nothing to fetch.
    pub fn from_path(path: PathBuf) -> Self {
        Self {
            spec: path.to_string_lossy().into_owned(),
            catalog: None,
            cache: None,
            mirror: None,
        }
    }
}

/// How the plugin connects to Postgres, tunes HNSW search, and finds the
/// embedding model.
///
/// `Debug` prints a placeholder for `database_url`: the url carries the
/// database password, in its userinfo or in its query string.
#[derive(Clone)]
pub struct BettyRetrievalConfig {
    pub database_url: String,
    pub model: ModelSelection,
    pub pool_size: usize,
    pub connect_timeout: Duration,
    pub ef_search: u32,
    pub max_scan_tuples: u32,
    pub embed_threads: usize,
    /// `statement_timeout` on every pooled session: the server cancels a
    /// statement that runs longer. Zero means no limit.
    pub statement_timeout: Duration,
    /// `idle_in_transaction_session_timeout` on every pooled session: the
    /// server ends a session that waits inside a transaction longer, which
    /// releases the locks the transaction held. Zero means no limit.
    pub idle_in_transaction_timeout: Duration,
    /// How many of the pool's connections one workload may hold at a time.
    /// `None` is half the pool, and at least one: several applications share
    /// a host, and one that holds every connection stops all the others.
    pub connections_per_workload: Option<usize>,
    /// How much one `transaction.query` may read, in bytes as the host holds
    /// the rows: their bytes as they came over the wire, plus a fixed amount
    /// for each row and each value (see `cost.rs`). A transaction's rows are
    /// read before the call returns, so without a cap one query could use up
    /// the host's memory.
    pub transaction_query_max_bytes: usize,
    /// The most texts one statement may ask the model to embed, counted over
    /// all of its `embed-text` and `embed-texts` parameters.
    pub embed_max_texts: usize,
    /// The longest text, in bytes, a statement may ask the model to embed.
    /// The model reads only its first few hundred tokens, but the tokenizer
    /// reads all of it first.
    pub embed_max_text_bytes: usize,
    /// A PEM file of CA certificates to trust for the database's TLS, beside
    /// the public web roots. A url's `sslrootcert` names one too; this wins.
    pub ca_file: Option<PathBuf>,
}

impl std::fmt::Debug for BettyRetrievalConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Destructured, so a field added later is a compile error here rather
        // than a field `Debug` silently leaves out.
        let Self {
            database_url: _,
            model,
            pool_size,
            connect_timeout,
            ef_search,
            max_scan_tuples,
            embed_threads,
            statement_timeout,
            idle_in_transaction_timeout,
            connections_per_workload,
            transaction_query_max_bytes,
            embed_max_texts,
            embed_max_text_bytes,
            ca_file,
        } = self;
        f.debug_struct("BettyRetrievalConfig")
            .field("database_url", &format_args!("<redacted>"))
            .field("model", model)
            .field("pool_size", pool_size)
            .field("connect_timeout", connect_timeout)
            .field("ef_search", ef_search)
            .field("max_scan_tuples", max_scan_tuples)
            .field("embed_threads", embed_threads)
            .field("statement_timeout", statement_timeout)
            .field("idle_in_transaction_timeout", idle_in_transaction_timeout)
            .field("connections_per_workload", connections_per_workload)
            .field("transaction_query_max_bytes", transaction_query_max_bytes)
            .field("embed_max_texts", embed_max_texts)
            .field("embed_max_text_bytes", embed_max_text_bytes)
            .field("ca_file", ca_file)
            .finish()
    }
}

impl BettyRetrievalConfig {
    /// `database_url` and a model descriptor path, with the native provider's
    /// defaults for everything else: pool 8, 10s connect timeout, `ef_search`
    /// 200, `max_scan_tuples` 20000, 4 embed threads. The two time limits,
    /// which the native provider does not set, default to 30s for a statement
    /// and 60s for an idle transaction, a workload may hold half the pool,
    /// one transaction query may read 64 MiB, and one statement may embed
    /// 1,000 texts of up to 64 KiB each. Use
    /// [`BettyRetrievalConfig::with_model`] to name a model instead.
    pub fn new(database_url: String, model_config: PathBuf) -> Self {
        Self::with_model(database_url, ModelSelection::from_path(model_config))
    }

    /// As [`BettyRetrievalConfig::new`], for a model that may be named rather
    /// than placed.
    pub fn with_model(database_url: String, model: ModelSelection) -> Self {
        Self {
            database_url,
            model,
            pool_size: 8,
            connect_timeout: Duration::from_secs(10),
            ef_search: 200,
            max_scan_tuples: 20_000,
            embed_threads: 4,
            statement_timeout: Duration::from_secs(30),
            idle_in_transaction_timeout: Duration::from_secs(60),
            connections_per_workload: None,
            transaction_query_max_bytes: 64 * 1024 * 1024,
            embed_max_texts: 1_000,
            embed_max_text_bytes: 64 * 1024,
            ca_file: None,
        }
    }

    /// How many connections one workload may hold: the set value, or else
    /// half the pool and at least one.
    pub fn workload_share(&self) -> usize {
        self.connections_per_workload
            .unwrap_or((self.pool_size / 2).max(1))
    }

    /// Refuses an unnamed model, a zero pool size, connect timeout, embed
    /// thread count, `ef_search`, `max_scan_tuples`, transaction query cap or
    /// embedding cap,
    /// a workload share of none or more than the pool, and a `database_url`
    /// that is not `postgres`/`postgresql`.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.model.spec.trim().is_empty() {
            bail!("no embedding model was named: give a model name or a descriptor path");
        }
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
        if self.max_scan_tuples == 0 {
            bail!("max_scan_tuples must be greater than zero");
        }
        if self.transaction_query_max_bytes == 0 {
            bail!("transaction_query_max_bytes must be greater than zero");
        }
        if self.embed_max_texts == 0 {
            bail!("embed_max_texts must be greater than zero");
        }
        if self.embed_max_text_bytes == 0 {
            bail!("embed_max_text_bytes must be greater than zero");
        }
        if let Some(share) = self.connections_per_workload
            && !(1..=self.pool_size).contains(&share)
        {
            bail!(
                "connections_per_workload must be between 1 and pool_size ({}), got {share}",
                self.pool_size
            );
        }
        // The value is left out: a url can carry a password.
        let url = Url::parse(&self.database_url).context("database_url is not a valid url")?;
        if !matches!(url.scheme(), "postgres" | "postgresql") {
            bail!(
                "database_url must use the postgres or postgresql scheme, got {:?}",
                url.scheme()
            );
        }
        // Here rather than at `start`: an option the client refuses, an
        // sslmode it cannot honour or a CA file it cannot read stops the host
        // at once, saying which, instead of failing every statement later.
        super::tls::DatabaseConnection::from_url(&self.database_url, self.ca_file.as_deref())?;
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

    #[test]
    fn model_selection_debug_keeps_urls_credentials_out() {
        let selection = ModelSelection {
            spec: "granite".to_string(),
            catalog: Some("https://user:pass@example.com/c?token=abc".to_string()),
            cache: Some(PathBuf::from("/var/cache/models")),
            mirror: Some("https://mirroruser:mirrorpw@mirror.example.com/m".to_string()),
        };
        let printed = format!("{selection:?}");
        assert!(!printed.contains("pass"), "{printed}");
        assert!(!printed.contains("user:"), "{printed}");
        assert!(!printed.contains("token=abc"), "{printed}");
        assert!(!printed.contains("mirrorpw"), "{printed}");
        assert!(printed.contains("example.com"), "{printed}");
        assert!(printed.contains("granite"), "{printed}");
    }

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
    fn a_workload_may_hold_half_the_pool_by_default_and_never_none() {
        let mut cfg = config();
        assert_eq!(cfg.connections_per_workload, None);
        for (pool_size, share) in [(8, 4), (9, 4), (3, 1), (2, 1), (1, 1)] {
            cfg.pool_size = pool_size;
            assert_eq!(cfg.workload_share(), share, "pool of {pool_size}");
        }
        cfg.connections_per_workload = Some(6);
        cfg.pool_size = 8;
        assert_eq!(cfg.workload_share(), 6, "a set share wins over the default");
    }

    #[test]
    fn validate_refuses_a_workload_share_of_none_or_more_than_the_pool() {
        for share in [0, 9] {
            let mut cfg = config();
            cfg.connections_per_workload = Some(share);
            let err = cfg
                .validate()
                .expect_err("a share must be at least one and at most the pool")
                .to_string();
            assert!(err.contains("connections_per_workload"), "{err}");
        }
    }

    #[test]
    fn validate_refuses_a_ca_file_that_cannot_be_read_without_printing_the_url() {
        let mut cfg = config();
        cfg.database_url =
            "postgres://genius:hunter2@db.internal:5432/retrieval?sslmode=verify-full".to_string();
        cfg.ca_file = Some(PathBuf::from("/nonexistent/retrieval-ca.pem"));
        let err = format!(
            "{:#}",
            cfg.validate()
                .expect_err("an unreadable CA file must be refused")
        );
        assert!(err.contains("/nonexistent/retrieval-ca.pem"), "{err}");
        assert!(!err.contains("hunter2"), "{err}");
    }

    #[test]
    fn a_statement_may_embed_1000_texts_of_64_kib_by_default_and_never_none() {
        let cfg = config();
        assert_eq!(cfg.embed_max_texts, 1_000);
        assert_eq!(cfg.embed_max_text_bytes, 64 * 1024);
        for (texts, bytes, field) in [(0, 1, "embed_max_texts"), (1, 0, "embed_max_text_bytes")] {
            let mut cfg = config();
            cfg.embed_max_texts = texts;
            cfg.embed_max_text_bytes = bytes;
            let err = cfg
                .validate()
                .expect_err("a cap of zero refuses everything")
                .to_string();
            assert!(err.contains(field), "{err}");
        }
    }

    #[test]
    fn a_transaction_query_may_read_64_mib_by_default_and_never_nothing() {
        let mut cfg = config();
        assert_eq!(cfg.transaction_query_max_bytes, 64 * 1024 * 1024);
        cfg.transaction_query_max_bytes = 0;
        let err = cfg
            .validate()
            .expect_err("a cap of zero would refuse every row")
            .to_string();
        assert!(err.contains("transaction_query_max_bytes"), "{err}");
    }

    #[test]
    fn statements_and_idle_transactions_have_time_limits_by_default() {
        let cfg = config();
        assert_eq!(cfg.statement_timeout, Duration::from_secs(30));
        assert_eq!(cfg.idle_in_transaction_timeout, Duration::from_secs(60));
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
    fn validate_refuses_a_zero_max_scan_tuples() {
        let mut cfg = config();
        cfg.max_scan_tuples = 0;
        let err = cfg
            .validate()
            .expect_err("a zero max_scan_tuples must be refused")
            .to_string();
        assert!(err.contains("max_scan_tuples"), "{err}");
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
    fn validate_names_a_bad_database_url_without_its_password() {
        for url in [
            "postgres://genius:hunter2@127.0.0.1:5543x/genius_retrieval",
            "mysql://genius:hunter2@127.0.0.1/genius_retrieval",
        ] {
            let mut cfg = config();
            cfg.database_url = url.to_string();
            let err = cfg
                .validate()
                .expect_err("a bad database_url must be refused");
            let message = format!("{err:#}");
            assert!(message.contains("database_url"), "{message}");
            assert!(!message.contains("hunter2"), "{message}");
        }
    }

    #[test]
    fn debug_leaves_out_the_database_url_and_keeps_the_rest() {
        for url in [
            "postgres://genius:hunter2@db.internal:5432/retrieval?sslmode=require",
            // libpq also takes the password as a query parameter.
            "postgres://db.internal:5432/retrieval?user=genius&password=hunter2",
        ] {
            let mut cfg = config();
            cfg.database_url = url.to_string();
            cfg.pool_size = 3;
            for printed in [format!("{cfg:?}"), format!("{cfg:#?}")] {
                assert!(!printed.contains("hunter2"), "{printed}");
                assert!(!printed.contains("db.internal"), "{printed}");
                assert!(printed.contains("<redacted>"), "{printed}");
                assert!(printed.contains("pool_size: 3"), "{printed}");
                assert!(
                    printed.contains("granite-embedding-107m-multilingual.json"),
                    "{printed}"
                );
            }
            // Only printing changes: the plugin still connects with the value.
            assert_eq!(cfg.database_url, url);
        }
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
