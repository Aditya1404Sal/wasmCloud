//! Host side of `betty-blocks:retrieval`, owning the Postgres pool and the
//! embedding model on behalf of the wasm component.
//!
//! A workload lists `wasmcloud:postgres/types@0.2.0` beside
//! `betty-blocks:retrieval/types,store@0.1.0`: the retrieval types use the
//! postgres ones, so a component imports both, and this plugin links the
//! postgres types only for a workload that lists them. `wash dev` derives both
//! entries from the component's imports.
//!
//! `genius-embed`, the embedder, is vendored at `crates/genius-embed` from the
//! context-provider repository; its `README.md` says at which commit, and that
//! the two copies must be kept in step.
//!
//! Running a host with this plugin is covered in
//! `crates/wash-runtime/docs/BETTY_RETRIEVAL_PLUGIN.md`: the host settings, how
//! the model's files reach the host, the image that carries the plugin, and
//! what the Helm chart's defaults break. In short: the host starts without the
//! database but not without the model, the plugin sets no statement timeout,
//! and its TLS trusts public roots only.

mod bindings;
mod config;
mod errors;
mod params;
mod store;
mod tx;

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context as _, anyhow, bail};
use deadpool_postgres::{Hook, HookError, Manager, ManagerConfig, Pool, RecyclingMethod};
use genius_embed::{Adapter, Embedder, space_identity};
use tokio::sync::OnceCell;
use url::Url;

use crate::engine::ctx::{ActiveCtx, SharedCtx, extract_active_ctx};
use crate::engine::workload::WorkloadItem;
use crate::plugin::{HostPlugin, WitInterfaces};
use crate::wit::{WitInterface, WitWorld};

pub use config::{BettyRetrievalConfig, ModelSelection};

pub(crate) const PLUGIN_BETTY_RETRIEVAL_ID: &str = "betty-blocks-retrieval";

/// The embedding space every corpus this plugin serves is written in. Fixed
/// for the plugin's lifetime; a component refuses a corpus tagged with a
/// different one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceInfo {
    pub space_id: String,
    pub model_id: String,
    pub dimension: u32,
}

/// wasmCloud host plugin for `betty-blocks:retrieval`: owns the Postgres pool
/// and the granite embedding model, turning `embed` bind parameters into
/// pgvector vectors before a statement reaches the database.
pub struct BettyRetrieval {
    config: BettyRetrievalConfig,
    /// Turns `embed` bind parameters into vectors.
    embedder: Arc<dyn Embedder>,
    space: SpaceInfo,
    /// Filled by [`HostPlugin::start`]; a plugin that never started has none.
    pool: OnceCell<Pool>,
}

impl BettyRetrieval {
    /// Resolve the configured model — a descriptor path, or a name fetched
    /// from a catalog — load it, and derive its [`SpaceInfo`]. The pool is
    /// built when [`HostPlugin::start`] runs and connects at its first
    /// checkout.
    ///
    /// Resolving happens here, at startup, on the thread that is about to load
    /// hundreds of megabytes of model anyway: a fetch is a once-per-machine
    /// cost, and everything after it reads from the cache.
    pub fn new(config: BettyRetrievalConfig) -> anyhow::Result<Self> {
        config.validate()?;
        let resolved = genius_embed::resolve(&genius_embed::ResolveRequest {
            spec: config.model.spec.clone(),
            catalog: config.model.catalog.clone(),
            cache: config.model.cache.clone(),
            mirror: config.model.mirror.clone(),
        })
        .context("resolve the betty-retrieval embedding model")?;
        let model = resolved.config;
        // `resolve` has already checked the artifacts against the digests the
        // descriptor pins, whichever way it found them.
        let adapter = Adapter::load_with_threads(&model, config.embed_threads, None)
            .context("load the betty-retrieval embedding model")?;
        let dimension = adapter.dim();
        let space_id = space_identity(&model, dimension)
            .context("derive the betty-retrieval embedding space identity")?
            .space_id();
        tracing::info!(
            event = "retrieval_model_resolved",
            model_id = %model.model_id,
            space_id = %space_id,
            from = resolved.origin.as_str(),
            descriptor = %resolved.descriptor.display(),
            "betty-retrieval embedding model resolved"
        );
        let space = SpaceInfo {
            space_id,
            model_id: model.model_id,
            dimension: u32::try_from(dimension)
                .context("the betty-retrieval model's dimension does not fit a u32")?,
        };
        Ok(Self {
            config,
            embedder: Arc::new(adapter),
            space,
            pool: OnceCell::new(),
        })
    }

    /// Build a plugin around a pre-built embedder and its [`SpaceInfo`] — for
    /// tests, and for embedders that bring their own model.
    pub fn with_embedder(
        config: BettyRetrievalConfig,
        embedder: Arc<dyn Embedder>,
        space: SpaceInfo,
    ) -> anyhow::Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            embedder,
            space,
            pool: OnceCell::new(),
        })
    }

    /// The pool [`HostPlugin::start`] built, or an error if it has not run
    /// yet.
    pub(crate) fn pool(&self) -> anyhow::Result<&Pool> {
        self.pool
            .get()
            .ok_or_else(|| anyhow!("the betty-retrieval plugin has not started its pool yet"))
    }

    /// The model `embed` bind parameters resolve with.
    pub(crate) fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.embedder
    }

    pub fn space(&self) -> &SpaceInfo {
        &self.space
    }
}

/// Whether `sslmode` in a postgres url calls for TLS. Mirrors
/// `wasmcloud_postgres::extract_tls_requirement`, which is private to that
/// module.
fn extract_tls_requirement(url: &Url) -> bool {
    url.query_pairs()
        .find(|(k, _)| k == "sslmode")
        .map(|(_, v)| matches!(v.as_ref(), "require" | "verify-ca" | "verify-full"))
        .unwrap_or(false)
}

/// A `rustls` connector trusting the platform's web PKI roots, matching the
/// stock postgres plugin's TLS setup.
fn rustls_connector() -> tokio_postgres_rustls::MakeRustlsConnect {
    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        })
        .with_no_client_auth();
    tokio_postgres_rustls::MakeRustlsConnect::new(tls_config)
}

/// The statements that create the vector extension, one session at a time
/// across everything that connects to the database.
///
/// `IF NOT EXISTS` alone does not do that: sessions that start together on a
/// new database all see no extension and all insert one, and every one but the
/// first fails on `pg_extension_name_index`. The advisory lock is held to the
/// end of the transaction, so a session that waited for it then finds the
/// extension. The native provider takes the lock under this same key; a host
/// and a provider starting on one new database do not race either.
///
/// `lock_timeout` bounds the wait for that lock at the server. The pool's own
/// timeouts end once a connection is open, and this runs after that: without
/// the bound, a session that took the lock and never ended its transaction
/// would hold every statement of this host up for as long as it lived.
fn create_vector_extension_sql(lock_timeout: std::time::Duration) -> String {
    // Whole milliseconds, and never zero: Postgres reads zero as no limit.
    let millis = lock_timeout.as_millis().max(1);
    format!(
        "BEGIN; \
         SET LOCAL lock_timeout = {millis}; \
         SELECT pg_advisory_xact_lock(hashtext('genius-retrieval:create-extension-vector')); \
         CREATE EXTENSION IF NOT EXISTS vector; \
         COMMIT;"
    )
}

/// Build the pool: fast recycling, wait/create timeouts bound to
/// `connect_timeout` and run on tokio (deadpool refuses timeouts without a
/// runtime), and a `post_create` hook that runs on every physical connection.
/// Building opens no connection.
///
/// The hook first makes sure the vector extension exists, once per pool: the
/// session setup probes `'[1]'::vector`, which fails until it does. Connections
/// of this pool opened together wait for the one creating it, other hosts wait
/// on the lock in [`create_vector_extension_sql`] for `connect_timeout` at
/// most, and a failed attempt leaves the next connection to try again. Then it
/// applies the native provider's session GUCs.
fn build_pool(mgr: Manager, config: &BettyRetrievalConfig) -> anyhow::Result<Pool> {
    let setup = Arc::new(config.session_setup_sql());
    let create_extension = Arc::new(create_vector_extension_sql(config.connect_timeout));
    let extension = Arc::new(OnceCell::new());
    Pool::builder(mgr)
        .max_size(config.pool_size)
        .runtime(deadpool_postgres::Runtime::Tokio1)
        .wait_timeout(Some(config.connect_timeout))
        .create_timeout(Some(config.connect_timeout))
        .post_create(Hook::async_fn(move |client, _| {
            let setup = Arc::clone(&setup);
            let create_extension = Arc::clone(&create_extension);
            let extension = Arc::clone(&extension);
            Box::pin(async move {
                extension
                    .get_or_try_init(|| async {
                        client.batch_execute(&create_extension).await?;
                        tracing::info!(
                            event = "retrieval_database_reached",
                            "betty-retrieval reached its database"
                        );
                        Ok::<_, tokio_postgres::Error>(())
                    })
                    .await
                    .map_err(HookError::Backend)?;
                client
                    .batch_execute(&setup)
                    .await
                    .map_err(HookError::Backend)?;
                Ok(())
            })
        }))
        .build()
        .context("build the betty-retrieval connection pool")
}

impl bindings::wasmcloud::postgres::types::Host for ActiveCtx<'_> {}

#[async_trait::async_trait]
impl HostPlugin for BettyRetrieval {
    fn id(&self) -> &'static str {
        PLUGIN_BETTY_RETRIEVAL_ID
    }

    fn world(&self) -> WitWorld {
        WitWorld {
            imports: HashSet::from([
                WitInterface::from("betty-blocks:retrieval/types,store@0.1.0"),
                WitInterface::from("wasmcloud:postgres/types@0.2.0"),
            ]),
            ..Default::default()
        }
    }

    /// Builds the pool and opens no connection, so a host comes up while its
    /// database is down. The pool connects at the first checkout; until one
    /// succeeds, every statement answers `connection-failed`.
    async fn start(&self) -> anyhow::Result<()> {
        let url = Url::parse(&self.config.database_url)
            .context("parse the betty-retrieval database url")?;
        let tls = extract_tls_requirement(&url);
        let pg_config: tokio_postgres::Config = self
            .config
            .database_url
            .parse()
            .context("parse the betty-retrieval postgres config")?;

        let mgr_config = ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        };
        let pool = if tls {
            build_pool(
                Manager::from_config(pg_config, rustls_connector(), mgr_config),
                &self.config,
            )?
        } else {
            build_pool(
                Manager::from_config(pg_config, tokio_postgres::NoTls, mgr_config),
                &self.config,
            )?
        };

        self.pool
            .set(pool)
            .map_err(|_| anyhow!("the betty-retrieval plugin already started"))?;
        Ok(())
    }

    async fn on_workload_item_bind<'a>(
        &self,
        item: &mut WorkloadItem<'a>,
        interfaces: WitInterfaces<'_>,
    ) -> anyhow::Result<()> {
        let retrieval: Vec<&WitInterface> = interfaces
            .iter()
            .filter(|i| i.namespace == "betty-blocks" && i.package == "retrieval")
            .collect();
        if retrieval.is_empty() {
            // Alone, a postgres types entry is a stock-postgres workload's, and
            // that plugin links the types when it binds query or prepared.
            return Ok(());
        }
        if retrieval.iter().any(|i| !i.config.is_empty()) {
            bail!(
                "betty-blocks:retrieval does not read interface config: the database and model are \
                 host settings (`wash host --retrieval-*` flags or `dev.retrieval_*` keys), not \
                 workload config"
            );
        }
        let linker = item.linker();
        store::add_to_linker(linker)?;
        // Plugins are offered interfaces in id order, so this plugin gets the
        // postgres types entry before the stock one, which would bail on it.
        if interfaces
            .iter()
            .any(|i| i.namespace == "wasmcloud" && i.package == "postgres")
        {
            bindings::wasmcloud::postgres::types::add_to_linker::<_, SharedCtx>(
                linker,
                extract_active_ctx,
            )?;
        }
        Ok(())
    }

    /// Closes the pool and nothing more. A transaction a guest still holds
    /// keeps its pinned connection until the guest commits or drops it, and
    /// that connection is then closed rather than pooled. Every later checkout
    /// answers `connection-failed` ("Pool has been closed"). Nothing is logged.
    async fn stop(&self) -> anyhow::Result<()> {
        if let Some(pool) = self.pool.get() {
            pool.close();
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    struct StubEmbedder;

    impl Embedder for StubEmbedder {
        fn embed(&self, _text: &str, _role: genius_embed::Role) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.0; 4])
        }

        fn dim(&self) -> usize {
            4
        }
    }

    fn stub_space() -> SpaceInfo {
        SpaceInfo {
            space_id: "test-space".to_string(),
            model_id: "test-model".to_string(),
            dimension: 4,
        }
    }

    fn stub_config() -> BettyRetrievalConfig {
        BettyRetrievalConfig::new(
            "postgres://genius:genius@127.0.0.1:55433/genius_retrieval".to_string(),
            std::path::PathBuf::from("models/granite-embedding-107m-multilingual.json"),
        )
    }

    #[test]
    fn with_embedder_reports_the_given_space() {
        let plugin =
            BettyRetrieval::with_embedder(stub_config(), Arc::new(StubEmbedder), stub_space())
                .expect("with_embedder builds a plugin from a stub embedder");
        assert_eq!(plugin.space(), &stub_space());
    }

    #[test]
    fn the_world_claims_the_postgres_types_the_retrieval_types_use() {
        let plugin =
            BettyRetrieval::with_embedder(stub_config(), Arc::new(StubEmbedder), stub_space())
                .expect("with_embedder builds a plugin from a stub embedder");
        let imports = plugin.world().imports;
        assert!(imports.contains(&WitInterface::from(
            "betty-blocks:retrieval/types,store@0.1.0"
        )));
        assert!(imports.contains(&WitInterface::from("wasmcloud:postgres/types@0.2.0")));
    }

    #[test]
    fn pool_errors_before_start_runs() {
        let plugin =
            BettyRetrieval::with_embedder(stub_config(), Arc::new(StubEmbedder), stub_space())
                .expect("with_embedder builds a plugin from a stub embedder");
        let err = plugin
            .pool()
            .expect_err("the pool is not open before start() runs")
            .to_string();
        assert!(err.contains("start"), "{err}");
    }

    /// A plugin whose database is at `addr`, giving up on a connection after
    /// 300 ms so a test can wait the timeout out.
    fn plugin_for_database_at(addr: std::net::SocketAddr) -> BettyRetrieval {
        let mut config = BettyRetrievalConfig::new(
            format!("postgres://betty:betty@{addr}/never_connected"),
            std::path::PathBuf::from("models/granite-embedding-107m-multilingual.json"),
        );
        config.connect_timeout = std::time::Duration::from_millis(300);
        BettyRetrieval::with_embedder(config, Arc::new(StubEmbedder), stub_space())
            .expect("with_embedder builds a plugin from a stub embedder")
    }

    /// A loopback port with no server behind it: bound, so no other process
    /// can take it, and never listening. Linux refuses a connection to it and
    /// macOS lets it time out; either way the database is down.
    fn dead_port() -> (tokio::net::TcpSocket, std::net::SocketAddr) {
        let socket = tokio::net::TcpSocket::new_v4().expect("a tcp socket");
        socket
            .bind("127.0.0.1:0".parse().expect("a loopback address"))
            .expect("bind a loopback port");
        let addr = socket.local_addr().expect("the bound address");
        (socket, addr)
    }

    fn is_connection_failed(
        checkout: &Result<
            deadpool_postgres::Client,
            bindings::betty_blocks::retrieval::types::Error,
        >,
    ) -> bool {
        use bindings::betty_blocks::retrieval::types::Error;
        use bindings::wasmcloud::postgres::types::Error as PgError;
        matches!(checkout, Err(Error::Postgres(PgError::ConnectionFailed(_))))
    }

    /// The host must come up with the database down: `start` opens no
    /// connection.
    #[tokio::test]
    async fn start_succeeds_while_the_database_is_down() {
        let (_port, addr) = dead_port();
        plugin_for_database_at(addr)
            .start()
            .await
            .expect("start must not need the database");
    }

    #[tokio::test]
    async fn a_checkout_while_the_database_is_down_is_connection_failed() {
        let (_port, addr) = dead_port();
        let plugin = plugin_for_database_at(addr);
        plugin
            .start()
            .await
            .expect("start must not need the database");
        let checkout = store::checkout(&plugin).await;
        assert!(
            is_connection_failed(&checkout),
            "a connection that could not be opened must be connection-failed, got {:?}",
            checkout.err()
        );
    }

    /// A server that accepts the connection and then says nothing: the port
    /// listens, so the handshake completes, and nothing ever reads from it.
    /// Neither `start` nor a checkout may wait on it past the connect timeout.
    #[tokio::test]
    async fn a_silent_server_holds_up_neither_start_nor_a_checkout_past_the_timeout() {
        let silent = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let plugin = plugin_for_database_at(silent.local_addr().expect("the bound address"));
        let patience = std::time::Duration::from_secs(5);

        tokio::time::timeout(patience, plugin.start())
            .await
            .expect("start must not wait on a server that never answers")
            .expect("start must not need the database");

        let checkout = tokio::time::timeout(patience, store::checkout(&plugin))
            .await
            .expect("a checkout must give up at the connect timeout");
        assert!(
            is_connection_failed(&checkout),
            "a connection that timed out must be connection-failed, got {:?}",
            checkout.err()
        );
    }

    /// Postgres reads `lock_timeout = 0` as "wait for ever", which is the one
    /// thing the setting is there to prevent.
    #[test]
    fn the_extension_lock_wait_is_bounded_in_whole_milliseconds_and_never_zero() {
        use std::time::Duration;
        assert!(
            create_vector_extension_sql(Duration::from_secs(10))
                .contains("SET LOCAL lock_timeout = 10000;")
        );
        assert!(
            create_vector_extension_sql(Duration::from_micros(10))
                .contains("SET LOCAL lock_timeout = 1;"),
            "a timeout under one millisecond must round up, not down to no limit"
        );
    }

    /// deadpool refuses wait and create timeouts at `build()` unless the pool
    /// has a runtime to run them on; building never connects.
    #[test]
    fn the_pool_builds_with_the_plugins_timeouts_set() {
        let config = BettyRetrievalConfig::new(
            "postgres://betty:betty@127.0.0.1:1/never_connected".to_string(),
            std::path::PathBuf::from("models/granite-embedding-107m-multilingual.json"),
        );
        let pg_config: tokio_postgres::Config = config
            .database_url
            .parse()
            .expect("the dummy url parses as a postgres config");
        let manager = Manager::from_config(
            pg_config,
            tokio_postgres::NoTls,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        build_pool(manager, &config).expect("a pool with the plugin's timeouts builds");
    }

    /// Cross-checks this plugin's `BettyRetrieval::new` against the native
    /// provider's own reported space identity for the same model, so a drift
    /// between the two embedding paths is caught here rather than in a
    /// mismatched corpus. Needs the real model on disk, so it is `#[ignore]`d
    /// and skips itself (with a reason on stderr) when
    /// `BETTY_RETRIEVAL_TEST_MODEL_CONFIG` is unset.
    #[test]
    #[ignore = "needs the granite model on disk; run with --ignored"]
    fn space_identity_matches_the_native_provider() {
        let Ok(model_config) = std::env::var("BETTY_RETRIEVAL_TEST_MODEL_CONFIG") else {
            eprintln!(
                "skipping: set BETTY_RETRIEVAL_TEST_MODEL_CONFIG to the granite model json to run this test"
            );
            return;
        };
        let config = BettyRetrievalConfig::new(
            "postgres://genius:genius@127.0.0.1:55433/genius_retrieval".to_string(),
            std::path::PathBuf::from(model_config),
        );
        let plugin = BettyRetrieval::new(config).expect("load the betty-retrieval plugin");
        assert_eq!(plugin.space().dimension, 384);
        assert_eq!(
            plugin.space().model_id,
            "granite-embedding-107m-multilingual"
        );
        assert_eq!(
            plugin.space().space_id,
            "471a7b4ab27a48ac568db817ea95029601a3e7f55079fe28d78439236ee955ba"
        );
    }
}
