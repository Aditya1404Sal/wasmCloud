//! Host side of `betty-blocks:retrieval`, owning the Postgres pool and the
//! embedding model on behalf of the wasm component.
//!
//! `genius-embed` is a path dependency into the context-provider POC checkout
//! (see this crate's `Cargo.toml`). Without that checkout the whole workspace
//! fails to resolve, even with this feature off, so `genius-embed` must be
//! vendored or published before this branch is merged.

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
use genius_embed::{Adapter, Embedder, ModelConfig, space_identity};
use tokio::sync::OnceCell;
use url::Url;

use crate::engine::ctx::{ActiveCtx, SharedCtx, extract_active_ctx};
use crate::engine::workload::WorkloadItem;
use crate::plugin::{HostPlugin, WitInterfaces};
use crate::wit::{WitInterface, WitWorld};

pub use config::BettyRetrievalConfig;

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
    /// Load the granite model from `config.model_config` and derive its
    /// [`SpaceInfo`]. The pool is not opened until [`HostPlugin::start`] runs.
    pub fn new(config: BettyRetrievalConfig) -> anyhow::Result<Self> {
        config.validate()?;
        let model = ModelConfig::load_anchored(&config.model_config)
            .context("load the betty-retrieval model config")?;
        model
            .verify_artifacts()
            .context("verify the betty-retrieval model artifacts")?;
        let adapter = Adapter::load_with_threads(&model, config.embed_threads, None)
            .context("load the betty-retrieval embedding model")?;
        let dimension = adapter.dim();
        let space_id = space_identity(&model, dimension)
            .context("derive the betty-retrieval embedding space identity")?
            .space_id();
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

    /// The pool [`HostPlugin::start`] opened, or an error if it has not run
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

/// Run `CREATE EXTENSION IF NOT EXISTS vector` on a one-off connection,
/// outside the pool: the pool's `post_create` hook probes `'[1]'::vector`,
/// which fails until the extension exists, and that hook runs on every
/// pooled connection.
async fn create_vector_extension(
    pg_config: &tokio_postgres::Config,
    tls: bool,
) -> anyhow::Result<()> {
    if tls {
        let (client, connection) = pg_config
            .connect(rustls_connector())
            .await
            .context("open a connection to create the vector extension")?;
        let task = tokio::spawn(async move {
            let _ = connection.await;
        });
        client
            .batch_execute("CREATE EXTENSION IF NOT EXISTS vector;")
            .await
            .context("create the vector extension")?;
        task.abort();
    } else {
        let (client, connection) = pg_config
            .connect(tokio_postgres::NoTls)
            .await
            .context("open a connection to create the vector extension")?;
        let task = tokio::spawn(async move {
            let _ = connection.await;
        });
        client
            .batch_execute("CREATE EXTENSION IF NOT EXISTS vector;")
            .await
            .context("create the vector extension")?;
        task.abort();
    }
    Ok(())
}

/// Build the pool: fast recycling, wait/create timeouts bound to
/// `connect_timeout` and run on tokio (deadpool refuses timeouts without a
/// runtime), and a `post_create` hook applying the native provider's session
/// GUCs to every physical connection.
fn build_pool(mgr: Manager, config: &BettyRetrievalConfig) -> anyhow::Result<Pool> {
    let setup = Arc::new(config.session_setup_sql());
    Pool::builder(mgr)
        .max_size(config.pool_size)
        .runtime(deadpool_postgres::Runtime::Tokio1)
        .wait_timeout(Some(config.connect_timeout))
        .create_timeout(Some(config.connect_timeout))
        .post_create(Hook::async_fn(move |client, _| {
            let setup = Arc::clone(&setup);
            Box::pin(async move {
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

    async fn start(&self) -> anyhow::Result<()> {
        let url = Url::parse(&self.config.database_url)
            .context("parse the betty-retrieval database url")?;
        let tls = extract_tls_requirement(&url);
        let pg_config: tokio_postgres::Config = self
            .config
            .database_url
            .parse()
            .context("parse the betty-retrieval postgres config")?;

        create_vector_extension(&pg_config, tls).await?;

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
