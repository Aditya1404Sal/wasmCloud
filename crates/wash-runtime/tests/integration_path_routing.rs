//! Integration tests for HTTP path-based routing with PathRouter
//!
//! This module tests:
//! 1. Multiple **workloads** routed by path prefixes (`/user/*` → user workload, `/admin` → admin workload)
//! 2. Multiple **components** within a single workload routed by different path prefixes
//! 3. Dynamic route registration/unregistration on workload bind/unbind
//! 4. Workload-level and component-level routing via `path` config in `wasi:http/incoming-handler`

use anyhow::{Context, Result};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::time::timeout;

use wash_runtime::{
    engine::Engine,
    host::{
        HostApi, HostBuilder,
        http::{HttpServer, PathRouter},
    },
    types::{Component, LocalResources, Workload, WorkloadStartRequest, WorkloadStopRequest},
    wit::WitInterface,
};

const USER_COMPONENT: &[u8] = include_bytes!("wasm/user_component.wasm");
const ADMIN_COMPONENT: &[u8] = include_bytes!("wasm/admin_component.wasm");

async fn start_path_router_host() -> Result<(impl HostApi, SocketAddr)> {
    let engine = Engine::builder().build()?;
    let http_plugin = HttpServer::new(PathRouter::default(), "127.0.0.1:0".parse()?).await?;
    let addr = http_plugin.addr();

    let host = HostBuilder::new()
        .with_engine(engine)
        .with_http_handler(Arc::new(http_plugin))
        .build()?;

    let host = host.start().await.context("failed to start host")?;
    Ok((host, addr))
}

#[tokio::test]
async fn test_path_based_routing() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .ok();

    let (host, addr) = start_path_router_host().await?;
    println!("Host started, HTTP server listening on {addr}");

    let user_workload_id = uuid::Uuid::new_v4().to_string();
    let user_req = WorkloadStartRequest {
        workload_id: user_workload_id.clone(),
        workload: Workload {
            namespace: "test".to_string(),
            name: "user-workload".to_string(),
            annotations: HashMap::new(),
            service: None,
            components: vec![Component {
                name: "user-component".to_string(),
                bytes: bytes::Bytes::from_static(USER_COMPONENT),
                digest: None,
                local_resources: LocalResources {
                    memory_limit_mb: 256,
                    cpu_limit: 1,
                    config: HashMap::new(),
                    environment: HashMap::new(),
                    volume_mounts: vec![],
                    allowed_hosts: vec![].into(),
                },
                pool_size: 1,
                max_invocations: 100,
                interface_config: HashMap::new(),
            }],
            host_interfaces: vec![WitInterface {
                namespace: "wasi".to_string(),
                package: "http".to_string(),
                interfaces: ["incoming-handler".to_string()].into_iter().collect(),
                version: None,
                name: None,
                config: {
                    let mut config = HashMap::new();
                    config.insert("path".to_string(), "/user/{server-id}".to_string()); // Similar pattern to be used for MCP server-id based routing
                    config
                },
            }],
            volumes: vec![],
        },
    };

    let admin_workload_id = uuid::Uuid::new_v4().to_string();
    let admin_req = WorkloadStartRequest {
        workload_id: admin_workload_id.clone(),
        workload: Workload {
            namespace: "test".to_string(),
            name: "admin-workload".to_string(),
            annotations: HashMap::new(),
            service: None,
            components: vec![Component {
                name: "admin-component".to_string(),
                bytes: bytes::Bytes::from_static(ADMIN_COMPONENT),
                digest: None,
                local_resources: LocalResources {
                    memory_limit_mb: 256,
                    cpu_limit: 1,
                    config: HashMap::new(),
                    environment: HashMap::new(),
                    volume_mounts: vec![],
                    allowed_hosts: vec![].into(),
                },
                pool_size: 1,
                max_invocations: 100,
                interface_config: HashMap::new(),
            }],
            host_interfaces: vec![WitInterface {
                namespace: "wasi".to_string(),
                package: "http".to_string(),
                interfaces: ["incoming-handler".to_string()].into_iter().collect(),
                version: None,
                name: None,
                config: {
                    let mut config = HashMap::new();
                    config.insert("path".to_string(), "/admin".to_string());
                    config
                },
            }],
            volumes: vec![],
        },
    };

    let _user_start_response = host
        .workload_start(user_req)
        .await
        .context("Failed to start user workload")?;
    println!("Started USER workload at /user");

    let _admin_start_response = host
        .workload_start(admin_req)
        .await
        .context("Failed to start admin workload")?;
    println!("Started Admin workload at /admin");

    let client = reqwest::Client::new();

    let user_id = uuid::Uuid::new_v4().to_string();

    println!("Testing /user/{user_id} endpoint");
    let api_response = timeout(
        Duration::from_secs(5),
        client
            .get(format!("http://{addr}/user/{user_id}"))
            .header("HOST", "localhost")
            .send(),
    )
    .await
    .context("USER request timed out")?
    .context("Failed to make USER request")?;

    assert!(
        api_response.status().is_success(),
        "Expected success for /user/{} got {}",
        user_id,
        api_response.status()
    );
    let api_body = api_response.text().await?;
    assert!(
        api_body.contains(&format!("USER: {}", user_id)),
        "Expected 'USER: {}' in response, got: {}",
        user_id,
        api_body
    );
    println!(
        "✓ /user/{user_id} responded with correct body: {}",
        api_body.trim()
    );

    println!("Testing /admin endpoint");
    let admin_response = timeout(
        Duration::from_secs(5),
        client
            .get(format!("http://{addr}/admin"))
            .header("HOST", "localhost")
            .send(),
    )
    .await
    .context("Admin request timed out")?
    .context("Failed to make Admin request")?;

    assert!(
        admin_response.status().is_success(),
        "Expected success for /admin, got {}",
        admin_response.status()
    );
    let admin_body = admin_response.text().await?;
    assert!(
        admin_body.contains("ADMIN"),
        "Expected 'ADMIN' in response, got: {}",
        admin_body
    );
    println!(
        "✓ /admin responded with correct body: {}",
        admin_body.trim()
    );

    host.workload_stop(WorkloadStopRequest {
        workload_id: user_workload_id,
    })
    .await
    .context("Failed to stop api workload")?;

    host.workload_stop(WorkloadStopRequest {
        workload_id: admin_workload_id,
    })
    .await
    .context("Failed to stop admin workload")?;

    println!("Path-based routing test passed!");
    Ok(())
}

/// Tests component-level path routing: two components in ONE workload, each
/// handling a different path prefix.
#[tokio::test]
async fn test_component_level_path_routing() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .ok();

    let (host, addr) = start_path_router_host().await?;
    println!("Host started, HTTP server listening on {addr}");

    // Single workload with TWO components, each declaring its own path via
    // per-component interface_config
    let workload_id = uuid::Uuid::new_v4().to_string();
    let req = WorkloadStartRequest {
        workload_id: workload_id.clone(),
        workload: Workload {
            namespace: "test".to_string(),
            name: "multi-component-workload".to_string(),
            annotations: HashMap::new(),
            service: None,
            components: vec![
                Component {
                    name: "user-component".to_string(),
                    bytes: bytes::Bytes::from_static(USER_COMPONENT),
                    digest: None,
                    local_resources: LocalResources {
                        memory_limit_mb: 256,
                        cpu_limit: 1,
                        config: HashMap::new(),
                        environment: HashMap::new(),
                        volume_mounts: vec![],
                        allowed_hosts: vec![].into(),
                    },
                    pool_size: 1,
                    max_invocations: 100,
                    interface_config: HashMap::from([(
                        "wasi:http/incoming-handler".to_string(),
                        HashMap::from([("path".to_string(), "/user/{id}".to_string())]),
                    )]),
                },
                Component {
                    name: "admin-component".to_string(),
                    bytes: bytes::Bytes::from_static(ADMIN_COMPONENT),
                    digest: None,
                    local_resources: LocalResources {
                        memory_limit_mb: 256,
                        cpu_limit: 1,
                        config: HashMap::new(),
                        environment: HashMap::new(),
                        volume_mounts: vec![],
                        allowed_hosts: vec![].into(),
                    },
                    pool_size: 1,
                    max_invocations: 100,
                    interface_config: HashMap::from([(
                        "wasi:http/incoming-handler".to_string(),
                        HashMap::from([("path".to_string(), "/admin".to_string())]),
                    )]),
                },
            ],
            // Workload-level host_interfaces declares that HTTP is needed,
            // but path config is per-component above
            host_interfaces: vec![WitInterface {
                namespace: "wasi".to_string(),
                package: "http".to_string(),
                interfaces: ["incoming-handler".to_string()].into_iter().collect(),
                version: None,
                name: None,
                config: HashMap::new(),
            }],
            volumes: vec![],
        },
    };

    let _start_response = host
        .workload_start(req)
        .await
        .context("Failed to start multi-component workload")?;
    println!("Started multi-component workload with /user/{{id}} and /admin paths");

    let client = reqwest::Client::new();

    // Test /user/{id} hits the USER component
    let user_id = uuid::Uuid::new_v4().to_string();
    println!("Testing /user/{user_id} endpoint");
    let user_response = timeout(
        Duration::from_secs(5),
        client
            .get(format!("http://{addr}/user/{user_id}"))
            .header("HOST", "localhost")
            .send(),
    )
    .await
    .context("USER request timed out")?
    .context("Failed to make USER request")?;

    assert!(
        user_response.status().is_success(),
        "Expected success for /user/{}, got {}",
        user_id,
        user_response.status()
    );
    let user_body = user_response.text().await?;
    assert!(
        user_body.contains(&format!("USER: {}", user_id)),
        "Expected 'USER: {}' in response, got: {}",
        user_id,
        user_body
    );
    println!(
        "  /user/{user_id} responded correctly: {}",
        user_body.trim()
    );

    // Test /admin hits the ADMIN component
    println!("Testing /admin endpoint");
    let admin_response = timeout(
        Duration::from_secs(5),
        client
            .get(format!("http://{addr}/admin"))
            .header("HOST", "localhost")
            .send(),
    )
    .await
    .context("Admin request timed out")?
    .context("Failed to make Admin request")?;

    assert!(
        admin_response.status().is_success(),
        "Expected success for /admin, got {}",
        admin_response.status()
    );
    let admin_body = admin_response.text().await?;
    assert!(
        admin_body.contains("ADMIN"),
        "Expected 'ADMIN' in response, got: {}",
        admin_body
    );
    println!("  /admin responded correctly: {}", admin_body.trim());

    host.workload_stop(WorkloadStopRequest { workload_id })
        .await
        .context("Failed to stop multi-component workload")?;

    println!("Component-level path routing test passed!");
    Ok(())
}
