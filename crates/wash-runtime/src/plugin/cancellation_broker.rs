//! Per-plan cancellation registry backed by NATS JetStream KV.
//!
//! `set-cancel(plan-id, true|false)` writes a per-plan cancel flag keyed by
//! `plan-id`; `wait-cancel(plan-id)` resolves once that flag is `true`. Because
//! the KV bucket is a shared NATS backplane, the signal spans hosts at the edge:
//! a cancel issued on one host is observed by agents running on another.
//!
//! `wait-cancel` subscribes (`watch`, which only delivers future updates) and
//! *then* reads the current value, so a cancel published in the gap between the
//! two is still observed — no subscribe race.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::{
    engine::{
        ctx::{ActiveCtx, SharedCtx, extract_active_ctx},
        workload::WorkloadItem,
    },
    plugin::HostPlugin,
    wit::{WitInterface, WitWorld},
};
use async_nats::jetstream::{self, kv};
use futures::StreamExt;
use tokio::sync::OnceCell;

const PLUGIN_CANCELLATION_BROKER_ID: &str = "betty-blocks-cancellation-broker";
/// Single KV bucket holding one cancel flag per plan.
const CANCEL_BUCKET: &str = "genius_cancel";
/// Per-plan cancel flag values. A key holding `CANCEL_TRUE` means cancelled;
/// `CANCEL_FALSE` (written at plan start) or an absent key means not cancelled.
const CANCEL_TRUE: &[u8] = b"cancelled";
const CANCEL_FALSE: &[u8] = b"active";
/// Tombstones only need to outlive an in-flight generation; the TTL keeps the
/// bucket self-cleaning so cancelled plan ids don't accumulate forever.
const CANCEL_TTL: Duration = Duration::from_secs(3600);

mod bindings {
    crate::wasmtime::component::bindgen!({
        world: "cancellation-broker",
        imports: { default: async | trappable | tracing },
    });
}

/// Cheap-to-clone handle over the JetStream context and the lazily-opened
/// cancel bucket. Cloned out of the plugin before any `.await` so we never hold
/// a borrow of the host context across a suspension point.
#[derive(Clone)]
struct CancelStore {
    jetstream: jetstream::Context,
    bucket: Arc<OnceCell<kv::Store>>,
}

impl CancelStore {
    /// Open the cancel bucket once and cache it. Tries to attach to an existing
    /// bucket first, then creates it with a short TTL if absent.
    async fn store(&self) -> anyhow::Result<kv::Store> {
        self.bucket
            .get_or_try_init(|| async {
                match self.jetstream.get_key_value(CANCEL_BUCKET).await {
                    Ok(kv) => Ok(kv),
                    Err(_) => self
                        .jetstream
                        .create_key_value(kv::Config {
                            bucket: CANCEL_BUCKET.to_string(),
                            history: 1,
                            max_age: CANCEL_TTL,
                            ..Default::default()
                        })
                        .await
                        .map_err(anyhow::Error::from),
                }
            })
            .await
            .cloned()
    }
}

#[derive(Clone)]
pub struct CancellationBroker {
    inner: CancelStore,
}

impl CancellationBroker {
    pub fn new(client: &async_nats::Client) -> Self {
        Self {
            inner: CancelStore {
                jetstream: jetstream::new(client.clone()),
                bucket: Arc::new(OnceCell::new()),
            },
        }
    }
}

/// NATS KV keys allow `[-/_=.a-zA-Z0-9]`; any other byte in a (possibly
/// client-supplied) plan id is mapped to `_` so the key is always valid. Both
/// `set-cancel` and `wait-cancel` apply this identically, so the encoding
/// is internally consistent.
fn kv_key(plan_id: &str) -> String {
    plan_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '=' | '.' | '/' => c,
            _ => '_',
        })
        .collect()
}

// `set-cancel` is sync in the WIT but bindgen wraps every import in async
// because the macro is configured with `default: async`. We clone the store
// handle out under the borrow, then await the KV write outside it.
impl<'a> bindings::betty_blocks::cancellation_broker::broker::Host for ActiveCtx<'a> {
    async fn set_cancel(&mut self, plan_id: String, cancelled: bool) -> wasmtime::Result<()> {
        let store_handle = {
            let Some(plugin) = self.get_plugin::<CancellationBroker>(PLUGIN_CANCELLATION_BROKER_ID)
            else {
                wasmtime::bail!("CancellationBroker plugin not found in context");
            };
            plugin.inner.clone()
        };

        let value: &[u8] = if cancelled { CANCEL_TRUE } else { CANCEL_FALSE };
        match store_handle.store().await {
            Ok(store) => {
                if let Err(e) = store.put(kv_key(&plan_id), value.into()).await {
                    tracing::error!(plan_id, cancelled, error = %e, "failed to write plan cancel flag to KV");
                }
            }
            Err(e) => {
                tracing::error!(plan_id, cancelled, error = %e, "failed to open cancel bucket");
            }
        }
        Ok(())
    }
}

// Async funcs live on `HostWithStore`, which bindgen invokes with an `Accessor`
// so the body can suspend without holding a store borrow. We extract the cloned
// `CancelStore` synchronously under `accessor.with(...)`, then await outside it.
impl<T: 'static> bindings::betty_blocks::cancellation_broker::broker::HostWithStore<T>
    for SharedCtx
{
    async fn wait_cancel(
        accessor: &wasmtime::component::Accessor<T, Self>,
        plan_id: String,
    ) -> wasmtime::Result<()> {
        let store_handle = accessor.with(|mut access| {
            let ctx = access.get();
            let Some(plugin) = ctx.get_plugin::<CancellationBroker>(PLUGIN_CANCELLATION_BROKER_ID)
            else {
                return Err(wasmtime::Error::msg(
                    "CancellationBroker plugin not found in context",
                ));
            };
            Ok(plugin.inner.clone())
        })?;

        let store = match store_handle.store().await {
            Ok(store) => store,
            Err(e) => {
                // KV is unreachable. Returning would be read by the racing guest
                // `select` as "cancelled" and abort a healthy generation, so we
                // never resolve instead: real work proceeds, and this future is
                // dropped when its select arm loses.
                tracing::error!(plan_id, error = %e, "cancel bucket unavailable; wait-cancel will not resolve");
                return std::future::pending().await;
            }
        };

        let key = kv_key(&plan_id);

        // Subscribe first (DeliverPolicy::New = future updates only) so any put
        // from here on is buffered on the watch...
        let mut watch = match store.watch(&key).await {
            Ok(watch) => watch,
            Err(e) => {
                tracing::error!(plan_id, error = %e, "failed to watch cancel key; wait-cancel will not resolve");
                return std::future::pending().await;
            }
        };

        // ...then read the current value to catch a cancel that already landed.
        match store.get(&key).await {
            Ok(Some(value)) if value.as_ref() == CANCEL_TRUE => return Ok(()),
            // Absent or CANCEL_FALSE (cleared at plan start) -> not cancelled.
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(plan_id, error = %e, "cancel key read failed; falling back to watch");
            }
        }

        loop {
            match watch.next().await {
                Some(Ok(entry))
                    if matches!(entry.operation, kv::Operation::Put)
                        && entry.value.as_ref() == CANCEL_TRUE =>
                {
                    return Ok(());
                }
                // A CANCEL_FALSE write or a Delete is not a cancel signal; skip.
                Some(Ok(_)) => continue,
                Some(Err(e)) => {
                    tracing::warn!(plan_id, error = %e, "cancel watch error; continuing");
                    continue;
                }
                // Watch ended unexpectedly: don't spuriously cancel.
                None => return std::future::pending().await,
            }
        }
    }
}

#[async_trait::async_trait]
impl HostPlugin for CancellationBroker {
    fn id(&self) -> &'static str {
        PLUGIN_CANCELLATION_BROKER_ID
    }

    fn world(&self) -> WitWorld {
        WitWorld {
            imports: HashSet::from([WitInterface::from(
                "betty-blocks:cancellation-broker/broker@0.2.0",
            )]),
            ..Default::default()
        }
    }

    async fn on_workload_item_bind<'a>(
        &self,
        item: &mut WorkloadItem<'a>,
        interfaces: HashSet<WitInterface>,
    ) -> anyhow::Result<()> {
        let Some(_interface) = interfaces
            .iter()
            .find(|i| i.namespace == "betty-blocks" && i.package == "cancellation-broker")
        else {
            return Ok(());
        };

        bindings::betty_blocks::cancellation_broker::broker::add_to_linker::<_, SharedCtx>(
            item.linker(),
            extract_active_ctx,
        )?;

        Ok(())
    }
}
