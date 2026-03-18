use anyhow::{Context, Result};
use serde::Deserialize;
use waki::Client;

mod bindings {
    wit_bindgen::generate!({
        generate_all,
    });
}

use bindings::{
    bettyblocks::smtp::client::{
        self, Attachment, Credentials, Message, Recipient, Sender, TlsMode,
    },
    exports::wasi::http::incoming_handler::Guest,
    wasi::{
        http::types::{Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam},
        io::streams::StreamError,
        logging::logging::{log, Level},
    },
};

struct Component;

impl Guest for Component {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        match handle_request(request) {
            Ok(message) => {
                log(Level::Info, "", &format!("{}", message));
                send_response(response_out, 200, message.as_bytes());
            }
            Err(e) => {
                log(Level::Error, "", &format!("Error: {e}"));
                let error_msg = format!("Failed to send email: {e}");
                send_response(response_out, 500, error_msg.as_bytes());
            }
        }
    }
}

fn handle_request(request: IncomingRequest) -> Result<String> {
    log(Level::Info, "", "Processing incoming SMTP request");

    let body_content = read_request_body(request)?;

    let smtp_config: SmtpConfig = serde_json::from_str(&body_content)
        .context("Request body must be valid JSON with SMTP configuration")?;

    let connection_key = connect_smtp(&smtp_config)?;

    let message = build_email_message(&smtp_config)?;

    let attachment_info = if message.attachments.is_some() {
        "with attachment"
    } else {
        "without attachment"
    };

    log(
        Level::Info,
        "",
        &format!("Sending: {} {}", message.subject, attachment_info),
    );

    let result =
        client::send(&connection_key, &message).map_err(|e| anyhow::anyhow!("Send failed: {e}"))?;

    if let Err(e) = client::disconnect(&connection_key) {
        log(
            Level::Warn,
            "",
            &format!("Failed to disconnect SMTP connection: {e}"),
        );
    }

    Ok(format!(
        "Email {} sent! Accepted: {}, Server: {}, ID: {}",
        attachment_info,
        result.accepted,
        result.server.unwrap_or_else(|| "unknown".to_string()),
        result.message_id.unwrap_or_else(|| "none".to_string())
    ))
}

#[derive(Clone, Deserialize)]
struct SmtpConfig {
    smtp: SmtpCredentials,
    from: String,
    to: FlexList,
    #[serde(default)]
    cc: Option<FlexList>,
    #[serde(default)]
    bcc: Option<FlexList>,
    #[serde(default = "default_subject")]
    subject: String,
    #[serde(default = "default_body")]
    body: String,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    attachments: Option<Vec<AttachmentConfig>>,
}

#[derive(Clone, Deserialize)]
struct SmtpCredentials {
    #[serde(default = "default_host")]
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    username: Option<String>,
    password: Option<String>,
    tls_mode: Option<String>,
}

#[derive(Clone, Deserialize)]
struct AttachmentConfig {
    url: String,
    filename: Option<String>,
    content_type: Option<String>,
}

/// Accepts either a single string or a list of strings in JSON.
#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum FlexList {
    Single(String),
    Multiple(Vec<String>),
}

impl FlexList {
    fn into_vec(self) -> Vec<String> {
        match self {
            FlexList::Single(s) => vec![s],
            FlexList::Multiple(v) => v,
        }
    }
}

fn default_host() -> String {
    "localhost".to_string()
}

fn default_port() -> u16 {
    25
}

fn default_subject() -> String {
    "Test Email from SMTP Component".to_string()
}

fn default_body() -> String {
    "Test email from SMTP component".to_string()
}

fn connect_smtp(config: &SmtpConfig) -> Result<String> {
    let (tls_mode, tls_mode_str) = if let Some(ref mode_str) = config.smtp.tls_mode {
        match mode_str.to_lowercase().as_str() {
            "none" | "plaintext" => (TlsMode::None, "None (plaintext)"),
            "starttls" | "explicit" => (TlsMode::Starttls, "Explicit (STARTTLS)"),
            "implicit" | "ssl" | "tls" => (TlsMode::Implicit, "Implicit (SSL)"),
            _ => {
                log(
                    Level::Warn,
                    "",
                    &format!(
                        "Unknown TLS mode '{}', falling back to port-based detection",
                        mode_str
                    ),
                );
                match config.smtp.port {
                    465 => (TlsMode::Implicit, "Implicit (SSL)"),
                    25 => (TlsMode::None, "None (plaintext)"),
                    _ => (TlsMode::Starttls, "Explicit (STARTTLS)"),
                }
            }
        }
    } else {
        match config.smtp.port {
            465 => (TlsMode::Implicit, "Implicit (SSL)"),
            25 => (TlsMode::None, "None (plaintext)"),
            _ => (TlsMode::Starttls, "Explicit (STARTTLS)"),
        }
    };

    log(
        Level::Info,
        "",
        &format!(
            "Connecting to {}:{} using {}...",
            config.smtp.host, config.smtp.port, tls_mode_str
        ),
    );

    let creds = Credentials {
        host: config.smtp.host.clone(),
        port: Some(config.smtp.port),
        username: config.smtp.username.clone(),
        password: config.smtp.password.clone(),
        tls_mode,
    };

    match client::connect(&creds) {
        Ok(connection_key) => {
            log(
                Level::Info,
                "",
                &format!("Connected to {}:{}", config.smtp.host, config.smtp.port),
            );
            Ok(connection_key)
        }
        Err(e) => {
            log(Level::Error, "", &format!("Connection failed: {}", e));
            Err(anyhow::anyhow!("Failed to connect to SMTP server: {}", e))
        }
    }
}

fn read_request_body(request: IncomingRequest) -> Result<String> {
    let body_stream = request
        .consume()
        .map_err(|_| anyhow::anyhow!("Failed to consume request"))?;

    let input_stream = body_stream
        .stream()
        .map_err(|_| anyhow::anyhow!("Failed to get stream"))?;

    let mut body_data = Vec::new();
    loop {
        match input_stream.blocking_read(8192) {
            Ok(chunk) if chunk.is_empty() => break,
            Ok(chunk) => body_data.extend_from_slice(&chunk),
            Err(StreamError::Closed) => break,
            Err(e) => return Err(anyhow::anyhow!("Stream error: {e:?}")),
        }
    }

    String::from_utf8(body_data).context("Invalid UTF-8 in request body")
}

fn download_from_url(url: &str) -> Result<Vec<u8>> {
    log(
        Level::Info,
        "",
        &format!("Downloading attachment from: {}", url),
    );

    let client = Client::new();
    let response = client
        .get(url)
        .send()
        .map_err(|e| anyhow::anyhow!("Failed to send HTTP request: {e:?}"))?;

    let status = response.status_code();
    if status < 200 || status >= 300 {
        return Err(anyhow::anyhow!(
            "HTTP request failed with status code: {}",
            status
        ));
    }

    log(
        Level::Info,
        "",
        &format!("Received response with status: {}", status),
    );

    let data = response
        .body()
        .map_err(|e| anyhow::anyhow!("Failed to read response body: {e:?}"))?;

    log(
        Level::Info,
        "",
        &format!("Downloaded {} bytes from URL", data.len()),
    );

    Ok(data)
}

fn extract_filename_from_url(url: &str) -> String {
    url.split('/')
        .last()
        .and_then(|s| s.split('?').next())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "downloaded_attachment".to_string())
}

fn build_email_message(config: &SmtpConfig) -> Result<Message> {
    let mut attachments: Option<Vec<Attachment>> = None;

    if let Some(ref attachment_configs) = config.attachments {
        let mut attachment_list = Vec::new();

        for att in attachment_configs {
            log(
                Level::Info,
                "",
                &format!("Fetching attachment from URL: {}", att.url),
            );

            let content = download_from_url(&att.url)
                .with_context(|| format!("Failed to download attachment from URL: {}", att.url))?;

            let filename = att
                .filename
                .clone()
                .unwrap_or_else(|| extract_filename_from_url(&att.url));

            let content_type = att
                .content_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_string());

            log(
                Level::Info,
                "",
                &format!(
                    "Downloaded {} bytes as '{}' ({})",
                    content.len(),
                    filename,
                    content_type
                ),
            );

            attachment_list.push(Attachment {
                filename,
                content_type,
                content,
            });
        }

        if !attachment_list.is_empty() {
            attachments = Some(attachment_list);
        }
    }

    Ok(Message {
        sender: Sender {
            from: config.from.clone(),
            reply_to: config.reply_to.clone(),
            display_name: config.display_name.clone(),
        },
        recipient: Recipient {
            to: config.to.clone().into_vec(),
            cc: config.cc.clone().map(|c| c.into_vec()),
            bcc: config.bcc.clone().map(|b| b.into_vec()),
        },
        subject: config.subject.clone(),
        body: config.body.clone(),
        attachments,
    })
}

fn send_response(response_out: ResponseOutparam, status: u16, body: &[u8]) {
    let response = OutgoingResponse::new(Fields::new());
    response.set_status_code(status).unwrap();
    let response_body = response.body().unwrap();
    ResponseOutparam::set(response_out, Ok(response));
    let stream = response_body.write().unwrap();
    stream.blocking_write_and_flush(body).unwrap();
    drop(stream);
    OutgoingBody::finish(response_body, None).unwrap();
}

bindings::export!(Component with_types_in bindings);
