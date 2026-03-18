use dashmap::DashMap;
use html2text::from_read;
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
    message::{Attachment, Mailbox, MultiPart, header::ContentType},
    transport::smtp::authentication::Credentials as LettreCredentials,
};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::{collections::HashSet, sync::Arc};

use crate::{
    engine::{
        ctx::{ActiveCtx, SharedCtx, extract_active_ctx},
        workload::WorkloadItem,
    },
    plugin::HostPlugin,
    wit::{WitInterface, WitWorld},
};

use bindings::bettyblocks::smtp::client::{
    Credentials, Host, Message, SendResult, TlsMode, add_to_linker,
};

const BETTYBLOCKS_SMTP_PLUGIN_ID: &str = "bettyblocks-smtp";
const PLAIN_TEXT_WIDTH: usize = 80;
const MAX_POOLED_CONNECTIONS: u32 = 5;
const MIN_IDLE_CONNECTIONS: u32 = 0;
const TRANSPORT_TTL_SECS: u64 = 24 * 60 * 60; //24 hours

mod bindings {
    wasmtime::component::bindgen!({
        world: "smtp",
        imports: {
            default: async | trappable | tracing
        },
    });
}

#[derive(Clone)]
pub struct SharedTransport {
    pub transport: Arc<AsyncSmtpTransport<Tokio1Executor>>,
    pub credentials: Credentials,
    /// Epoch seconds when this transport was last used (for send)
    pub last_used: u64,
    /// The connection key is the hash of host:port:username:password to identify unique connections
    pub connection_key: String,
}

#[derive(Clone, Default)]
pub struct BettySmtp {
    transport_pool: Arc<DashMap<String, SharedTransport>>,
}

impl BettySmtp {
    pub fn new() -> Self {
        Self {
            transport_pool: Arc::new(DashMap::new()),
        }
    }

    fn get_timestamp() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn generate_connection_key(credentials: &Credentials) -> String {
        let mut hasher = DefaultHasher::new();
        credentials.host.hash(&mut hasher);
        credentials.port.hash(&mut hasher);
        credentials.username.hash(&mut hasher);
        credentials.password.hash(&mut hasher);
        match credentials.tls_mode {
            TlsMode::None => 0u8.hash(&mut hasher),
            TlsMode::Starttls => 1u8.hash(&mut hasher),
            TlsMode::Implicit => 2u8.hash(&mut hasher),
        }

        format!("conn-{:x}", hasher.finish())
    }

    fn build_transport(
        credentials: &Credentials,
    ) -> anyhow::Result<AsyncSmtpTransport<Tokio1Executor>> {
        let mut builder = match credentials.tls_mode {
            TlsMode::Implicit => AsyncSmtpTransport::<Tokio1Executor>::relay(&credentials.host)?,
            TlsMode::Starttls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&credentials.host)?
            }
            TlsMode::None => {
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&credentials.host)
            }
        };

        if let Some(custom_port) = credentials.port {
            builder = builder.port(custom_port);
        }

        if let (Some(username), Some(password)) = (&credentials.username, &credentials.password) {
            builder =
                builder.credentials(LettreCredentials::new(username.clone(), password.clone()));
        }

        Ok(builder
            .pool_config(
                lettre::transport::smtp::PoolConfig::new()
                    .max_size(MAX_POOLED_CONNECTIONS)
                    .min_idle(MIN_IDLE_CONNECTIONS),
            )
            .build())
    }

    fn cleanup_stale_transports(&self) {
        let now = Self::get_timestamp();
        self.transport_pool.retain(|_key, transport| {
            let stale = now.saturating_sub(transport.last_used) > TRANSPORT_TTL_SECS;
            if stale {
                tracing::info!(
                    last_used_secs_ago = now.saturating_sub(transport.last_used),
                    "Removing stale SMTP transport from pool"
                );
            }
            !stale
        });
    }

    async fn get_or_create_transport(&self, credentials: &Credentials) -> anyhow::Result<String> {
        self.cleanup_stale_transports();

        let connection_key = Self::generate_connection_key(credentials);

        if let Some(mut entry) = self.transport_pool.get_mut(&connection_key) {
            entry.last_used = Self::get_timestamp();
            tracing::debug!(
                host = credentials.host,
                port = credentials.port,
                "Reusing existing SMTP transport from pool"
            );
            return Ok(connection_key);
        }

        let transport = Self::build_transport(credentials)?;

        transport
            .test_connection()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect to SMTP server: {e}"))?;

        tracing::info!(
            host = credentials.host,
            port = credentials.port,
            "Creating new SMTP transport"
        );

        let shared_transport = SharedTransport {
            transport: Arc::new(transport),
            credentials: credentials.clone(),
            last_used: Self::get_timestamp(),
            connection_key: connection_key.clone(),
        };

        self.transport_pool
            .entry(connection_key.clone())
            .or_insert(shared_transport);

        Ok(connection_key)
    }
}

#[async_trait::async_trait]
impl HostPlugin for BettySmtp {
    fn id(&self) -> &'static str {
        BETTYBLOCKS_SMTP_PLUGIN_ID
    }

    fn world(&self) -> WitWorld {
        WitWorld {
            imports: HashSet::from([WitInterface::from("bettyblocks:smtp/client@0.2.0")]),
            ..Default::default()
        }
    }

    async fn on_workload_item_bind<'a>(
        &self,
        item: &mut WorkloadItem<'a>,
        interfaces: std::collections::HashSet<crate::wit::WitInterface>,
    ) -> anyhow::Result<()> {
        let has_smtp = interfaces
            .iter()
            .any(|i| i.namespace == "bettyblocks" && i.package == "smtp");

        if !has_smtp {
            tracing::warn!(
                "BettySmtp plugin requested for non-Betty:smtp interface(s): {:?}",
                interfaces
            );
            return Ok(());
        }

        tracing::debug!(
            workload_id = item.id(),
            "Adding SMTP interface to linker for workload"
        );

        add_to_linker::<_, SharedCtx>(item.linker(), extract_active_ctx)?;

        let id = item.workload_id();
        tracing::debug!(
            workload_id = id,
            "Successfully added SMTP interface to linker for workload"
        );

        tracing::debug!("BettySmtp plugin bound to workload '{id}'");

        Ok(())
    }

    async fn on_workload_unbind(
        &self,
        workload_id: &str,
        _interfaces: HashSet<crate::wit::WitInterface>,
    ) -> anyhow::Result<()> {
        tracing::debug!("BettySmtp plugin unbound from workload '{workload_id}'");
        Ok(())
    }
}

impl<'a> Host for ActiveCtx<'a> {
    async fn connect(
        &mut self,
        credentials: Credentials,
    ) -> wasmtime::Result<Result<String, String>> {
        let Some(plugin) = self.get_plugin::<BettySmtp>(BETTYBLOCKS_SMTP_PLUGIN_ID) else {
            return Ok(Err("SMTP plugin not available".to_string()));
        };

        let connection_key = match plugin.get_or_create_transport(&credentials).await {
            Ok(key) => key,
            Err(e) => {
                return Ok(Err(format!("Failed to connect to SMTP server: {e}")));
            }
        };

        tracing::debug!(
            workload_id = self.workload_id.to_string(),
            host = credentials.host,
            port = credentials.port,
            "SMTP client connected (using shared transport)"
        );

        Ok(Ok(connection_key))
    }

    async fn send(
        &mut self,
        connection_key: String,
        message: Message,
    ) -> wasmtime::Result<Result<SendResult, String>> {
        let Some(plugin) = self.get_plugin::<BettySmtp>(BETTYBLOCKS_SMTP_PLUGIN_ID) else {
            return Ok(Err("SMTP plugin not available".to_string()));
        };
        plugin.cleanup_stale_transports();

        // Important(Aditya): We clone transport data and drop the DashMap guard before any .await
        // to avoid holding a sync lock across async suspension points (deadlock).
        let transport_data = {
            let Some(mut shared_transport) = plugin.transport_pool.get_mut(&connection_key) else {
                return Ok(Err(format!(
                    "SMTP transport '{}' not found",
                    connection_key
                )));
            };
            shared_transport.last_used = BettySmtp::get_timestamp();
            shared_transport.clone()
        };

        let display_name = message
            .sender
            .display_name
            .as_deref()
            .filter(|n| !n.is_empty())
            .map(String::from);

        let from_mailbox = Mailbox::new(
            display_name.clone(),
            message
                .sender
                .from
                .parse()
                .map_err(|e| wasmtime::Error::msg(format!("invalid sender address: {e}")))?,
        );

        let mut email_builder = lettre::Message::builder()
            .from(from_mailbox)
            .subject(message.subject.clone());

        if let Some(reply_to) = message.sender.reply_to {
            let reply_to_mailbox = Mailbox::new(
                display_name,
                reply_to
                    .parse()
                    .map_err(|e| wasmtime::Error::msg(format!("invalid reply-to address: {e}")))?,
            );
            email_builder = email_builder.reply_to(reply_to_mailbox);
        }

        for to in message.recipient.to {
            email_builder = email_builder.to(to
                .parse()
                .map_err(|e| wasmtime::Error::msg(format!("invalid recipient address: {e}")))?);
        }

        if let Some(cc_list) = message.recipient.cc {
            for cc in cc_list {
                email_builder = email_builder.cc(cc
                    .parse()
                    .map_err(|e| wasmtime::Error::msg(format!("invalid CC address: {e}")))?);
            }
        }

        if let Some(bcc_list) = message.recipient.bcc {
            for bcc in bcc_list {
                email_builder = email_builder.bcc(
                    bcc.parse()
                        .map_err(|e| wasmtime::Error::msg(format!("invalid BCC address: {e}")))?,
                );
            }
        }

        let plain_text = from_read(message.body.as_bytes(), PLAIN_TEXT_WIDTH).unwrap_or_else(|e| {
            tracing::warn!(
                "Failed to convert HTML to plain text: {}, using raw body as fallback",
                e
            );
            message.body.clone()
        });

        let mut multipart = MultiPart::mixed()
            .multipart(MultiPart::alternative_plain_html(plain_text, message.body));

        if let Some(attachments) = message.attachments {
            for attachment in attachments {
                let content_type =
                    ContentType::parse(&attachment.content_type).unwrap_or(ContentType::TEXT_PLAIN);

                let attachment =
                    Attachment::new(attachment.filename).body(attachment.content, content_type);

                multipart = multipart.singlepart(attachment);
            }
        }

        let email = email_builder
            .multipart(multipart)
            .map_err(|e| wasmtime::Error::msg(format!("failed to build email: {e}")))?;

        tracing::info!(
            workload_id = %self.workload_id,
            "Sending email via shared SMTP transport"
        );

        match transport_data.transport.send(email).await {
            Ok(response) => {
                tracing::debug!(
                    workload_id = %self.workload_id,
                    response = ?response,
                    "Email sent successfully"
                );

                let raw_msg = response.message().collect::<Vec<_>>().join(" ");
                let message_id_opt = if raw_msg.is_empty() {
                    None
                } else {
                    Some(raw_msg)
                };

                let effective_port = transport_data.credentials.port.unwrap_or(
                    match transport_data.credentials.tls_mode {
                        TlsMode::Implicit => 465,
                        TlsMode::Starttls => 587,
                        TlsMode::None => 25,
                    },
                );

                let server_addr = Some(format!(
                    "{}:{}",
                    transport_data.credentials.host, effective_port
                ));

                Ok(Ok(SendResult {
                    accepted: true,
                    server: server_addr,
                    message_id: message_id_opt,
                }))
            }
            Err(e) => {
                tracing::error!(
                    workload_id = %self.workload_id,
                    error = %e,
                    "Failed to send email"
                );
                Ok(Err(format!("failed to send email: {e}")))
            }
        }
    }

    async fn disconnect(&mut self, connection_key: String) -> wasmtime::Result<Result<(), String>> {
        let Some(plugin) = self.get_plugin::<BettySmtp>(BETTYBLOCKS_SMTP_PLUGIN_ID) else {
            return Ok(Err("SMTP plugin not available".to_string()));
        };

        if plugin.transport_pool.remove(&connection_key).is_some() {
            tracing::debug!(
                workload_id = self.workload_id.to_string(),
                "SMTP transport disconnected and removed from pool"
            );
            Ok(Ok(()))
        } else {
            tracing::warn!(
                workload_id = self.workload_id.to_string(),
                "SMTP transport not found in pool (may have already been disconnected)"
            );
            Ok(Ok(()))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_key_consistency() {
        let creds1 = Credentials {
            host: "smtp.gmail.com".to_string(),
            port: None,
            username: Some("test@gmail.com".to_string()),
            password: Some("password".to_string()),
            tls_mode: TlsMode::Implicit,
        };

        let creds2 = Credentials {
            host: "smtp.gmail.com".to_string(),
            port: None,
            username: Some("test@gmail.com".to_string()),
            password: Some("password".to_string()),
            tls_mode: TlsMode::Implicit,
        };

        let key1 = BettySmtp::generate_connection_key(&creds1);
        let key2 = BettySmtp::generate_connection_key(&creds2);

        assert_eq!(
            key1, key2,
            "Same credentials should produce same connection key"
        );
    }

    #[tokio::test]
    async fn test_stale_transport_cleanup() {
        let smtp = BettySmtp::new();

        let old_transport = SharedTransport {
            transport: Arc::new(
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous("localhost").build(),
            ),
            credentials: Credentials {
                host: "localhost".to_string(),
                port: None,
                username: None,
                password: None,
                tls_mode: TlsMode::None,
            },
            last_used: 0, // 0 epoch = very old
            connection_key: "conn-old".to_string(),
        };
        smtp.transport_pool
            .insert("conn-old".to_string(), old_transport);

        let fresh_transport = SharedTransport {
            transport: Arc::new(
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous("localhost").build(),
            ),
            credentials: Credentials {
                host: "localhost".to_string(),
                port: None,
                username: None,
                password: None,
                tls_mode: TlsMode::None,
            },
            last_used: BettySmtp::get_timestamp(),
            connection_key: "conn-fresh".to_string(),
        };
        smtp.transport_pool
            .insert("conn-fresh".to_string(), fresh_transport);

        assert_eq!(smtp.transport_pool.len(), 2);

        smtp.cleanup_stale_transports();

        assert_eq!(smtp.transport_pool.len(), 1);
        assert!(smtp.transport_pool.contains_key("conn-fresh"));
        assert!(!smtp.transport_pool.contains_key("conn-old"));
    }

    #[tokio::test]
    async fn test_connection_reuse_same_credentials() {
        let smtp = BettySmtp::new();

        let creds = Credentials {
            host: "localhost".to_string(),
            port: Some(2525),
            username: None,
            password: None,
            tls_mode: TlsMode::None,
        };

        let key = BettySmtp::generate_connection_key(&creds);
        let transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous("localhost")
            .port(2525)
            .build();
        smtp.transport_pool.insert(
            key.clone(),
            SharedTransport {
                transport: Arc::new(transport),
                credentials: creds.clone(),
                last_used: BettySmtp::get_timestamp(),
                connection_key: key.clone(),
            },
        );

        let result = smtp.get_or_create_transport(&creds).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), key);
        assert_eq!(
            smtp.transport_pool.len(),
            1,
            "pool should still have exactly 1 entry (reused, not duplicated)"
        );
    }
}
