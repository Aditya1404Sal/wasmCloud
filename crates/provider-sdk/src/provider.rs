use core::fmt;
use core::fmt::Formatter;
use core::future::Future;

use core::pin::{pin, Pin};
use core::time::Duration;
use std::collections::HashMap;
use std::io::BufRead;
use std::sync::Arc;

use anyhow::{bail, Context as _, Result};
use async_nats::subject::ToSubject as _;
use async_nats::HeaderMap;
use base64::Engine;
use bytes::Bytes;
use futures::{stream, Stream, StreamExt as _, TryStreamExt as _};
use nkeys::XKey;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};
use tokio::task::{spawn_blocking, JoinSet};
use tokio::{select, spawn, try_join};
use tracing::{debug, error, info, instrument, trace, warn, Instrument as _};
use wasmcloud_core::nats::convert_header_map_to_hashmap;
use wasmcloud_core::rpc::{health_subject, link_del_subject, link_put_subject, shutdown_subject};
use wasmcloud_core::secrets::SecretValue;
use wasmcloud_core::{
    provider_config_update_subject, HealthCheckRequest, HealthCheckResponse, HostData,
    InterfaceLinkDefinition, LatticeTarget,
};

#[cfg(feature = "otel")]
use wasmcloud_core::TraceContext;
#[cfg(feature = "otel")]
use wasmcloud_tracing::context::attach_span_context;
use wrpc_transport::InvokeExt as _;

use crate::error::{ProviderInitError, ProviderInitResult};
use crate::{with_connection_event_logging, Context, LinkConfig, Provider, DEFAULT_NATS_ADDR};

/// Name of the header that should be passed for invocations that identifies the source
const WRPC_SOURCE_ID_HEADER_NAME: &str = "source-id";

static HOST_DATA: OnceCell<HostData> = OnceCell::new();
static CONNECTION: OnceCell<ProviderConnection> = OnceCell::new();

/// Retrieves the currently configured connection to the lattice. DO NOT call this method until
/// after the provider is running (meaning [`run_provider`] has been called)
/// or this method will panic. Only in extremely rare cases should this be called manually and it
/// will only be used by generated code
// NOTE(thomastaylor312): This isn't the most elegant solution, but providers that need to send
// messages to the lattice rather than just responding need to get the same connection used when the
// provider was started, which means a global static
pub fn get_connection() -> &'static ProviderConnection {
    CONNECTION
        .get()
        .expect("Provider connection not initialized")
}

/// Loads configuration data sent from the host over stdin. The returned host data contains all the
/// configuration information needed to connect to the lattice and any additional configuration
/// provided to this provider (like `config_json`).
///
/// NOTE: this function will read the data from stdin exactly once. If this function is called more
/// than once, it will return a copy of the original data fetched
pub fn load_host_data() -> ProviderInitResult<&'static HostData> {
    HOST_DATA.get_or_try_init(_load_host_data)
}

/// Initializes the host data with the provided data. This is useful for testing or if the host data
/// is not being provided over stdin.
///
/// If the host data has already been initialized, this function will return the existing host data.
pub fn initialize_host_data(host_data: HostData) -> ProviderInitResult<&'static HostData> {
    HOST_DATA.get_or_try_init(|| Ok(host_data))
}

// Internal function for populating the host data
fn _load_host_data() -> ProviderInitResult<HostData> {
    let mut buffer = String::new();
    let stdin = std::io::stdin();
    {
        let mut handle = stdin.lock();
        handle.read_line(&mut buffer).map_err(|e| {
            ProviderInitError::Initialization(format!(
                "failed to read host data configuration from stdin: {e}"
            ))
        })?;
    }
    // remove spaces, tabs, and newlines before and after base64-encoded data
    let buffer = buffer.trim();
    if buffer.is_empty() {
        return Err(ProviderInitError::Initialization(
            "stdin is empty - expecting host data configuration".to_string(),
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(buffer.as_bytes())
        .map_err(|e| {
            ProviderInitError::Initialization(format!(
            "host data configuration passed through stdin has invalid encoding (expected base64): \
             {e}"
        ))
        })?;
    let host_data: HostData = serde_json::from_slice(&bytes).map_err(|e| {
        ProviderInitError::Initialization(format!(
            "parsing host data: {}:\n{}",
            e,
            String::from_utf8_lossy(&bytes)
        ))
    })?;
    Ok(host_data)
}

pub type QuitSignal = broadcast::Receiver<()>;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ShutdownMessage {
    /// The ID of the host that sent the message
    pub host_id: String,
}

#[doc(hidden)]
/// Process subscription, until closed or exhausted, or value is received on the channel.
/// `sub` is a mutable Subscriber (regular or queue subscription)
/// `channel` may be either tokio mpsc::Receiver or broadcast::Receiver, and is considered signaled
/// when a value is sent or the channel is closed.
/// `msg` is the variable name to be used in the handler
/// `on_item` is an async handler
macro_rules! process_until_quit {
    ($sub:ident, $channel:ident, $msg:ident, $on_item:tt) => {
        spawn(async move {
            loop {
                select! {
                    _ = $channel.recv() => {
                        let _ = $sub.unsubscribe().await;
                        break;
                    },
                    __msg = $sub.next() => {
                        match __msg {
                            None => break,
                            Some($msg) => $on_item
                        }
                    }
                }
            }
        })
    };
}

async fn subscribe_health(
    nats: Arc<async_nats::Client>,
    mut quit: broadcast::Receiver<()>,
    lattice: &str,
    provider_key: &str,
) -> ProviderInitResult<mpsc::Receiver<(HealthCheckRequest, oneshot::Sender<HealthCheckResponse>)>>
{
    let mut sub = nats
        .subscribe(health_subject(lattice, provider_key))
        .await?;
    let (health_tx, health_rx) = mpsc::channel(1);
    spawn({
        let nats = Arc::clone(&nats);
        async move {
            process_until_quit!(sub, quit, msg, {
                let (tx, rx) = oneshot::channel();
                if let Err(err) = health_tx.send((HealthCheckRequest {}, tx)).await {
                    error!(%err, "failed to send health check request");
                    continue;
                }
                match rx.await.as_ref().map(serde_json::to_vec) {
                    Err(err) => {
                        error!(%err, "failed to receive health check response");
                    }
                    Ok(Ok(t)) => {
                        if let Some(reply_to) = msg.reply {
                            if let Err(err) = nats.publish(reply_to, t.into()).await {
                                error!(%err, "failed sending health check response");
                            }
                        }
                    }
                    Ok(Err(err)) => {
                        // extremely unlikely that InvocationResponse would fail to serialize
                        error!(%err, "failed serializing HealthCheckResponse");
                    }
                }
            });
        }
        .instrument(tracing::debug_span!("subscribe_health"))
    });
    Ok(health_rx)
}

async fn subscribe_shutdown(
    nats: Arc<async_nats::Client>,
    quit: broadcast::Sender<()>,
    lattice: &str,
    provider_key: &str,
    host_id: impl Into<Arc<str>>,
) -> ProviderInitResult<mpsc::Receiver<oneshot::Sender<()>>> {
    let mut sub = nats
        .subscribe(shutdown_subject(lattice, provider_key, "default"))
        .await?;
    let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
    let host_id = host_id.into();
    spawn({
        async move {
            loop {
                let msg = sub.next().await;
                // Check if we really need to shut down
                if let Some(async_nats::Message {
                    reply: Some(reply_to),
                    payload,
                    ..
                }) = msg
                {
                    let ShutdownMessage {
                        host_id: ref req_host_id,
                    } = serde_json::from_slice(&payload).unwrap_or_default();
                    if req_host_id == host_id.as_ref() {
                        info!("Received termination signal and stopping");
                        // Tell provider to shutdown - before we shut down nats subscriptions,
                        // in case it needs to do any message passing during shutdown
                        let (tx, rx) = oneshot::channel();
                        match shutdown_tx.send(tx).await {
                            Ok(()) => {
                                if let Err(err) = rx.await {
                                    error!(%err, "failed to await shutdown");
                                }
                            }
                            Err(err) => error!(%err, "failed to send shutdown"),
                        }
                        if let Err(err) = nats.publish(reply_to, "shutting down".into()).await {
                            warn!(%err, "failed to send shutdown ack");
                        }
                        // unsubscribe from shutdown topic
                        if let Err(err) = sub.unsubscribe().await {
                            warn!(%err, "failed to unsubscribe from shutdown topic");
                        }
                        // send shutdown signal to all listeners: quit all subscribers and signal main thread to quit
                        if let Err(err) = quit.send(()) {
                            error!(%err, "Problem shutting down:  failure to send signal");
                        }
                        break;
                    }
                    trace!("Ignoring termination signal (request targeted for different host)");
                }
            }
        }
        .instrument(tracing::debug_span!("shutdown_subscriber"))
    });
    Ok(shutdown_rx)
}

async fn subscribe_link_put(
    nats: Arc<async_nats::Client>,
    mut quit: broadcast::Receiver<()>,
    lattice: &str,
    provider_xkey: &str,
) -> ProviderInitResult<mpsc::Receiver<(InterfaceLinkDefinition, oneshot::Sender<()>)>> {
    let (link_put_tx, link_put_rx) = mpsc::channel(1);
    let mut sub = nats
        .subscribe(link_put_subject(lattice, provider_xkey))
        .await?;
    spawn(async move {
        process_until_quit!(sub, quit, msg, {
            match serde_json::from_slice::<InterfaceLinkDefinition>(&msg.payload) {
                Ok(ld) => {
                    let span = tracing::Span::current();
                    span.record("source_id", tracing::field::display(&ld.source_id));
                    span.record("target", tracing::field::display(&ld.target));
                    span.record("wit_namespace", tracing::field::display(&ld.wit_namespace));
                    span.record("wit_package", tracing::field::display(&ld.wit_package));
                    span.record(
                        "wit_interfaces",
                        tracing::field::display(&ld.interfaces.join(",")),
                    );
                    span.record("link_name", tracing::field::display(&ld.name));
                    let (tx, rx) = oneshot::channel();
                    if let Err(err) = link_put_tx.send((ld, tx)).await {
                        error!(%err, "failed to send link put request");
                        continue;
                    }
                    if let Err(err) = rx.await {
                        error!(%err, "failed to await link_put");
                    }
                }
                Err(err) => {
                    error!(%err, "received invalid link def data on message");
                }
            }
        });
    });
    Ok(link_put_rx)
}

async fn subscribe_link_del(
    nats: Arc<async_nats::Client>,
    mut quit: broadcast::Receiver<()>,
    lattice: &str,
    provider_key: &str,
) -> ProviderInitResult<mpsc::Receiver<(InterfaceLinkDefinition, oneshot::Sender<()>)>> {
    let subject = link_del_subject(lattice, provider_key).to_subject();
    debug!(%subject, "subscribing for link del");
    let mut sub = nats.subscribe(subject.clone()).await?;
    let (link_del_tx, link_del_rx) = mpsc::channel(1);
    let span = tracing::trace_span!("subscribe_link_del", %subject);
    spawn(
        async move {
            process_until_quit!(sub, quit, msg, {
                if let Ok(ld) = serde_json::from_slice::<InterfaceLinkDefinition>(&msg.payload) {
                    let (tx, rx) = oneshot::channel();
                    if let Err(err) = link_del_tx.send((ld, tx)).await {
                        error!(%err, "failed to send link del request");
                        continue;
                    }
                    if let Err(err) = rx.await {
                        error!(%err, "failed to await link_del");
                    }
                } else {
                    error!("received invalid link on link_del");
                }
            });
        }
        .instrument(span),
    );
    Ok(link_del_rx)
}

/// Subscribe to configuration updates that are passed by the host.
///
/// We expect the hosts to send configuration updates messages over NATS,
/// with information on whether the configuration applies to a specific link,
/// and the contents of the new/updated configuration.
async fn subscribe_config_update(
    nats: Arc<async_nats::Client>,
    mut quit: broadcast::Receiver<()>,
    lattice: &str,
    provider_key: &str,
) -> ProviderInitResult<mpsc::Receiver<(HashMap<String, String>, oneshot::Sender<()>)>> {
    let (config_update_tx, config_update_rx) = mpsc::channel(1);
    let mut sub = nats
        .subscribe(provider_config_update_subject(lattice, provider_key).to_subject())
        .await?;
    spawn({
        async move {
            process_until_quit!(sub, quit, msg, {
                match serde_json::from_slice::<HashMap<String, String>>(&msg.payload) {
                    Ok(update) => {
                        let (tx, rx) = oneshot::channel();
                        // Perform the config update on the host
                        if let Err(err) = config_update_tx.send((update, tx)).await {
                            error!(%err, "failed to send config update");
                            continue;
                        }
                        // Wait for the response from the rx to perform it
                        if let Err(err) = rx.await.as_ref() {
                            error!(%err, "failed to receive config update response");
                        }
                    }
                    Err(err) => {
                        error!(%err, "received invalid config update data on message");
                    }
                }
            });
        }
        .instrument(tracing::debug_span!("subscribe_config_update"))
    });

    Ok(config_update_rx)
}

pub struct ProviderCommandReceivers {
    health: mpsc::Receiver<(HealthCheckRequest, oneshot::Sender<HealthCheckResponse>)>,
    shutdown: mpsc::Receiver<oneshot::Sender<()>>,
    link_put: mpsc::Receiver<(InterfaceLinkDefinition, oneshot::Sender<()>)>,
    link_del: mpsc::Receiver<(InterfaceLinkDefinition, oneshot::Sender<()>)>,
    config_update: mpsc::Receiver<(HashMap<String, String>, oneshot::Sender<()>)>,
}

impl ProviderCommandReceivers {
    pub async fn new(
        nats: Arc<async_nats::Client>,
        quit_tx: &broadcast::Sender<()>,
        lattice: &str,
        provider_key: &str,
        provider_link_put_id: &str,
        host_id: &str,
    ) -> ProviderInitResult<Self> {
        let (health, shutdown, link_put, link_del, config_update) = try_join!(
            subscribe_health(
                Arc::clone(&nats),
                quit_tx.subscribe(),
                lattice,
                provider_key
            ),
            subscribe_shutdown(
                Arc::clone(&nats),
                quit_tx.clone(),
                lattice,
                provider_key,
                host_id
            ),
            subscribe_link_put(
                Arc::clone(&nats),
                quit_tx.subscribe(),
                lattice,
                provider_link_put_id
            ),
            subscribe_link_del(
                Arc::clone(&nats),
                quit_tx.subscribe(),
                lattice,
                provider_key
            ),
            subscribe_config_update(
                Arc::clone(&nats),
                quit_tx.subscribe(),
                lattice,
                provider_key
            ),
        )?;
        Ok(Self {
            health,
            shutdown,
            link_put,
            link_del,
            config_update,
        })
    }
}

/// State of provider initialization
pub(crate) struct ProviderInitState {
    pub nats: Arc<async_nats::Client>,
    pub quit_rx: broadcast::Receiver<()>,
    pub quit_tx: broadcast::Sender<()>,
    pub host_id: String,
    pub lattice_rpc_prefix: String,
    pub provider_key: String,
    pub link_definitions: Vec<InterfaceLinkDefinition>,
    pub commands: ProviderCommandReceivers,
    pub config: HashMap<String, String>,
    pub secrets: HashMap<String, SecretValue>,
    /// The public key xkey of the host, used for decrypting secrets
    /// Do not attempt to access the [`XKey::seed()`] of this XKey, it will always error.
    host_public_xkey: XKey,
    provider_private_xkey: XKey,
}

#[instrument]
async fn init_provider(name: &str) -> ProviderInitResult<ProviderInitState> {
    let HostData {
        host_id,
        lattice_rpc_prefix,
        lattice_rpc_user_jwt,
        lattice_rpc_user_seed,
        lattice_rpc_url,
        provider_key,
        env_values: _,
        cluster_issuers: _,
        instance_id,
        link_definitions,
        config,
        secrets,
        default_rpc_timeout_ms: _,
        link_name: _link_name,
        host_xkey_public_key,
        provider_xkey_private_key,
        ..
    } = spawn_blocking(load_host_data).await.map_err(|e| {
        ProviderInitError::Initialization(format!("failed to load host data: {e}"))
    })??;

    let (quit_tx, quit_rx) = broadcast::channel(1);

    // If the xkey strings are empty, it just means that the host is <1.1.0 and does not support secrets.
    // There aren't any negative side effects here, so it's really just a warning to update to 1.1.0.
    let host_public_xkey = if host_xkey_public_key.is_empty() {
        warn!("Provider is running on a host that does not provide a host xkey, secrets will not be supported");
        XKey::new()
    } else {
        XKey::from_public_key(host_xkey_public_key).map_err(|e| {
            ProviderInitError::Initialization(format!(
                "failed to create host xkey from public key: {e}"
            ))
        })?
    };
    let provider_private_xkey = if provider_xkey_private_key.is_empty() {
        warn!("Provider is running on a host that does not provide a provider xkey, secrets will not be supported");
        XKey::new()
    } else {
        XKey::from_seed(provider_xkey_private_key).map_err(|e| {
            ProviderInitError::Initialization(format!(
                "failed to create provider xkey from private key: {e}"
            ))
        })?
    };

    // wasmCloud 1.1.0 hosts provide xkeys and publish links to the provider using the xkey public key in the NATS subject.
    // Older hosts will use the provider key in the NATS subject.
    // This allows for backwards compatibility with older hosts.
    let provider_link_put_id = if host_xkey_public_key.is_empty()
        && provider_xkey_private_key.is_empty()
    {
        debug!("Provider is running on a host that does not provide xkeys, using provider key in NATS subject");
        provider_key.to_string()
    } else {
        debug!("Provider is running on a host that provides xkeys, using provider xkey in NATS subject");
        provider_private_xkey.public_key()
    };

    info!(
        "Starting capability provider {provider_key} instance {instance_id} with nats url {lattice_rpc_url}"
    );

    // Build the NATS client
    let nats_addr = if !lattice_rpc_url.is_empty() {
        lattice_rpc_url.as_str()
    } else {
        DEFAULT_NATS_ADDR
    };

    let nats = with_connection_event_logging(
        match (lattice_rpc_user_jwt.trim(), lattice_rpc_user_seed.trim()) {
            ("", "") => async_nats::ConnectOptions::default(),
            (rpc_jwt, rpc_seed) => {
                let key_pair = Arc::new(nkeys::KeyPair::from_seed(rpc_seed).unwrap());
                let jwt = rpc_jwt.to_owned();
                async_nats::ConnectOptions::with_jwt(jwt, move |nonce| {
                    let key_pair = key_pair.clone();
                    async move { key_pair.sign(&nonce).map_err(async_nats::AuthError::new) }
                })
            }
        },
    )
    .name(name)
    .connect(nats_addr)
    .await?;
    let nats = Arc::new(nats);

    // Listen and process various provider events/functionality
    let commands = ProviderCommandReceivers::new(
        Arc::clone(&nats),
        &quit_tx,
        lattice_rpc_prefix,
        provider_key,
        &provider_link_put_id,
        host_id,
    )
    .await?;
    Ok(ProviderInitState {
        nats,
        quit_rx,
        quit_tx,
        host_id: host_id.clone(),
        lattice_rpc_prefix: lattice_rpc_prefix.clone(),
        provider_key: provider_key.clone(),
        link_definitions: link_definitions.clone(),
        config: config.clone(),
        secrets: secrets.clone(),
        host_public_xkey,
        provider_private_xkey,
        commands,
    })
}

/// Appropriately receive a link (depending on if it's source/target) for a provider
pub async fn receive_link_for_provider<P>(
    provider: &P,
    connection: &ProviderConnection,
    ld: InterfaceLinkDefinition,
) -> Result<()>
where
    P: Provider,
{
    match if ld.source_id == *connection.provider_id {
        provider
            .receive_link_config_as_source(LinkConfig {
                source_id: &ld.source_id,
                target_id: &ld.target,
                link_name: &ld.name,
                config: &ld.source_config,
                secrets: &decrypt_link_secret(
                    ld.source_secrets.as_deref(),
                    &connection.provider_xkey,
                    &connection.host_xkey,
                )?,
                wit_metadata: (&ld.wit_namespace, &ld.wit_package, &ld.interfaces),
            })
            .await
    } else if ld.target == *connection.provider_id {
        provider
            .receive_link_config_as_target(LinkConfig {
                source_id: &ld.source_id,
                target_id: &ld.target,
                link_name: &ld.name,
                config: &ld.target_config,
                secrets: &decrypt_link_secret(
                    ld.target_secrets.as_deref(),
                    &connection.provider_xkey,
                    &connection.host_xkey,
                )?,
                wit_metadata: (&ld.wit_namespace, &ld.wit_package, &ld.interfaces),
            })
            .await
    } else {
        bail!("received link put where provider was neither source nor target");
    } {
        Ok(()) => connection.put_link(ld).await,
        Err(e) => {
            warn!(error = %e, "receiving link failed");
        }
    };
    Ok(())
}

/// Given a serialized and encrypted [`HashMap<String, SecretValue>`], decrypts the secrets and deserializes
/// the inner bytes into a [`HashMap<String, SecretValue>`]. This can either fail due to a decryption error
/// or a deserialization error.
///
/// This will return an empty [`HashMap`] if no secrets are provided.
fn decrypt_link_secret(
    secrets: Option<&[u8]>,
    provider_xkey: &XKey,
    host_xkey: &XKey,
) -> Result<HashMap<String, SecretValue>> {
    // Note that we only `unwrap_or` in the fallback case where there are no secrets,
    // not when the decryption or deserialization fails.
    secrets
        .map(|secrets| {
            provider_xkey.open(secrets, host_xkey).map(|secrets| {
                serde_json::from_slice(&secrets).context("failed to deserialize secrets")
            })?
        })
        .unwrap_or(Ok(HashMap::with_capacity(0)))
}

async fn delete_link_for_provider<P>(
    provider: &P,
    connection: &ProviderConnection,
    ld: InterfaceLinkDefinition,
) -> Result<()>
where
    P: Provider,
{
    debug!(
        provider_id = &connection.provider_id.to_string(),
        "Deleting link for provider {ld:?}"
    );
    if *ld.source_id == *connection.provider_id {
        if let Err(e) = provider.delete_link_as_source(&ld).await {
            error!(error = %e, target = &ld.target, "failed to delete link to component");
        }
    } else if *ld.target == *connection.provider_id {
        if let Err(e) = provider.delete_link_as_target(&ld).await {
            error!(error = %e, source = &ld.source_id, "failed to delete link from component");
        }
    }
    connection.delete_link(&ld.source_id, &ld.target).await;
    Ok(())
}

/// Handle provider commands in a loop.
pub async fn handle_provider_commands(
    provider: impl Provider,
    connection: &ProviderConnection,
    mut quit_rx: broadcast::Receiver<()>,
    quit_tx: broadcast::Sender<()>,
    ProviderCommandReceivers {
        mut health,
        mut shutdown,
        mut link_put,
        mut link_del,
        mut config_update,
    }: ProviderCommandReceivers,
) {
    loop {
        select! {
            // run until we receive a shutdown request from host
            _ = quit_rx.recv() => {
                // flush async_nats client
                connection.flush().await;
                return
            }
            req = health.recv() => {
                if let Some((req, tx)) = req {
                    let res = match provider.health_request(&req).await {
                        Ok(v) => v,
                        Err(e) => {
                            error!(error = %e, "provider health request failed");
                            return;
                        }
                    };
                    if tx.send(res).is_err() {
                        error!("failed to send health check response");
                    }
                } else {
                    error!("failed to handle health check, shutdown");
                    if let Err(e) = provider.shutdown().await {
                        error!(error = %e, "failed to shutdown provider");
                    }
                    if quit_tx.send(()).is_err() {
                        error!("failed to send quit");
                    };
                    return
                };
            }
            req = shutdown.recv() => {
                if let Some(tx) = req {
                    if let Err(e) = provider.shutdown().await {
                        error!(error = %e, "failed to shutdown provider");
                    }
                    if tx.send(()).is_err() {
                        error!("failed to send shutdown response");
                    }
                } else {
                    error!("failed to handle shutdown, shutdown");
                    if let Err(e) = provider.shutdown().await {
                        error!(error = %e, "failed to shutdown provider");
                    }
                    if quit_tx.send(()).is_err() {
                        error!("failed to send quit");
                    };
                    return
                };
            }
            req = link_put.recv() => {
                if let Some((ld, tx)) = req {
                    // If the link has already been put, return early
                    if connection.is_linked(&ld.source_id, &ld.target, &ld.wit_namespace, &ld.wit_package, &ld.name).await {
                        warn!(
                            source = &ld.source_id,
                            target = &ld.target,
                            link_name = &ld.name,
                            "Ignoring duplicate link put"
                        );
                    } else {
                        info!("Linking component with provider");
                        if let Err(e) = receive_link_for_provider(&provider, connection, ld).await {
                            error!(error = %e, "failed to receive link for provider");
                        }
                    }
                    if tx.send(()).is_err() {
                        error!("failed to send link put response");
                    }
                } else {
                    error!("failed to handle link put, shutdown");
                    if let Err(e) = provider.shutdown().await {
                        error!(error = %e, "failed to shutdown provider");
                    }
                    if quit_tx.send(()).is_err() {
                        error!("failed to send quit");
                    };
                    return;
                };
            }
            req = link_del.recv() => {
                if let Some((ld, tx)) = req {
                    // notify provider that link is deleted
                    if let Err(e) = delete_link_for_provider(&provider, connection, ld).await {
                        error!(error = %e, "failed to delete link for provider");
                    }

                    if tx.send(()).is_err() {
                        error!("failed to send link del response");
                    }
                } else {
                    error!("failed to handle link del, shutdown");
                    if let Err(e) = provider.shutdown().await {
                        error!(error = %e, "failed to shutdown provider");
                    }
                    if quit_tx.send(()).is_err() {
                        error!("failed to send quit");
                    };
                    return
                };
            }
            req = config_update.recv() => {
                if let Some((cfg, tx)) = req {
                    // Notify the provider that some config has been updated
                    if let Err(e) = provider.on_config_update(&cfg).await {
                        error!(error = %e, "failed to pass through config update for provider");
                    }

                    if tx.send(()).is_err() {
                        error!("failed to send config update response");
                    }
                } else {
                    error!("failed to handle config update, shutdown");
                    if let Err(e) = provider.shutdown().await {
                        error!(error = %e, "failed to shutdown provider");
                    }
                    if quit_tx.send(()).is_err() {
                        error!("failed to send quit");
                    };
                    return
                };
            }
        }
    }
}

/// Runs the provider handler given a provider implementation and a name.
/// It returns a [Future], which will become ready once shutdown signal is received.
pub async fn run_provider(
    provider: impl Provider,
    friendly_name: &str,
) -> ProviderInitResult<impl Future<Output = ()>> {
    let init_state = init_provider(friendly_name).await?;

    // Run user-implemented provider-internal specific initialization
    if let Err(e) = provider.init(&init_state).await {
        return Err(ProviderInitError::Initialization(format!(
            "provider init failed: {e}"
        )));
    }

    let ProviderInitState {
        nats,
        quit_rx,
        quit_tx,
        host_id,
        lattice_rpc_prefix,
        provider_key,
        link_definitions,
        commands,
        config,
        secrets: _secrets,
        host_public_xkey: host_xkey,
        provider_private_xkey: provider_xkey,
    } = init_state;

    let connection = ProviderConnection::new(
        Arc::clone(&nats),
        provider_key,
        lattice_rpc_prefix,
        host_id,
        config,
        provider_xkey,
        host_xkey,
    )?;
    CONNECTION.set(connection).map_err(|_| {
        ProviderInitError::Initialization("Provider connection was already initialized".to_string())
    })?;
    let connection = get_connection();

    // Provide all links to the provider at startup to establish the initial state
    for ld in link_definitions {
        if let Err(e) = receive_link_for_provider(&provider, connection, ld).await {
            error!(
                error = %e,
                "failed to initialize link during provider startup",
            );
        }
    }

    debug!(?friendly_name, "provider finished initialization");
    Ok(handle_provider_commands(
        provider, connection, quit_rx, quit_tx, commands,
    ))
}

/// This is the type returned by the `serve` function generated by [`wit-bindgen-wrpc`]
pub type InvocationStreams = Vec<(
    &'static str,
    &'static str,
    Pin<
        Box<
            dyn Stream<
                    Item = anyhow::Result<
                        Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>>,
                    >,
                > + Send
                + 'static,
        >,
    >,
)>;

/// Serve exports of the provider using the `serve` function generated by [`wit-bindgen-wrpc`]
pub async fn serve_provider_exports<'a, P, F, Fut>(
    client: &'a WrpcClient,
    provider: P,
    shutdown: impl Future<Output = ()>,
    serve: F,
) -> anyhow::Result<()>
where
    F: FnOnce(&'a WrpcClient, P) -> Fut,
    Fut: Future<Output = anyhow::Result<InvocationStreams>> + wrpc_transport::Captures<'a>,
{
    let invocations = serve(client, provider)
        .await
        .context("failed to serve exports")?;
    let mut invocations = stream::select_all(
        invocations
            .into_iter()
            .map(|(instance, name, invocations)| invocations.map(move |res| (instance, name, res))),
    );
    let mut shutdown = pin!(shutdown);
    let mut tasks = JoinSet::new();
    loop {
        select! {
            Some((instance, name, res)) = invocations.next() => {
                match res {
                    Ok(fut) => {
                        tasks.spawn(async move {
                            if let Err(err) = fut.await {
                                warn!(?err, instance, name, "failed to serve invocation");
                            }
                            trace!(instance, name, "successfully served invocation");
                        });
                    },
                    Err(err) => {
                        warn!(?err, instance, name, "failed to accept invocation");
                    }
                }
            },
            () = &mut shutdown => {
                return Ok(())
            }
        }
    }
}

/// Source ID for a link
type SourceId = String;

#[derive(Clone)]
pub struct ProviderConnection {
    /// Links from the provider to other components, aka where the provider is the
    /// source of the link. Indexed by the component ID of the target
    pub source_links: Arc<RwLock<HashMap<LatticeTarget, InterfaceLinkDefinition>>>,
    /// Links from other components to the provider, aka where the provider is the
    /// target of the link. Indexed by the component ID of the source
    pub target_links: Arc<RwLock<HashMap<SourceId, InterfaceLinkDefinition>>>,

    /// NATS client used for performing RPCs
    pub nats: Arc<async_nats::Client>,

    /// Lattice name
    pub lattice: Arc<str>,
    pub host_id: String,
    pub provider_id: Arc<str>,

    /// Secrets XKeys
    pub provider_xkey: Arc<XKey>,
    pub host_xkey: Arc<XKey>,

    // TODO: Reference this field to get static config
    #[allow(unused)]
    pub config: HashMap<String, String>,
}

impl fmt::Debug for ProviderConnection {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderConnection")
            .field("provider_id", &self.provider_key())
            .field("host_id", &self.host_id)
            .field("lattice", &self.lattice)
            .finish()
    }
}

/// Extracts trace context from incoming headers
pub fn invocation_context(headers: &HeaderMap) -> Context {
    #[cfg(feature = "otel")]
    {
        let trace_context: TraceContext = convert_header_map_to_hashmap(headers)
            .into_iter()
            .collect::<Vec<(String, String)>>();
        attach_span_context(&trace_context);
    }
    // Determine source ID for the invocation
    let source_id = headers
        .get(WRPC_SOURCE_ID_HEADER_NAME)
        .map_or_else(|| "<unknown>".into(), ToString::to_string);
    Context {
        component: Some(source_id),
        tracing: convert_header_map_to_hashmap(headers),
    }
}

#[derive(Clone)]
pub struct WrpcClient {
    nats: wrpc_transport_nats::Client,
    timeout: Duration,
    provider_id: Arc<str>,
    target: Arc<str>,
}

impl wrpc_transport::Invoke for WrpcClient {
    type Context = Option<HeaderMap>;
    type Outgoing = <wrpc_transport_nats::Client as wrpc_transport::Invoke>::Outgoing;
    type Incoming = <wrpc_transport_nats::Client as wrpc_transport::Invoke>::Incoming;

    async fn invoke<P>(
        &self,
        cx: Self::Context,
        instance: &str,
        func: &str,
        params: Bytes,
        paths: impl AsRef<[P]> + Send,
    ) -> anyhow::Result<(Self::Outgoing, Self::Incoming)>
    where
        P: AsRef<[Option<usize>]> + Send + Sync,
    {
        let mut headers = cx.unwrap_or_default();
        headers.insert("source-id", &*self.provider_id);
        headers.insert("target-id", &*self.target);
        self.nats
            .timeout(self.timeout)
            .invoke(Some(headers), instance, func, params, paths)
            .await
    }
}

impl wrpc_transport::Serve for WrpcClient {
    type Context = Option<Context>;
    type Outgoing = <wrpc_transport_nats::Client as wrpc_transport::Serve>::Outgoing;
    type Incoming = <wrpc_transport_nats::Client as wrpc_transport::Serve>::Incoming;

    async fn serve(
        &self,
        instance: &str,
        func: &str,
        paths: impl Into<Arc<[Box<[Option<usize>]>]>> + Send,
    ) -> anyhow::Result<
        impl Stream<Item = anyhow::Result<(Self::Context, Self::Outgoing, Self::Incoming)>>
            + Send
            + 'static,
    > {
        let invocations = self.nats.serve(instance, func, paths).await?;
        Ok(invocations.and_then(|(cx, tx, rx)| async move {
            Ok((cx.as_ref().map(invocation_context), tx, rx))
        }))
    }
}

impl ProviderConnection {
    pub fn new(
        nats: impl Into<Arc<async_nats::Client>>,
        provider_id: impl Into<Arc<str>>,
        lattice: impl Into<Arc<str>>,
        host_id: String,
        config: HashMap<String, String>,
        provider_private_xkey: impl Into<Arc<XKey>>,
        host_public_xkey: impl Into<Arc<XKey>>,
    ) -> ProviderInitResult<ProviderConnection> {
        Ok(ProviderConnection {
            source_links: Arc::default(),
            target_links: Arc::default(),
            nats: nats.into(),
            lattice: lattice.into(),
            host_id,
            provider_id: provider_id.into(),
            config,
            provider_xkey: provider_private_xkey.into(),
            host_xkey: host_public_xkey.into(),
        })
    }

    /// Retrieve a wRPC client that can be used based on the NATS client of this connection
    ///
    /// # Arguments
    ///
    /// * `target` - Target ID to which invocations will be sent
    pub async fn get_wrpc_client(&self, target: &str) -> anyhow::Result<WrpcClient> {
        self.get_wrpc_client_custom(target, None).await
    }

    /// Retrieve a wRPC client that can be used based on the NATS client of this connection,
    /// customized with invocation timeout
    ///
    /// # Arguments
    ///
    /// * `target` - Target ID to which invocations will be sent
    /// * `timeout` - Timeout to be set on the client (by default if this is unset it will be 10 seconds)
    pub async fn get_wrpc_client_custom(
        &self,
        target: &str,
        timeout: Option<Duration>,
    ) -> anyhow::Result<WrpcClient> {
        let prefix = Arc::from(format!("{}.{target}", &self.lattice));
        let nats = wrpc_transport_nats::Client::new(
            Arc::clone(&self.nats),
            Arc::clone(&prefix),
            Some(prefix),
        )
        .await?;
        Ok(WrpcClient {
            nats,
            provider_id: Arc::clone(&self.provider_id),
            target: Arc::from(target),
            timeout: timeout.unwrap_or_else(|| Duration::from_secs(10)),
        })
    }

    /// Get the provider key that was assigned to this host at startup
    #[must_use]
    pub fn provider_key(&self) -> &str {
        &self.provider_id
    }

    /// Stores link in the [`ProviderConnection`], either as a source link or target link
    /// depending on if the provider is the source or target of the link
    pub async fn put_link(&self, ld: InterfaceLinkDefinition) {
        if ld.source_id == *self.provider_id {
            self.source_links
                .write()
                .await
                .insert(ld.target.to_string(), ld);
        } else {
            self.target_links
                .write()
                .await
                .insert(ld.source_id.to_string(), ld);
        }
    }

    /// Deletes link from the [`ProviderConnection`], either a source link or target link
    /// based on if the provider is the source or target of the link
    pub async fn delete_link(&self, source_id: &str, target: &str) {
        if source_id == &*self.provider_id {
            self.source_links.write().await.remove(target);
        } else if target == &*self.provider_id {
            self.target_links.write().await.remove(source_id);
        }
    }

    /// Returns true if the source is linked to this provider or if the provider is linked to the target
    /// on the given interface and link name
    pub async fn is_linked(
        &self,
        source_id: &str,
        target_id: &str,
        wit_namespace: &str,
        wit_package: &str,
        link_name: &str,
    ) -> bool {
        // Provider is the source of the link, so we check if the target is linked
        if &*self.provider_id == source_id {
            if let Some(link) = self.source_links.read().await.get(target_id) {
                // In older host versions, the wit_namespace and wit_package are not provided
                // so we should see if it's empty
                (link.wit_namespace.is_empty() || link.wit_namespace == wit_namespace)
                    && (link.wit_package.is_empty() || link.wit_package == wit_package)
                    && link.name == link_name
            } else {
                false
            }
        // Provider is the target of the link, so we check if the source is linked
        } else if &*self.provider_id == target_id {
            if let Some(link) = self.target_links.read().await.get(source_id) {
                // In older host versions, the wit_namespace and wit_package are not provided
                // so we should see if it's empty
                (link.wit_namespace.is_empty() || link.wit_namespace == wit_namespace)
                    && (link.wit_package.is_empty() || link.wit_package == wit_package)
                    && link.name == link_name
            } else {
                false
            }
        } else {
            // Shouldn't occur, but if the provider is neither source nor target then it's not linked
            false
        }
    }

    /// flush nats - called before main process exits
    pub(crate) async fn flush(&self) {
        if let Err(err) = self.nats.flush().await {
            error!(%err, "error flushing NATS client");
        }
    }
}
