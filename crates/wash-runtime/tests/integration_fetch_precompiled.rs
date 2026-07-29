use anyhow::{Context, Result};
use async_nats::jetstream::object_store;
use std::collections::HashMap;
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

use wash_runtime::{
    engine::Engine,
    fetch_precompiled,
    host::{HostApi, HostBuilder},
    types::{Component, Source, Workload, WorkloadStartRequest, WorkloadState},
};

#[tokio::test]
#[ignore = "network: requires Docker for NATS container"]
async fn workload_starts_with_file_backed_precompiled_component() -> Result<()> {
    let (_container, nats_url) = start_nats().await?;

    let cwasm = precompile_minimal_component()?;
    let bucket = "test-precompiled-file";
    let key = "minimal.cwasm";
    push_to_nats_bucket(&nats_url, bucket, key, &cwasm).await?;

    // Resolve the URL to a host-owned file in a compiled cache dir. This writes the
    // `.cwasm` once; the engine maps it file-backed via `deserialize_file`.
    let cache_dir = tempfile::tempdir()?;
    let url = format!("nats://{bucket}/{key}");
    set_nats_url(&nats_url);
    let path = fetch_precompiled::download_cwasm(&url, cache_dir.path()).await?;
    assert!(path.exists(), "download_cwasm must write a .cwasm file");
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("cwasm"));

    let engine = Engine::builder().build()?;
    let host = HostBuilder::new().with_engine(engine).build()?;
    let host = host.start().await.context("Failed to start host")?;

    let req = WorkloadStartRequest {
        workload_id: uuid::Uuid::new_v4().to_string(),
        workload: Workload {
            namespace: "test".to_string(),
            name: "precompiled-file-workload".to_string(),
            annotations: HashMap::new(),
            service: None,
            components: vec![Component {
                name: "precompiled-component".to_string(),
                digest: None,
                local_resources: Default::default(),
                max_invocations: 1,
                pool_size: 0,
                source: Source::PrecompiledFile(path.clone()),
            }],
            host_interfaces: vec![],
            volumes: vec![],
        },
    };

    // workload_start returns Ok even when a component fails to load (the failure lands
    // in the status), so assert on the reported state.
    let resp = host
        .workload_start(req)
        .await
        .context("workload_start errored for file-backed precompiled component")?;
    assert_eq!(
        resp.workload_status.workload_state,
        WorkloadState::Running,
        "file-backed precompiled component failed to load: {}",
        resp.workload_status.message
    );

    // The host-owned file must still be present since it backs the live mapping.
    assert!(
        path.exists(),
        "the .cwasm mapping file must persist while in use"
    );

    Ok(())
}

fn set_nats_url(nats_url: &str) {
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("NATS_URL", nats_url);
    }
}

async fn start_nats() -> Result<(ContainerAsync<GenericImage>, String)> {
    let container = GenericImage::new("nats", "2-alpine")
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

    Ok((container, nats_url))
}

fn precompile_minimal_component() -> Result<Vec<u8>> {
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    let engine = wasmtime::Engine::new(&config)?;
    let wasm = wat::parse_str("(component)")?;
    let cwasm = engine.precompile_component(&wasm)?;
    Ok(cwasm)
}

async fn push_to_nats_bucket(nats_url: &str, bucket: &str, key: &str, bytes: &[u8]) -> Result<()> {
    let client = async_nats::connect(nats_url)
        .await
        .with_context(|| format!("failed to connect to NATS at {nats_url}"))?;
    let jetstream = async_nats::jetstream::new(client);
    let store = jetstream
        .create_object_store(object_store::Config {
            bucket: bucket.to_string(),
            ..Default::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("failed to create object store '{bucket}': {e}"))?;

    let mut reader: &[u8] = bytes;
    store
        .put(key, &mut reader)
        .await
        .map_err(|e| anyhow::anyhow!("failed to put object '{key}' in '{bucket}': {e}"))?;
    Ok(())
}
