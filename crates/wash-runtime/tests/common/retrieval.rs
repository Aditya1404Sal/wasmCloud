//! Shared helpers for the `betty-blocks:retrieval@0.1.0` integration tests: a
//! database with pgvector, a scratch table on a connection of the test's own,
//! and a host running the `retrieval-store-p3` fixture on the betty-retrieval
//! plugin.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, ensure};
use genius_embed::FakeEmbedder;
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio::sync::OnceCell;
use url::Url;

use wash_runtime::{
    engine::Engine,
    host::{
        HostApi, HostBuilder,
        http::{DevRouter, Ingress},
    },
    plugin::betty_retrieval::{BettyRetrieval, BettyRetrievalConfig, SpaceInfo},
    types::{Component, LocalResources, Workload, WorkloadStartRequest, WorkloadState},
    wit::WitInterface,
};

use super::http_incoming_handler_interface;
use super::postgres::admin_client;

const RETRIEVAL_FIXTURE_WASM: &[u8] = include_bytes!("../wasm/retrieval_store_p3.wasm");

/// The `Host` header the fixture's workload answers to.
pub const HOST_HEADER: &str = "retrieval-store";

/// The width the tests embed at: the granite model's.
pub const DIMENSION: usize = 384;

/// A [`FakeEmbedder`] at [`DIMENSION`], shared so a test can count its calls.
pub fn fake_embedder() -> Arc<FakeEmbedder> {
    Arc::new(FakeEmbedder::new(DIMENSION))
}

/// The embedding space the plugin under test reports.
pub fn fake_space() -> SpaceInfo {
    SpaceInfo {
        space_id: "betty-it-fake-space".to_string(),
        model_id: "genius-embed-fake".to_string(),
        dimension: u32::try_from(DIMENSION).expect("the dimension fits a u32"),
    }
}

/// The database the tests run against, and the container behind it when one
/// had to be started.
pub struct TestDatabase {
    pub url: String,
    _container: Option<ContainerAsync<GenericImage>>,
}

/// `BETTY_RETRIEVAL_TEST_DATABASE_URL` when set (locally, the genius-retrieval
/// compose database); otherwise a `pgvector/pgvector:pg17` container that
/// lives as long as the returned value. Either way the vector extension exists
/// before a host starts.
pub async fn test_database() -> Result<TestDatabase> {
    if let Ok(url) = std::env::var("BETTY_RETRIEVAL_TEST_DATABASE_URL") {
        // Once for all the tests sharing this database, so that they do not
        // depend on the plugin creating the extension. That is tested on its
        // own, on databases made without it (`FreshDatabase`).
        static VECTOR_EXTENSION: OnceCell<()> = OnceCell::const_new();
        VECTOR_EXTENSION
            .get_or_try_init(|| create_vector_extension(&url))
            .await?;
        return Ok(TestDatabase {
            url,
            _container: None,
        });
    }
    let container = GenericImage::new("pgvector/pgvector", "pg17")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .start()
        .await
        .map_err(|e| anyhow!("failed to start pgvector: {e}"))?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    // The image logs that line for its initdb server too, which takes no TCP
    // connections; this waits for the server that does.
    create_vector_extension(&url).await?;
    Ok(TestDatabase {
        url,
        _container: Some(container),
    })
}

/// `CREATE EXTENSION IF NOT EXISTS vector`, once `database_url` accepts
/// connections.
async fn create_vector_extension(database_url: &str) -> Result<()> {
    admin_client(database_url)
        .await?
        .batch_execute("CREATE EXTENSION IF NOT EXISTS vector")
        .await
        .context("create the vector extension")
}

/// A pool of `pool_size` connections and a five-second wait, so a connection
/// the plugin never returns makes the next checkout `pool-exhausted` well
/// inside the request. With one connection, the statements one request runs
/// share a Postgres session unless the plugin replaced it.
fn plugin(
    database_url: &str,
    embedder: Arc<FakeEmbedder>,
    pool_size: usize,
) -> Result<BettyRetrieval> {
    let mut config = BettyRetrievalConfig::new(
        database_url.to_string(),
        // `with_embedder` takes the embedder ready-made, so nothing reads this.
        PathBuf::from("unused-model-config.json"),
    );
    config.pool_size = pool_size;
    config.connect_timeout = Duration::from_secs(5);
    BettyRetrieval::with_embedder(config, embedder, fake_space())
}

/// Stand up a host with the betty-retrieval plugin over `database_url`, with a
/// pool of `pool_size` connections and embedding with `embedder`, plus an HTTP
/// entrypoint, and start the `retrieval-store-p3` workload under
/// [`HOST_HEADER`].
pub async fn start_retrieval_workload(
    database_url: &str,
    embedder: Arc<FakeEmbedder>,
    pool_size: usize,
) -> Result<(std::net::SocketAddr, impl HostApi + use<>)> {
    let engine = Engine::builder().build()?;
    let ingress = Ingress::new(DevRouter::default(), "127.0.0.1:0".parse()?).await?;
    let addr = ingress.addr();

    let host = HostBuilder::new()
        .with_engine(engine)
        .with_http_handler(Arc::new(ingress))
        .with_plugin(Arc::new(plugin(database_url, embedder, pool_size)?))?
        .build()?;
    let host = host.start().await.context("failed to start host")?;

    let req = WorkloadStartRequest {
        workload_id: uuid::Uuid::new_v4().to_string(),
        workload: Workload {
            namespace: "test".to_string(),
            name: "retrieval-store-p3".to_string(),
            annotations: HashMap::new(),
            service: None,
            components: vec![Component {
                name: "retrieval-store-p3.wasm".to_string(),
                digest: None,
                source: wash_runtime::types::Source::Compile(bytes::Bytes::from_static(
                    RETRIEVAL_FIXTURE_WASM,
                )),
                local_resources: LocalResources::default(),
                pool_size: 1,
                max_invocations: 100,
                max_concurrency: 1,
                reclaim_window_seconds: 0,
                reclaim_min_instances: 0,
            }],
            host_interfaces: vec![
                http_incoming_handler_interface(HOST_HEADER, None),
                WitInterface::from("betty-blocks:retrieval/types,store@0.1.0"),
                // The retrieval types `use` these, so the component imports them too.
                WitInterface::from("wasmcloud:postgres/types@0.2.0"),
            ],
            volumes: vec![],
        },
    };

    let resp = host
        .workload_start(req)
        .await
        .context("workload_start call failed")?;
    ensure!(
        resp.workload_status.workload_state == WorkloadState::Running,
        "workload should resolve: {}",
        resp.workload_status.message
    );
    Ok((addr, host))
}

/// A uniquely named `public.betty_it_` table of one text column, created, read
/// and dropped on a connection of the test's own rather than the plugin's. A
/// table its test never passes to [`ScratchTable::drop_table`], having panicked
/// or returned early, is dropped when the value is.
pub struct ScratchTable {
    pub name: String,
    database_url: String,
    client: tokio_postgres::Client,
    dropped: bool,
}

impl ScratchTable {
    pub async fn create(database_url: &str, purpose: &str) -> Result<Self> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        // Qualified: the plugin's sessions pin `search_path` to `public`, while
        // this connection keeps the role's own.
        let name = format!("public.betty_it_{purpose}_{}_{nanos}", std::process::id());
        let client = scratch_client(database_url).await?;
        client
            .batch_execute(&format!("CREATE TABLE {name} (v text)"))
            .await
            .with_context(|| format!("create {name}"))?;
        Ok(Self {
            name,
            database_url: database_url.to_string(),
            client,
            dropped: false,
        })
    }

    /// Every value in the table, in order, as this connection sees them.
    pub async fn values(&self) -> Result<Vec<String>> {
        let rows = self
            .client
            .query(&format!("SELECT v FROM {} ORDER BY v", self.name), &[])
            .await
            .with_context(|| format!("read {}", self.name))?;
        rows.iter()
            .map(|row| row.try_get(0).context("decode a value"))
            .collect()
    }

    pub async fn drop_table(mut self) -> Result<()> {
        let dropped = drop_scratch_table(&self.client, &self.name).await;
        self.dropped = true;
        dropped
    }
}

impl Drop for ScratchTable {
    fn drop(&mut self) {
        if self.dropped {
            return;
        }
        // `drop` cannot await, and no runtime can block inside the test's: the
        // table goes on a thread with a runtime and a connection of its own.
        let (url, name) = (self.database_url.clone(), self.name.clone());
        let cleanup = std::thread::spawn(move || -> Result<()> {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(async {
                    let client = scratch_client(&url).await?;
                    drop_scratch_table(&client, &name).await
                })
        });
        match cleanup.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("left scratch table {} behind: {e:#}", self.name),
            Err(_) => eprintln!(
                "left scratch table {} behind: the thread dropping it panicked",
                self.name
            ),
        }
    }
}

/// A connection of the test's own, on which a statement waiting for a lock
/// fails after ten seconds instead of hanging the test.
async fn scratch_client(database_url: &str) -> Result<tokio_postgres::Client> {
    let client = admin_client(database_url).await?;
    client
        .batch_execute("SET lock_timeout = '10s'")
        .await
        .context("set lock_timeout")?;
    Ok(client)
}

/// Drop `table`, first terminating every other session holding a lock on it.
/// A transaction the plugin failed to end holds one, and the plugin's pool can
/// outlive the test's host through the ingress server's workload handles.
async fn drop_scratch_table(client: &tokio_postgres::Client, table: &str) -> Result<()> {
    let terminated = client
        .query(
            &format!(
                "SELECT pg_terminate_backend(pid) FROM pg_locks \
                 WHERE relation = '{table}'::regclass AND pid <> pg_backend_pid()"
            ),
            &[],
        )
        .await
        .with_context(|| format!("terminate the sessions locking {table}"))?;
    if !terminated.is_empty() {
        eprintln!(
            "terminated the sessions behind {} lock(s) on {table}",
            terminated.len()
        );
    }
    client
        .batch_execute(&format!("DROP TABLE {table}"))
        .await
        .with_context(|| format!("drop {table}"))
}

/// A loopback address with no server behind it until [`Gate::open`], and one
/// that forwards every connection to a server after: a database that is down
/// and then up, with no container restarted and no address changing under the
/// plugin. Closed, Linux refuses a connection to it and macOS lets it time out.
pub struct Gate {
    addr: SocketAddr,
    /// Bound, so the port stays this gate's, and listening only once open.
    closed: Option<tokio::net::TcpSocket>,
}

impl Gate {
    pub fn closed() -> Result<Self> {
        let socket = tokio::net::TcpSocket::new_v4()?;
        socket.bind("127.0.0.1:0".parse()?)?;
        Ok(Self {
            addr: socket.local_addr()?,
            closed: Some(socket),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Start forwarding connections to `server`, a `host:port`.
    pub fn open(&mut self, server: String) -> Result<()> {
        let listener = self
            .closed
            .take()
            .context("the gate is already open")?
            .listen(64)?;
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                let server = server.clone();
                tokio::spawn(async move {
                    if let Ok(mut outbound) = tokio::net::TcpStream::connect(&server).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
            }
        });
        Ok(())
    }
}

/// A new database on the test database's server, without the vector extension:
/// a plugin pointed at it has to create the extension itself. A database its
/// test never passes to [`FreshDatabase::drop_database`], having panicked or
/// returned early, is dropped when the value is.
pub struct FreshDatabase {
    name: String,
    /// The test database's url: where this one is created and dropped from.
    server_url: String,
    dropped: bool,
}

impl FreshDatabase {
    pub async fn create(server_url: &str, purpose: &str) -> Result<Self> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let name = format!("betty_it_{purpose}_{}_{nanos}", std::process::id());
        admin_client(server_url)
            .await?
            .batch_execute(&format!("CREATE DATABASE {name}"))
            .await
            .with_context(|| format!("create database {name}"))?;
        Ok(Self {
            name,
            server_url: server_url.to_string(),
            dropped: false,
        })
    }

    /// This database's url, at the server's own address.
    pub fn url(&self) -> Result<String> {
        let mut url = Url::parse(&self.server_url)?;
        url.set_path(&self.name);
        Ok(url.into())
    }

    /// This database's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The server's own `host:port`.
    pub fn server_address(&self) -> Result<String> {
        let url = Url::parse(&self.server_url)?;
        let host = url.host_str().context("the database url names no host")?;
        let port = url.port().unwrap_or(5432);
        Ok(format!("{host}:{port}"))
    }

    /// This database's url, with `addr` in place of the server's address.
    pub fn url_through(&self, addr: SocketAddr) -> Result<String> {
        let mut url = Url::parse(&self.server_url)?;
        url.set_ip_host(addr.ip())
            .map_err(|()| anyhow!("the database url takes no host"))?;
        url.set_port(Some(addr.port()))
            .map_err(|()| anyhow!("the database url takes no port"))?;
        url.set_path(&self.name);
        Ok(url.into())
    }

    pub async fn drop_database(mut self) -> Result<()> {
        let dropped = drop_database(&self.server_url, &self.name).await;
        self.dropped = true;
        dropped
    }
}

impl Drop for FreshDatabase {
    fn drop(&mut self) {
        if self.dropped {
            return;
        }
        // As `ScratchTable`: `drop` cannot await, so the database goes on a
        // thread with a runtime of its own.
        let (url, name) = (self.server_url.clone(), self.name.clone());
        let cleanup = std::thread::spawn(move || -> Result<()> {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(drop_database(&url, &name))
        });
        match cleanup.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("left database {} behind: {e:#}", self.name),
            Err(_) => eprintln!(
                "left database {} behind: the thread dropping it panicked",
                self.name
            ),
        }
    }
}

/// Drop `name`, closing the sessions a plugin's pool still holds on it.
async fn drop_database(server_url: &str, name: &str) -> Result<()> {
    admin_client(server_url)
        .await?
        .batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .await
        .with_context(|| format!("drop database {name}"))
}
