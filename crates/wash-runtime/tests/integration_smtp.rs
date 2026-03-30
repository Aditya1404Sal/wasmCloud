use anyhow::{Context, Result};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use testcontainers::{GenericImage, core::IntoContainerPort, core::WaitFor, runners::AsyncRunner};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use wash_runtime::{
    engine::Engine,
    host::{
        HostApi, HostBuilder,
        http::{DevRouter, HttpServer},
    },
    plugin::{smtp::BettySmtp, wasi_config::DynamicConfig, wasi_logging::TracingLogger},
    types::{Component, LocalResources, Workload, WorkloadStartRequest},
    wit::WitInterface,
};

const SMTP_DEMO_WASM: &[u8] = include_bytes!("wasm/smtp_demo.wasm");

async fn find_available_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    Ok(listener.local_addr()?.port())
}
const SMTP4DEV_SMTP_PORT: u16 = 25;
const SMTP4DEV_API_PORT: u16 = 80;

fn create_workload_request(name: &str, host_header: &str) -> WorkloadStartRequest {
    WorkloadStartRequest {
        workload_id: uuid::Uuid::new_v4().to_string(),
        workload: Workload {
            namespace: "test".to_string(),
            name: name.to_string(),
            annotations: HashMap::new(),
            service: None,
            components: vec![Component {
                name: "smtp-demo-component".to_string(),
                digest: None,
                bytes: bytes::Bytes::from_static(SMTP_DEMO_WASM),
                local_resources: LocalResources {
                    memory_limit_mb: 256,
                    cpu_limit: 1,
                    config: HashMap::new(),
                    environment: HashMap::new(),
                    volume_mounts: vec![],
                    allowed_hosts: Default::default(),
                },
                pool_size: 1,
                max_invocations: 100,
            }],
            host_interfaces: vec![
                WitInterface {
                    namespace: "wasi".to_string(),
                    package: "http".to_string(),
                    interfaces: ["incoming-handler".to_string()].into_iter().collect(),
                    version: Some(semver::Version::parse("0.2.2").unwrap()),
                    config: {
                        let mut config = HashMap::new();
                        config.insert("host".to_string(), host_header.to_string());
                        config
                    },
                    name: None,
                },
                WitInterface {
                    namespace: "wasi".to_string(),
                    package: "http".to_string(),
                    interfaces: ["outgoing-handler".to_string()].into_iter().collect(),
                    version: Some(semver::Version::parse("0.2.2").unwrap()),
                    config: HashMap::new(),
                    name: None,
                },
                WitInterface {
                    namespace: "betty-blocks".to_string(),
                    package: "smtp".to_string(),
                    interfaces: ["client".to_string()].into_iter().collect(),
                    version: None,
                    config: HashMap::new(),
                    name: None,
                },
                WitInterface {
                    namespace: "wasi".to_string(),
                    package: "logging".to_string(),
                    interfaces: ["logging".to_string()].into_iter().collect(),
                    version: Some(semver::Version::parse("0.1.0-draft").unwrap()),
                    config: HashMap::new(),
                    name: None,
                },
                WitInterface {
                    namespace: "wasi".to_string(),
                    package: "config".to_string(),
                    interfaces: ["store".to_string()].into_iter().collect(),
                    version: Some(semver::Version::parse("0.2.0-rc.1").unwrap()),
                    config: HashMap::new(),
                    name: None,
                },
            ],
            volumes: vec![],
        },
    }
}

async fn setup_test_host(
    host_header: &str,
    workload_name: &str,
) -> Result<(SocketAddr, impl std::any::Any)> {
    let engine = Engine::builder().build()?;
    let http_plugin = HttpServer::new(DevRouter::default(), "127.0.0.1:0".parse()?).await?;
    let http_addr = http_plugin.addr();

    let host = HostBuilder::new()
        .with_engine(engine.clone())
        .with_http_handler(Arc::new(http_plugin))
        .with_plugin(Arc::new(BettySmtp::new()))?
        .with_plugin(Arc::new(TracingLogger::default()))?
        .with_plugin(Arc::new(DynamicConfig::default()))?
        .build()?;

    let host = host.start().await.context("failed to start host")?;

    let req = create_workload_request(workload_name, host_header);
    host.workload_start(req)
        .await
        .context("failed to start workload")?;

    Ok((http_addr, host))
}

async fn send_email(
    client: &reqwest::Client,
    http_addr: SocketAddr,
    host_header: &str,
    payload: &serde_json::Value,
    timeout_secs: u64,
) -> Result<(reqwest::StatusCode, String)> {
    let response = timeout(
        Duration::from_secs(timeout_secs),
        client
            .post(format!("http://{http_addr}/"))
            .header("HOST", host_header)
            .header("Content-Type", "application/json")
            .json(payload)
            .send(),
    )
    .await
    .context("request timed out")?
    .context("request failed")?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    Ok((status, text))
}

fn smtp_payload(
    smtp_host: &str,
    smtp_port: u16,
    from: &str,
    to: &[&str],
    subject: &str,
    body: &str,
) -> serde_json::Value {
    serde_json::json!({
        "smtp": { "host": smtp_host, "port": smtp_port, "tls_mode": "none" },
        "from": from,
        "to": to,
        "subject": subject,
        "body": body,
    })
}

struct SmtpServer {
    smtp_host: String,
    smtp_port: u16,
    api_port: u16,
    _container: testcontainers::ContainerAsync<GenericImage>,
}

async fn start_smtp4dev() -> Result<SmtpServer> {
    let container = GenericImage::new("rnwood/smtp4dev", "latest")
        .with_exposed_port(SMTP4DEV_SMTP_PORT.tcp())
        .with_exposed_port(SMTP4DEV_API_PORT.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Now listening on"))
        .start()
        .await
        .context("failed to start smtp4dev container")?;

    let smtp_port = container
        .get_host_port_ipv4(SMTP4DEV_SMTP_PORT)
        .await
        .context("failed to get smtp4dev SMTP port")?;
    let api_port = container
        .get_host_port_ipv4(SMTP4DEV_API_PORT)
        .await
        .context("failed to get smtp4dev API port")?;

    Ok(SmtpServer {
        smtp_host: "127.0.0.1".to_string(),
        smtp_port,
        api_port,
        _container: container,
    })
}

async fn clear_smtp4dev_messages(api_port: u16) -> Result<()> {
    reqwest::Client::new()
        .delete(format!("http://127.0.0.1:{api_port}/api/messages/*"))
        .send()
        .await
        .context("failed to clear smtp4dev messages")?;
    Ok(())
}

async fn get_smtp4dev_messages(api_port: u16) -> Result<serde_json::Value> {
    let resp = reqwest::get(format!("http://127.0.0.1:{api_port}/api/messages"))
        .await
        .context("failed to get smtp4dev messages")?;
    let json: serde_json::Value = resp
        .json()
        .await
        .context("failed to parse smtp4dev response")?;
    Ok(json)
}

async fn get_smtp4dev_message_plaintext(api_port: u16, message_id: &str) -> Result<String> {
    let resp = reqwest::get(format!(
        "http://127.0.0.1:{api_port}/api/messages/{message_id}/plaintext"
    ))
    .await
    .context("failed to get message plaintext")?;
    resp.text().await.context("failed to read plaintext body")
}

async fn get_smtp4dev_message_html(api_port: u16, message_id: &str) -> Result<String> {
    let resp = reqwest::get(format!(
        "http://127.0.0.1:{api_port}/api/messages/{message_id}/html"
    ))
    .await
    .context("failed to get message html")?;
    resp.text().await.context("failed to read html body")
}

async fn get_smtp4dev_first_message(api_port: u16) -> Result<(String, serde_json::Value)> {
    let messages = get_smtp4dev_messages(api_port).await?;
    let results = messages["results"]
        .as_array()
        .context("results should be array")?;
    anyhow::ensure!(!results.is_empty(), "no messages in smtp4dev");
    let msg = &results[0];
    let id = msg["id"]
        .as_str()
        .context("message should have id")?
        .to_string();
    Ok((id, msg.clone()))
}

async fn get_smtp4dev_message_count(api_port: u16) -> Result<usize> {
    let json = get_smtp4dev_messages(api_port).await?;
    let count = json["results"].as_array().map(|a| a.len()).unwrap_or(0);
    Ok(count)
}

async fn wait_for_smtp4dev_messages(
    api_port: u16,
    expected_count: usize,
    timeout_duration: Duration,
) -> Result<usize> {
    let start = tokio::time::Instant::now();
    let poll_interval = Duration::from_millis(100);

    loop {
        let count = get_smtp4dev_message_count(api_port).await?;
        if count >= expected_count {
            return Ok(count);
        }
        if start.elapsed() >= timeout_duration {
            anyhow::bail!(
                "Timed out waiting for {} messages in smtp4dev (got {})",
                expected_count,
                count
            );
        }
        tokio::time::sleep(poll_interval).await;
    }
}

#[tokio::test]
async fn smtp_sends_plaintext_email() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .ok();

    let server = start_smtp4dev().await?;
    clear_smtp4dev_messages(server.api_port).await?;

    let (http_addr, _host) = setup_test_host("smtp-test", "plaintext-test").await?;
    let client = reqwest::Client::new();

    let payload = smtp_payload(
        &server.smtp_host,
        server.smtp_port,
        "sender@test.local",
        &["recipient@test.local"],
        "Plaintext Email Test",
        "Hello, this is a plaintext email.",
    );
    let (status, _) = send_email(&client, http_addr, "smtp-test", &payload, 15).await?;

    assert!(
        status.is_success(),
        "plaintext email should succeed, got {status}"
    );

    let count = wait_for_smtp4dev_messages(server.api_port, 1, Duration::from_secs(5)).await?;
    assert_eq!(count, 1, "smtp4dev should have captured 1 email");

    let (message_id, summary) = get_smtp4dev_first_message(server.api_port).await?;

    assert_eq!(
        summary["subject"].as_str().unwrap_or(""),
        "Plaintext Email Test",
        "subject should match"
    );
    assert!(
        summary["from"]
            .as_str()
            .unwrap_or("")
            .contains("sender@test.local"),
        "from should contain sender address"
    );

    let body = get_smtp4dev_message_plaintext(server.api_port, &message_id).await?;
    assert!(
        body.contains("Hello, this is a plaintext email."),
        "email body should contain the sent text, got: {body}"
    );

    Ok(())
}

#[tokio::test]
async fn smtp_sends_html_email() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .ok();

    let server = start_smtp4dev().await?;
    clear_smtp4dev_messages(server.api_port).await?;

    let (http_addr, _host) = setup_test_host("smtp-html", "html-test").await?;
    let client = reqwest::Client::new();

    let payload = smtp_payload(
        &server.smtp_host,
        server.smtp_port,
        "html-sender@test.local",
        &["html-recipient@test.local"],
        "HTML Email Test",
        "<html><body><h1>HTML Email</h1><p>Sent via <strong>SMTP component</strong>.</p></body></html>",
    );
    let (status, _) = send_email(&client, http_addr, "smtp-html", &payload, 15).await?;

    assert!(
        status.is_success(),
        "HTML email should succeed, got {status}"
    );

    let count = wait_for_smtp4dev_messages(server.api_port, 1, Duration::from_secs(5)).await?;
    assert_eq!(count, 1, "smtp4dev should have captured 1 email");

    let (message_id, summary) = get_smtp4dev_first_message(server.api_port).await?;

    assert_eq!(
        summary["subject"].as_str().unwrap_or(""),
        "HTML Email Test",
        "subject should match"
    );
    assert!(
        summary["from"]
            .as_str()
            .unwrap_or("")
            .contains("html-sender@test.local"),
        "from should contain sender address"
    );

    let html = get_smtp4dev_message_html(server.api_port, &message_id).await?;
    assert!(
        html.contains("<h1>HTML Email</h1>"),
        "html body should contain the <h1> tag, got: {html}"
    );
    assert!(
        html.contains("<strong>SMTP component</strong>"),
        "html body should contain the <strong> tag, got: {html}"
    );

    let plaintext = get_smtp4dev_message_plaintext(server.api_port, &message_id).await?;
    assert!(
        plaintext.contains("SMTP component"),
        "plaintext fallback should contain 'SMTP component', got: {plaintext}"
    );

    Ok(())
}

#[tokio::test]
async fn smtp_sends_email_with_cc_and_bcc() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .ok();

    let server = start_smtp4dev().await?;
    clear_smtp4dev_messages(server.api_port).await?;

    let (http_addr, _host) = setup_test_host("smtp-cc", "cc-bcc-test").await?;
    let client = reqwest::Client::new();

    let payload = serde_json::json!({
        "smtp": { "host": server.smtp_host, "port": server.smtp_port, "tls_mode": "none" },
        "from": "sender@test.local",
        "to": ["primary@test.local"],
        "cc": ["cc1@test.local", "cc2@test.local"],
        "bcc": ["bcc@test.local"],
        "subject": "CC and BCC Test",
        "body": "This email has CC and BCC recipients."
    });
    let (status, _) = send_email(&client, http_addr, "smtp-cc", &payload, 15).await?;

    assert!(
        status.is_success(),
        "CC/BCC email should succeed, got {status}"
    );

    let count = wait_for_smtp4dev_messages(server.api_port, 1, Duration::from_secs(5)).await?;
    assert_eq!(count, 1, "smtp4dev should have captured 1 email");

    let (message_id, summary) = get_smtp4dev_first_message(server.api_port).await?;

    assert_eq!(
        summary["subject"].as_str().unwrap_or(""),
        "CC and BCC Test",
        "subject should match"
    );

    let plaintext = get_smtp4dev_message_plaintext(server.api_port, &message_id).await?;
    assert!(
        plaintext.contains("This email has CC and BCC recipients."),
        "body should contain expected text, got: {plaintext}"
    );

    Ok(())
}

#[tokio::test]
async fn smtp_sends_concurrent_emails() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .ok();

    let server = start_smtp4dev().await?;
    clear_smtp4dev_messages(server.api_port).await?;

    let (http_addr, _host) = setup_test_host("smtp-concurrent", "concurrent-test").await?;

    let mut handles = Vec::new();
    for i in 0..5 {
        let client = reqwest::Client::new();
        let addr = http_addr;
        let host = server.smtp_host.clone();
        let port = server.smtp_port;
        handles.push(tokio::spawn(async move {
            let payload = serde_json::json!({
                "smtp": { "host": host, "port": port, "tls_mode": "none" },
                "from": format!("concurrent-{i}@test.local"),
                "to": [format!("recipient-{i}@test.local")],
                "subject": format!("Concurrent Email #{}", i + 1),
                "body": format!("Concurrent email number {}.", i + 1)
            });
            send_email(&client, addr, "smtp-concurrent", &payload, 15)
                .await
                .map(|(s, _)| s.is_success())
                .unwrap_or(false)
        }));
    }

    let mut success_count = 0;
    for handle in handles {
        if handle.await.unwrap_or(false) {
            success_count += 1;
        }
    }

    assert_eq!(success_count, 5, "all 5 concurrent emails should succeed");

    let count = wait_for_smtp4dev_messages(server.api_port, 5, Duration::from_secs(5)).await?;
    assert_eq!(count, 5, "smtp4dev should have captured all 5 emails");

    let messages = get_smtp4dev_messages(server.api_port).await?;
    let results = messages["results"]
        .as_array()
        .expect("results should be array");
    let subjects: Vec<&str> = results
        .iter()
        .filter_map(|m| m["subject"].as_str())
        .collect();
    for i in 1..=5 {
        let expected = format!("Concurrent Email #{i}");
        assert!(
            subjects.contains(&expected.as_str()),
            "should find '{expected}' in smtp4dev, got subjects: {subjects:?}"
        );
    }

    Ok(())
}

#[tokio::test]
async fn smtp_sends_large_body_email() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .ok();

    let server = start_smtp4dev().await?;
    clear_smtp4dev_messages(server.api_port).await?;

    let (http_addr, _host) = setup_test_host("smtp-large", "large-body-test").await?;
    let client = reqwest::Client::new();

    let large_body = {
        let mut body = String::from("<html><body><h1>Large Email</h1>");
        for i in 0..500 {
            body.push_str(&format!(
                "<p>Paragraph {i}: Lorem ipsum dolor sit amet, consectetur adipiscing elit.</p>",
            ));
        }
        body.push_str("</body></html>");
        body
    };

    let payload = smtp_payload(
        &server.smtp_host,
        server.smtp_port,
        "large-sender@test.local",
        &["large-recipient@test.local"],
        "Large Body Email Test",
        &large_body,
    );
    let (status, _) = send_email(&client, http_addr, "smtp-large", &payload, 30).await?;

    assert!(
        status.is_success(),
        "large body email should succeed, got {status}"
    );

    let count = wait_for_smtp4dev_messages(server.api_port, 1, Duration::from_secs(5)).await?;
    assert_eq!(count, 1, "smtp4dev should have captured 1 email");

    let (message_id, summary) = get_smtp4dev_first_message(server.api_port).await?;

    assert_eq!(
        summary["subject"].as_str().unwrap_or(""),
        "Large Body Email Test",
        "subject should match"
    );

    let html = get_smtp4dev_message_html(server.api_port, &message_id).await?;
    assert!(
        html.contains("<h1>Large Email</h1>"),
        "html should contain the heading"
    );
    assert!(
        html.contains("Paragraph 0:") && html.contains("Paragraph 499:"),
        "html should contain first and last paragraphs"
    );

    Ok(())
}

#[tokio::test]
async fn smtp_returns_error_for_unreachable_server() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .ok();

    let (http_addr, _host) = setup_test_host("smtp-error", "error-test").await?;
    let client = reqwest::Client::new();

    let invalid_port = find_available_port().await?;
    let payload = smtp_payload(
        "127.0.0.1",
        invalid_port,
        "sender@test.local",
        &["recipient@test.local"],
        "Should Fail",
        "This email should fail because there is no SMTP server.",
    );
    let (status, _) = send_email(&client, http_addr, "smtp-error", &payload, 15).await?;

    assert!(
        status.as_u16() >= 400,
        "connection to non-existent server should return error status, got {status}"
    );

    Ok(())
}

#[tokio::test]
async fn smtp_reconnects_after_disconnect() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .ok();

    let server = start_smtp4dev().await?;
    clear_smtp4dev_messages(server.api_port).await?;

    let (http_addr, _host) = setup_test_host("smtp-reconn", "reconnect-test").await?;
    let client = reqwest::Client::new();

    let payload1 = smtp_payload(
        &server.smtp_host,
        server.smtp_port,
        "sender@test.local",
        &["recipient@test.local"],
        "First Email",
        "First email body.",
    );
    let (status1, _) = send_email(&client, http_addr, "smtp-reconn", &payload1, 15).await?;
    assert!(
        status1.is_success(),
        "first send should succeed, got {status1}"
    );

    let payload2 = smtp_payload(
        &server.smtp_host,
        server.smtp_port,
        "sender@test.local",
        &["recipient@test.local"],
        "Second Email",
        "Second email body.",
    );
    let (status2, _) = send_email(&client, http_addr, "smtp-reconn", &payload2, 15).await?;
    assert!(
        status2.is_success(),
        "second send should succeed after reconnect, got {status2}"
    );

    let count = wait_for_smtp4dev_messages(server.api_port, 2, Duration::from_secs(5)).await?;
    assert_eq!(count, 2, "smtp4dev should have captured 2 emails");

    let messages = get_smtp4dev_messages(server.api_port).await?;
    let results = messages["results"]
        .as_array()
        .expect("results should be array");
    let subjects: Vec<&str> = results
        .iter()
        .filter_map(|m| m["subject"].as_str())
        .collect();
    assert!(
        subjects.contains(&"First Email") && subjects.contains(&"Second Email"),
        "should find both email subjects, got: {subjects:?}"
    );

    Ok(())
}

#[tokio::test]
async fn smtp_sends_email_with_attachment() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .ok();

    let server = start_smtp4dev().await?;
    clear_smtp4dev_messages(server.api_port).await?;

    let attachment_port = find_available_port().await?;
    let attachment_bytes = b"Hello, this is a test attachment file content.";

    spawn_attachment_server(attachment_port, attachment_bytes).await?;

    let (http_addr, _host) = setup_test_host("smtp-attach", "attachment-test").await?;
    let client = reqwest::Client::new();

    let payload = serde_json::json!({
        "smtp": { "host": server.smtp_host, "port": server.smtp_port, "tls_mode": "none" },
        "from": "sender@test.local",
        "to": ["recipient@test.local"],
        "subject": "Attachment Test",
        "body": "This email has an attachment.",
        "attachments": [{
            "url": format!("http://127.0.0.1:{}/test.txt", attachment_port),
            "filename": "test.txt",
            "content_type": "text/plain"
        }]
    });

    let (status, _) = send_email(&client, http_addr, "smtp-attach", &payload, 30).await?;
    assert!(
        status.is_success(),
        "attachment email should succeed, got {status}"
    );

    let count = wait_for_smtp4dev_messages(server.api_port, 1, Duration::from_secs(5)).await?;
    assert_eq!(count, 1, "smtp4dev should have captured 1 email");

    let (message_id, summary) = get_smtp4dev_first_message(server.api_port).await?;

    assert_eq!(summary["subject"].as_str().unwrap_or(""), "Attachment Test");
    assert_eq!(summary["attachmentCount"].as_i64().unwrap_or(0), 1);

    let plaintext = get_smtp4dev_message_plaintext(server.api_port, &message_id).await?;
    assert!(
        plaintext.contains("This email has an attachment."),
        "body should contain expected text, got: {plaintext}"
    );

    Ok(())
}

async fn spawn_attachment_server(
    port: u16,
    attachment_bytes: &'static [u8],
) -> Result<JoinHandle<()>> {
    use tokio::io::AsyncWriteExt;

    let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).await?;

    let handle = tokio::spawn(async move {
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
                    attachment_bytes.len()
                );

                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.write_all(attachment_bytes).await;
            }
        }
    });

    Ok(handle)
}
