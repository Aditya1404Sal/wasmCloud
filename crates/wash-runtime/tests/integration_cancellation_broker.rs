//! Integration tests for the `betty-blocks:cancellation-broker/broker` plugin.
//!
//! Setup (two P3 components on *two separate hosts*, one NATS):
//!   - `cancel-broker-worker` runs on host A. `GET /run?plan=<id>` streams a
//!     ten-step job, one step per second, racing each step against the host's
//!     `wait-cancel(<id>)`.
//!   - `cancel-broker-commander` runs on host B. `GET /cancel?plan=<id>` sets
//!     the flag; `GET /start?plan=<id>` clears it.
//!
//! The two hosts share nothing but the NATS JetStream KV bucket, which is the
//! point: the plugin's claim is that a cancel issued on one host reaches an
//! agent running on another. An in-process registry would pass a single-host
//! test and fail every one of these.
//!
//! Covered:
//!   - baseline: an uncancelled plan runs to completion;
//!   - a cancel from the *other host* truncates the job mid-flight;
//!   - a cancel set *before* the job starts is still observed (the plugin
//!     subscribes then reads, so it cannot be missed);
//!   - clearing the flag lets a plan id be reused;
//!   - a cancel for a different plan id leaves the job alone.
//!
//! Requires Docker for the NATS container, which must run with JetStream (`-js`)
//! enabled — the plugin's KV bucket does not exist without it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use anyhow::{Context, Result};
use futures::StreamExt;
use std::time::Duration;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio::time::timeout;

use wash_runtime::{host::HostApi, types::LocalResources};

mod common;
use common::{
    component_workload_request, http_cancellation_broker_host_interfaces,
    start_host_with_p3_cancellation_broker,
};

const CANCEL_BROKER_WORKER_WASM: &[u8] = include_bytes!("wasm/cancel_broker_worker.wasm");
const CANCEL_BROKER_COMMANDER_WASM: &[u8] = include_bytes!("wasm/cancel_broker_commander.wasm");

const WORKER_HOST: &str = "cb-worker";
const COMMANDER_HOST: &str = "cb-commander";

/// The worker emits one step per second for ten steps.
const EXPECTED_FULL: &[&str] = &["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"];

/// Fire the mid-flight cancel between two ticks rather than on one: steps land
/// at whole seconds, so 3.5s leaves ~500ms of slack on either side before the
/// observed count would shift.
const CANCEL_AFTER: Duration = Duration::from_millis(3500);
/// The step count 3.5s of work should produce, give or take scheduler jitter.
const EXPECTED_AT_CANCEL: std::ops::RangeInclusive<usize> = 2..=4;

/// Block until the container's published port actually accepts NATS clients.
///
/// `start()` resolves when the server logs "Server is ready", but the host-side
/// port forward can be published a moment later — and when several tests start
/// containers at once, that gap is wide enough that a plain `connect` fails
/// immediately with "connection refused". Retry instead of letting the port
/// forward's timing decide whether the suite passes.
async fn wait_for_nats(url: &str) -> Result<()> {
    const READY_TIMEOUT: Duration = Duration::from_secs(30);
    timeout(READY_TIMEOUT, async {
        while async_nats::connect(url).await.is_err() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .with_context(|| format!("NATS at {url} not accepting connections after {READY_TIMEOUT:?}"))
}

struct Harness {
    worker_addr: std::net::SocketAddr,
    commander_addr: std::net::SocketAddr,
    client: reqwest::Client,
    _worker_host: Box<dyn std::any::Any + Send>,
    _commander_host: Box<dyn std::any::Any + Send>,
    _container: Box<dyn std::any::Any + Send>,
}

/// Start one JetStream-enabled NATS, then two independent hosts against it:
/// the worker on one, the commander on the other.
async fn setup() -> Result<Harness> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .ok();

    // `-js` is required: the plugin stores plan flags in a JetStream KV bucket,
    // and bucket creation fails outright on a core-NATS-only server.
    let container = GenericImage::new("nats", "2.12.8-alpine")
        .with_exposed_port(4222.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Server is ready"))
        .with_cmd(["-js"])
        .start()
        .await
        .map_err(|e| anyhow::anyhow!("failed to start NATS container: {e}"))?;

    let port = container
        .get_host_port_ipv4(4222)
        .await
        .map_err(|e| anyhow::anyhow!("failed to get NATS host port: {e}"))?;
    let nats_url = format!("nats://127.0.0.1:{port}");
    wait_for_nats(&nats_url).await?;

    let (worker_addr, worker_host) =
        start_host_with_p3_cancellation_broker("127.0.0.1:0", &nats_url).await?;
    worker_host
        .workload_start(component_workload_request(
            "cancel-broker-worker",
            "cancellation-broker-worker",
            CANCEL_BROKER_WORKER_WASM,
            LocalResources::default(),
            http_cancellation_broker_host_interfaces(WORKER_HOST),
        ))
        .await
        .context("worker workload should start")?;

    let (commander_addr, commander_host) =
        start_host_with_p3_cancellation_broker("127.0.0.1:0", &nats_url).await?;
    commander_host
        .workload_start(component_workload_request(
            "cancel-broker-commander",
            "cancellation-broker-commander",
            CANCEL_BROKER_COMMANDER_WASM,
            LocalResources::default(),
            http_cancellation_broker_host_interfaces(COMMANDER_HOST),
        ))
        .await
        .context("commander workload should start")?;

    Ok(Harness {
        worker_addr,
        commander_addr,
        client: reqwest::Client::new(),
        _worker_host: Box::new(worker_host),
        _commander_host: Box::new(commander_host),
        _container: Box::new(container),
    })
}

impl Harness {
    /// Start a job on the worker host and return its streaming response.
    async fn run_plan(&self, plan: &str) -> Result<reqwest::Response> {
        let response = timeout(
            Duration::from_secs(20),
            self.client
                .get(format!("http://{}/run?plan={plan}", self.worker_addr))
                .header("HOST", WORKER_HOST)
                .send(),
        )
        .await
        .context("run request timed out")?
        .context("run request failed")?;
        anyhow::ensure!(
            response.status().is_success(),
            "worker should return 2xx, got {}",
            response.status()
        );
        Ok(response)
    }

    /// Hit the commander host (`/cancel` or `/start`) and return its body.
    async fn command(&self, route: &str, plan: &str) -> Result<String> {
        let response = timeout(
            Duration::from_secs(20),
            self.client
                .get(format!(
                    "http://{}/{route}?plan={plan}",
                    self.commander_addr
                ))
                .header("HOST", COMMANDER_HOST)
                .send(),
        )
        .await
        .context("commander request timed out")?
        .context("commander request failed")?
        .error_for_status()
        .context("commander returned non-2xx")?;
        response
            .text()
            .await
            .context("reading commander response failed")
    }
}

/// Drain a streaming body to text, returning whatever arrived before EOF.
async fn collect_body(response: reqwest::Response) -> Result<String> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    loop {
        match timeout(Duration::from_secs(30), stream.next()).await {
            Ok(None) => break,
            Ok(Some(Ok(chunk))) => body.extend_from_slice(&chunk),
            Ok(Some(Err(e))) => anyhow::bail!("body stream ended abruptly instead of at EOF: {e}"),
            Err(_) => anyhow::bail!("timed out waiting for a body chunk"),
        }
    }
    String::from_utf8(body).context("streamed body was not valid utf8")
}

/// Split a worker body into its numbered steps and whether it ended cancelled.
fn parse_steps(body: &str) -> (Vec<&str>, bool) {
    let lines: Vec<&str> = body.lines().filter(|l| !l.is_empty()).collect();
    match lines.split_last() {
        Some((&"cancelled", steps)) => (steps.to_vec(), true),
        _ => (lines, false),
    }
}

/// Baseline: nobody cancels, so the plan runs all ten steps to completion.
/// Without this, a broker that cancelled everything would pass the tests below.
#[tokio::test]
async fn test_uncancelled_plan_runs_to_completion() -> Result<()> {
    let harness = setup().await?;

    let body = collect_body(harness.run_plan("baseline").await?).await?;
    let (steps, cancelled) = parse_steps(&body);

    assert!(!cancelled, "an untouched plan should not report cancelled");
    assert_eq!(
        steps, EXPECTED_FULL,
        "an untouched plan should stream all ten steps"
    );
    Ok(())
}

/// The headline: a cancel issued by a component on a *different host* stops the
/// worker mid-job. The only path between the two is the JetStream KV bucket.
#[tokio::test]
async fn test_cancel_from_another_host_stops_worker_midway() -> Result<()> {
    const PLAN: &str = "cross-host";
    let harness = setup().await?;

    let response = harness.run_plan(PLAN).await?;

    // Cancel partway in, from the other host, while the body is still streaming.
    tokio::time::sleep(CANCEL_AFTER).await;
    let ack = harness.command("cancel", PLAN).await?;
    assert_eq!(ack.trim(), "cancel-set", "commander should set the flag");

    let body = collect_body(response).await?;
    let (steps, cancelled) = parse_steps(&body);

    assert!(
        cancelled,
        "the worker should observe the other host's cancel, but the job ended normally with steps {steps:?}"
    );
    assert_ne!(
        steps, EXPECTED_FULL,
        "a cancelled job must not deliver all ten steps"
    );
    assert!(
        EXPECTED_AT_CANCEL.contains(&steps.len()),
        "a cancel at {CANCEL_AFTER:?} should land around step 3, got {steps:?}"
    );
    assert_eq!(
        Some(steps.as_slice()),
        EXPECTED_FULL.get(..steps.len()),
        "the truncated job should be an in-order prefix of the full run"
    );
    Ok(())
}

/// A cancel set *before* the job starts must still be seen. This is the
/// subscribe race the plugin closes by watching the key first and only then
/// reading its current value: a watch-only implementation would hang here until
/// the job ran to completion, since no further update is ever published.
#[tokio::test]
async fn test_cancel_set_before_run_is_observed_immediately() -> Result<()> {
    const PLAN: &str = "pre-cancelled";
    let harness = setup().await?;

    let ack = harness.command("cancel", PLAN).await?;
    assert_eq!(ack.trim(), "cancel-set");

    let body = collect_body(harness.run_plan(PLAN).await?).await?;
    let (steps, cancelled) = parse_steps(&body);

    assert!(
        cancelled,
        "a plan cancelled before it started should report cancelled"
    );
    assert!(
        steps.is_empty(),
        "an already-cancelled plan should stop before its first step, got {steps:?}"
    );
    Ok(())
}

/// `set-cancel(id, false)` clears the flag, so an id used by a cancelled run is
/// reusable — the flag is per-plan state, not a permanent tombstone.
#[tokio::test]
async fn test_cleared_flag_allows_plan_id_reuse() -> Result<()> {
    const PLAN: &str = "reused";
    let harness = setup().await?;

    // Cancel the id, and confirm a run under it is indeed stopped.
    harness.command("cancel", PLAN).await?;
    let cancelled_body = collect_body(harness.run_plan(PLAN).await?).await?;
    let (_, cancelled) = parse_steps(&cancelled_body);
    assert!(cancelled, "the plan should be cancelled before it is reset");

    // Clear it, then reuse the same id for a fresh run.
    let ack = harness.command("start", PLAN).await?;
    assert_eq!(ack.trim(), "cleared", "commander should clear the flag");

    let body = collect_body(harness.run_plan(PLAN).await?).await?;
    let (steps, cancelled) = parse_steps(&body);
    assert!(
        !cancelled,
        "a cleared plan id should run normally, but it reported cancelled"
    );
    assert_eq!(
        steps, EXPECTED_FULL,
        "a cleared plan id should stream all ten steps"
    );
    Ok(())
}

/// Cancellation is scoped to its plan id: cancelling one plan leaves another
/// running. A bucket-wide or key-agnostic watch would fail this.
#[tokio::test]
async fn test_cancel_for_different_plan_does_not_affect_worker() -> Result<()> {
    const RUNNING_PLAN: &str = "alpha";
    const OTHER_PLAN: &str = "beta";
    let harness = setup().await?;

    let response = harness.run_plan(RUNNING_PLAN).await?;

    tokio::time::sleep(CANCEL_AFTER).await;
    harness.command("cancel", OTHER_PLAN).await?;

    let body = collect_body(response).await?;
    let (steps, cancelled) = parse_steps(&body);

    assert!(
        !cancelled,
        "cancelling {OTHER_PLAN} must not cancel {RUNNING_PLAN}"
    );
    assert_eq!(
        steps, EXPECTED_FULL,
        "a plan cancelled by a different id should stream all ten steps"
    );
    Ok(())
}
