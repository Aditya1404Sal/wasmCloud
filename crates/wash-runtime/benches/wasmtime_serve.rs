//! "wasmtime serve"–equivalent HTTP benchmark.
//!
//! Uses `wasmtime_wasi_http::handler::ProxyHandler`  - the same dispatch path
//! `wasmtime serve` uses  - to get instance reuse and concurrent reuse. The
//! defaults mirror wasmtime serve:
//!
//!   * P2: `max_instance_reuse_count = 1` (no reuse; fresh instance per request)
//!   * P3: `max_instance_reuse_count = 128`, `max_instance_concurrent_reuse_count = 16`
//!
//! Compare against `wasmtime_baseline.rs` (no-reuse, per-request instantiation)
//! and `http_invoke.rs` (wash-runtime). The three benches together give a
//! decomposition of where the 5× gap against `wasmtime serve` lives:
//!
//!   `http_invoke`           =  wasmtime + wash-runtime wrappers
//!   `wasmtime_baseline`     =  wasmtime, per-request instance (our strategy)
//!   `wasmtime_serve`        =  wasmtime + ProxyHandler (wasmtime serve's strategy)
//!
//! Run with:
//! ```text
//! cargo bench -p wash-runtime --features wasip3 --bench wasmtime_serve
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use bytes::Bytes;
use common::Flavor;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use http_body_util::{BodyExt, combinators::UnsyncBoxBody};
use hyper::body::Incoming;
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto,
};
use tokio::{net::TcpListener, runtime::Runtime, sync::oneshot, task::JoinHandle};
use wasmtime::{
    Engine, Store, StoreContextMut,
    component::{Component, GuestTaskId, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::{
    WasiHttpCtx,
    handler::{
        HandlerState, Instance, ProxyHandler, ProxyPre, ShouldAccept, ViewFn, WorkerExpiration,
        WorkerState, WorkerStatus,
    },
};

// Response body produced by `ProxyHandler::handle`.
type RespBody = UnsyncBoxBody<Bytes, wasmtime::Error>;

// Defaults lifted from wasmtime-cli/src/commands/serve.rs.
const DEFAULT_WASIP3_MAX_INSTANCE_REUSE_COUNT: usize = 128;
const DEFAULT_WASIP2_MAX_INSTANCE_REUSE_COUNT: usize = 1;
const DEFAULT_WASIP3_MAX_INSTANCE_CONCURRENT_REUSE_COUNT: usize = 16;

// ---------------------------------------------------------------------------
// Store context
// ---------------------------------------------------------------------------

struct Ctx {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
}

impl Ctx {
    fn new() -> Self {
        Self {
            wasi: WasiCtxBuilder::new().build(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
        }
    }
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl wasmtime_wasi_http::p2::WasiHttpView for Ctx {
    fn http(&mut self) -> wasmtime_wasi_http::p2::WasiHttpCtxView<'_> {
        wasmtime_wasi_http::p2::WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

impl wasmtime_wasi_http::p3::WasiHttpView for Ctx {
    fn http(&mut self) -> wasmtime_wasi_http::p3::WasiHttpCtxView<'_> {
        wasmtime_wasi_http::p3::WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: wasmtime_wasi_http::p3::default_hooks(),
        }
    }
}

// ---------------------------------------------------------------------------
// Engine + Linker
// ---------------------------------------------------------------------------

fn max_instance_reuse(flavor: Flavor) -> usize {
    match flavor {
        Flavor::P2 => DEFAULT_WASIP2_MAX_INSTANCE_REUSE_COUNT,
        Flavor::P3 => DEFAULT_WASIP3_MAX_INSTANCE_REUSE_COUNT,
    }
}

fn max_concurrent_reuse(flavor: Flavor) -> usize {
    match flavor {
        Flavor::P2 => 1,
        Flavor::P3 => DEFAULT_WASIP3_MAX_INSTANCE_CONCURRENT_REUSE_COUNT,
    }
}

fn build_engine() -> anyhow::Result<Engine> {
    let mut cfg = wasmtime::Config::default();
    let mut pool = wasmtime::PoolingAllocationConfig::default();
    pool.total_memories(100);
    pool.total_tables(100);
    pool.total_component_instances(100);
    cfg.allocation_strategy(wasmtime::InstanceAllocationStrategy::Pooling(pool));
    cfg.wasm_component_model_async(true);
    Ok(Engine::new(&cfg)?)
}

fn build_linker(engine: &Engine) -> anyhow::Result<Linker<Ctx>> {
    let mut linker: Linker<Ctx> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    Ok(linker)
}

// ---------------------------------------------------------------------------
// Worker policy  - WorkerExpiration + WorkerState mirror wasmtime serve.
// ---------------------------------------------------------------------------

/// Decides when an idle/active worker should be dropped. `request_timeout` of
/// `Duration::MAX` means a request never expires (what we want for a bench).
struct Expiration {
    idle_timeout: Duration,
    request_timeout: Duration,
    sleep: Pin<Box<tokio::time::Sleep>>,
}

impl WorkerExpiration for Expiration {
    fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        status: WorkerStatus,
        start: Instant,
    ) -> Poll<()> {
        // `Self` is `Unpin` (only `Pin<Box<Sleep>>` + `Duration` fields), so
        // we can take a `&mut Self` and pin-project through the boxed sleep.
        let me = self.get_mut();
        let timeout = match status {
            WorkerStatus::Idle => me.idle_timeout,
            WorkerStatus::Requests | WorkerStatus::PostReturn => me.request_timeout,
        };
        if let Some(deadline) = start.checked_add(timeout) {
            let deadline = deadline.into();
            if deadline != me.sleep.deadline() {
                me.sleep.as_mut().reset(deadline);
            }
            me.sleep.as_mut().poll(cx)
        } else {
            // `start + Duration::MAX` overflows: never expire.
            Poll::Pending
        }
    }
}

/// Instance-reuse policy + per-request hooks.
struct Worker {
    max_instance_reuse_count: usize,
    max_instance_concurrent_reuse_count: usize,
    request_timeout: Duration,
}

impl WorkerState for Worker {
    type StoreData = Ctx;
    type RequestId = u64;

    fn should_accept_request(&self, concurrent_count: usize, total_count: usize) -> ShouldAccept {
        if total_count >= self.max_instance_reuse_count {
            ShouldAccept::Never
        } else if concurrent_count >= self.max_instance_concurrent_reuse_count {
            ShouldAccept::No
        } else {
            ShouldAccept::Yes
        }
    }

    fn on_request_start(
        &self,
        _store: StoreContextMut<'_, Ctx>,
        _id: u64,
        _task: GuestTaskId,
    ) -> Pin<Box<dyn Future<Output = ()> + 'static + Send + Sync>> {
        // Resolving this future expires the request; `Duration::MAX` means it
        // never does, so the guest always gets to produce a response.
        Box::pin(tokio::time::sleep(self.request_timeout))
    }

    fn drop(&self, store: Store<Ctx>, result: Result<(), wasmtime::Error>) {
        if let Err(error) = result {
            eprintln!("[wasmtime_serve bench] worker error: {error:?}");
        }
        drop(store);
    }
}

// ---------------------------------------------------------------------------
// HandlerState  - tells ProxyHandler how to instantiate workers + bound reuse.
// ---------------------------------------------------------------------------

struct State {
    engine: Engine,
    instance: ProxyPre<Ctx>,
    max_instance_reuse_count: usize,
    max_instance_concurrent_reuse_count: usize,
    request_timeout: Duration,
    idle_timeout: Duration,
    next_request_id: AtomicU64,
}

impl State {
    fn view(&self) -> ViewFn<Ctx> {
        match &self.instance {
            ProxyPre::P2(_) => ViewFn::P2(wasmtime_wasi_http::p2::WasiHttpView::http),
            ProxyPre::P3(_) => ViewFn::P3(wasmtime_wasi_http::p3::WasiHttpView::http),
        }
    }
}

impl HandlerState for State {
    type StoreData = Ctx;
    type WorkerExpiration = Expiration;
    type WorkerState = Worker;

    async fn instantiate(&self) -> wasmtime::Result<Instance<Ctx, Expiration, Worker>> {
        let mut store = Store::new(&self.engine, Ctx::new());
        let proxy = self.instance.instantiate_async(&mut store).await?;
        Ok(Instance {
            store,
            proxy,
            view: self.view(),
            expiration: Expiration {
                idle_timeout: self.idle_timeout,
                request_timeout: self.request_timeout,
                sleep: Box::pin(tokio::time::sleep(Duration::MAX)),
            },
            state: Worker {
                max_instance_reuse_count: self.max_instance_reuse_count,
                max_instance_concurrent_reuse_count: self.max_instance_concurrent_reuse_count,
                request_timeout: self.request_timeout,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Request dispatch  - ProxyHandler::handle drives the whole P2/P3 + reuse path.
// ---------------------------------------------------------------------------

async fn handle_request(
    handler: ProxyHandler<State>,
    req: hyper::Request<Incoming>,
) -> wasmtime::Result<hyper::Response<RespBody>> {
    use wasmtime_wasi_http::p3::bindings::http::types::ErrorCode;

    let request_id = handler
        .state()
        .next_request_id
        .fetch_add(1, Ordering::Relaxed);

    // Map the incoming hyper body into the error type `handler::Request` wants.
    let req = req.map(|body| {
        body.map_err(ErrorCode::from_hyper_request_error)
            .map_err(wasmtime_wasi_http::handler::ErrorCode::from)
            .boxed_unsync()
    });

    handler.handle(request_id, req).await
}

// ---------------------------------------------------------------------------
// Hyper server  - binds, accepts, dispatches through ProxyHandler.
// ---------------------------------------------------------------------------

struct Server {
    addr: SocketAddr,
    shutdown: oneshot::Sender<()>,
    _join: JoinHandle<()>,
}

impl Server {
    async fn start(flavor: Flavor) -> anyhow::Result<Self> {
        let engine = build_engine()?;
        let linker = build_linker(&engine)?;
        let component = Component::from_binary(&engine, flavor.wasm())?;
        let instance_pre = linker.instantiate_pre(&component)?;

        // Wrap InstancePre in the right handler::ProxyPre variant.
        let handler_pre = match flavor {
            Flavor::P2 => ProxyPre::P2(wasmtime_wasi_http::p2::bindings::ProxyPre::new(
                instance_pre,
            )?),
            Flavor::P3 => ProxyPre::P3(wasmtime_wasi_http::p3::bindings::ServicePre::new(
                instance_pre,
            )?),
        };

        let handler = ProxyHandler::new(State {
            engine,
            instance: handler_pre,
            max_instance_reuse_count: max_instance_reuse(flavor),
            max_instance_concurrent_reuse_count: max_concurrent_reuse(flavor),
            request_timeout: Duration::MAX,
            // Short enough that idle workers don't linger across bench groups,
            // long enough that it never fires mid-run.
            idle_timeout: Duration::from_secs(60),
            next_request_id: AtomicU64::new(0),
        });

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        let join = tokio::spawn(async move {
            loop {
                let accept = tokio::select! {
                    _ = &mut shutdown_rx => break,
                    res = listener.accept() => res,
                };
                let (stream, _) = match accept {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                stream.set_nodelay(true).ok();
                let handler = handler.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service =
                        hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                            let handler = handler.clone();
                            async move {
                                match handle_request(handler, req).await {
                                    Ok(r) => Ok::<_, hyper::Error>(r),
                                    Err(e) => {
                                        tracing::error!(err = ?e, "handler error");
                                        Ok(error_response(500))
                                    }
                                }
                            }
                        });
                    let builder = {
                        let mut b = auto::Builder::new(TokioExecutor::new());
                        b.http1().timer(TokioTimer::new());
                        b.http2().timer(TokioTimer::new());
                        b
                    };
                    let _ = builder.serve_connection(io, service).await;
                });
            }
        });

        Ok(Server {
            addr,
            shutdown: shutdown_tx,
            _join: join,
        })
    }

    fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let (tx, _rx) = oneshot::channel::<()>();
        let old = std::mem::replace(&mut self.shutdown, tx);
        let _ = old.send(());
    }
}

fn error_response(status: u16) -> hyper::Response<RespBody> {
    let body: RespBody = http_body_util::Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed_unsync();
    hyper::Response::builder()
        .status(status)
        .body(body)
        .unwrap()
}

// ---------------------------------------------------------------------------
// Benchmark harness
// ---------------------------------------------------------------------------

struct Warm {
    server: Server,
    client: reqwest::Client,
}

async fn start_warm(flavor: Flavor) -> anyhow::Result<Warm> {
    let server = Server::start(flavor).await?;
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(64)
        .tcp_nodelay(true)
        .build()?;
    let warmup = client
        .get(format!("http://{}/", server.addr()))
        .send()
        .await?;
    anyhow::ensure!(
        warmup.status().is_success(),
        "warmup failed for {flavor:?}: {}",
        warmup.status()
    );
    let body = warmup.text().await?;
    anyhow::ensure!(
        body == flavor.expected_body(),
        "unexpected warmup body for {flavor:?}: {body:?}"
    );
    Ok(Warm { server, client })
}

async fn cold_once(flavor: Flavor) -> anyhow::Result<()> {
    let warm = start_warm(flavor).await?;
    drop(warm);
    Ok(())
}

async fn hot_once(warm: &Warm) -> anyhow::Result<()> {
    let resp = warm
        .client
        .get(format!("http://{}/", warm.server.addr()))
        .send()
        .await?;
    anyhow::ensure!(resp.status().is_success(), "non-2xx: {}", resp.status());
    let _ = resp.bytes().await?;
    Ok(())
}

fn bench_cold(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("serve_cold_invocation");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));

    for flavor in [Flavor::P2, Flavor::P3] {
        group.bench_function(BenchmarkId::from_parameter(flavor.name()), |b| {
            b.to_async(&rt)
                .iter(|| async move { cold_once(flavor).await.unwrap() });
        });
    }
    group.finish();
}

fn bench_hot(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("serve_hot_invocation");
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(10));

    for flavor in [Flavor::P2, Flavor::P3] {
        let warm = rt.block_on(start_warm(flavor)).expect("warm");
        group.bench_function(BenchmarkId::from_parameter(flavor.name()), |b| {
            b.to_async(&rt)
                .iter(|| async { hot_once(&warm).await.unwrap() });
        });
        drop(warm);
    }
    group.finish();
}

fn bench_throughput(c: &mut Criterion) {
    const CONCURRENCY: usize = 32;
    const BATCH: usize = 256;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let mut group = c.benchmark_group("serve_http_throughput");
    group.throughput(Throughput::Elements(BATCH as u64));
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(15));

    for flavor in [Flavor::P2, Flavor::P3] {
        let warm = rt.block_on(start_warm(flavor)).expect("warm");
        let url = format!("http://{}/", warm.server.addr());
        let client = warm.client.clone();

        let failures = Arc::new(AtomicUsize::new(0));
        let failures_ref = failures.clone();
        group.bench_function(BenchmarkId::from_parameter(flavor.name()), |b| {
            b.to_async(&rt).iter_custom(|iters| {
                let url = url.clone();
                let client = client.clone();
                let failures = failures_ref.clone();
                async move {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let start = Instant::now();
                        let mut handles = Vec::with_capacity(CONCURRENCY);
                        let per_worker = BATCH / CONCURRENCY;
                        for _ in 0..CONCURRENCY {
                            let client = client.clone();
                            let url = url.clone();
                            let failures = failures.clone();
                            handles.push(tokio::spawn(async move {
                                for _ in 0..per_worker {
                                    match client.get(&url).send().await {
                                        Ok(resp) if resp.status().is_success() => {
                                            let _ = resp.bytes().await;
                                        }
                                        _ => {
                                            failures.fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                }
                            }));
                        }
                        for h in handles {
                            h.await.ok();
                        }
                        total += start.elapsed();
                    }
                    total
                }
            });
        });
        let failed = failures.load(Ordering::Relaxed);
        if failed > 0 {
            eprintln!(
                "[serve_http_throughput/{}] {failed} requests failed during bench run",
                flavor.name()
            );
        }
        drop(warm);
    }
    group.finish();
}

criterion_group!(benches, bench_cold, bench_hot, bench_throughput);
criterion_main!(benches);
