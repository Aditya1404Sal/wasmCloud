//! Server-Sent Events bridge for components that export
//! `betty-blocks:sse/handler@0.1`.
//!
//! Flow:
//!   1. The HTTP router decides this request should go to SSE (it
//!      carries `Accept: text/event-stream` and the workload exports
//!      the SSE handler).
//!   2. Open a `200 OK` response with `Content-Type: text/event-stream`
//!      and a streaming body channel we own.
//!   3. Instantiate the component, invoke its `handle` export with the
//!      captured `subscribe-request`. The component returns a
//!      `StreamReader<Event>` for events to send to the client.
//!   4. In a detached task, pipe the component's event stream into the
//!      response body, framing each `event` record as the canonical
//!      SSE wire form (`event:`, `id:`, `data:`, `retry:`, blank line).
//!      Closing happens when the component drops the writer or hyper
//!      detects the client disconnected.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use crate::engine::ctx::SharedCtx;
use crate::engine::workload::ResolvedWorkload;
use crate::observability::FuelConsumptionMeter;
use bytes::Bytes;
use futures::SinkExt;
use futures::channel::mpsc;
use http_body_util::{BodyExt, StreamBody};
use tokio::sync::oneshot;
use wasmtime::Store;
use wasmtime::component::{InstancePre, Source, StreamConsumer, StreamResult};
use wasmtime_wasi_http::p2::bindings::http::types::ErrorCode as WasiHttpErrorCode;
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;

mod bindings {
    crate::wasmtime::component::bindgen!({
        world: "sse",
        exports: { default: async },
    });
}

use bindings::SsePre;
use bindings::exports::betty_blocks::sse::handler::{
    Event as GuestEvent, SubscribeRequest as GuestSubscribeRequest,
};

const SSE_WRITE_QUEUE_CAPACITY: usize = 8;

/// Bridge an HTTP request into a component that exports
/// `betty-blocks:sse/handler@0.1`. The response is `200 OK` with a
/// streaming body whose chunks are SSE-framed events produced by the
/// component.
pub async fn handle_sse_request(
    workload_handle: ResolvedWorkload,
    store: Store<SharedCtx>,
    pre: InstancePre<SharedCtx>,
    req: hyper::Request<hyper::body::Incoming>,
    fuel_meter: FuelConsumptionMeter,
) -> anyhow::Result<hyper::Response<HyperOutgoingBody>> {
    let _ = &fuel_meter;

    let sse_pre = match SsePre::new(pre) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(err = %e, "failed to build SsePre");
            return Ok(error_resp(500, "sse handler bindgen failed"));
        }
    };

    let guest_req = build_guest_request(&req);

    // Create the body channel BEFORE we spawn — the response head must
    // be returnable synchronously from this function.
    let (body_tx, body_rx) = mpsc::channel::<Result<hyper::body::Frame<Bytes>, WasiHttpErrorCode>>(
        SSE_WRITE_QUEUE_CAPACITY,
    );

    tokio::spawn(run_sse_session(
        workload_handle,
        store,
        sse_pre,
        guest_req,
        body_tx,
    ));

    let body = StreamBody::new(body_rx).boxed_unsync();
    let mut resp = hyper::Response::builder()
        .status(hyper::StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "text/event-stream")
        .header(hyper::header::CACHE_CONTROL, "no-cache")
        // Disable proxy buffering (e.g. nginx) so events flush promptly.
        .header("X-Accel-Buffering", "no")
        .body(HyperOutgoingBody::new(body))
        .expect("SSE 200 response with static headers is well-formed");
    // Mirror server behaviour for streaming responses: tell hyper not to
    // expect a content-length.
    resp.headers_mut().remove(hyper::header::CONTENT_LENGTH);
    Ok(resp)
}

fn error_resp(status: u16, msg: &'static str) -> hyper::Response<HyperOutgoingBody> {
    let body = http_body_util::Full::new(Bytes::from_static(msg.as_bytes()))
        .map_err(|never: std::convert::Infallible| match never {})
        .boxed_unsync();
    hyper::Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain")
        .body(HyperOutgoingBody::new(body))
        .expect("error response is well-formed")
}

fn build_guest_request(req: &hyper::Request<hyper::body::Incoming>) -> GuestSubscribeRequest {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let headers = req
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str().to_string(), value.as_bytes().to_vec()))
        .collect();
    let last_event_id = req
        .headers()
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    GuestSubscribeRequest {
        path,
        query,
        headers,
        last_event_id,
    }
}

async fn run_sse_session(
    workload_handle: ResolvedWorkload,
    mut store: Store<SharedCtx>,
    sse_pre: SsePre<SharedCtx>,
    guest_req: GuestSubscribeRequest,
    body_tx: mpsc::Sender<Result<hyper::body::Frame<Bytes>, WasiHttpErrorCode>>,
) {
    let store_id = store.data().active_ctx.store_id.clone();

    let instance = match sse_pre.instantiate_async(&mut store).await {
        Ok(i) => i,
        Err(e) => {
            tracing::error!(err = %e, "failed to instantiate sse component");
            return;
        }
    };

    // close_tx fires when the consumer reaches end-of-stream — either the
    // component dropped its writer or hyper hung up on us. Awaiting it
    // keeps `run_concurrent` alive long enough for streaming writes to
    // actually drain into the hyper body channel; without this the call
    // future completes the moment `pipe` registers the consumer, the
    // store is dropped, and the body channel closes before any frame
    // was sent.
    let (close_tx, close_rx) = oneshot::channel::<()>();
    let consumer = SseEventConsumer::new(body_tx, Some(close_tx));

    let result = store
        .run_concurrent(async move |accessor| {
            let call = instance
                .betty_blocks_sse_handler()
                .call_handle(accessor, guest_req)
                .await;
            match call {
                Ok(Ok(outgoing)) => {
                    let pipe_res = accessor.with(|mut s| outgoing.pipe(&mut s, consumer));
                    if let Err(e) = pipe_res {
                        tracing::error!(err = ?e, "sse pipe to response body failed");
                        return Ok::<_, anyhow::Error>(());
                    }
                    let _ = close_rx.await;
                    Ok(())
                }
                Ok(Err(msg)) => {
                    tracing::warn!(reason = %msg, "sse handler returned Err; closing body");
                    drop(close_rx);
                    Ok(())
                }
                Err(e) => {
                    tracing::error!(err = ?e, "sse handler trapped");
                    drop(close_rx);
                    Ok(())
                }
            }
        })
        .await;

    if let Err(e) = result {
        tracing::error!(err = ?e, "sse run_concurrent failed");
    }

    workload_handle.clear_exporter_instances_for_store(&store_id);
}

// ---------------------------------------------------------------------
// Consumer: guest `outgoing: stream<event>` → HTTP response body chunks
// ---------------------------------------------------------------------

struct SseEventConsumer {
    body_tx: mpsc::Sender<Result<hyper::body::Frame<Bytes>, WasiHttpErrorCode>>,
    close_tx: Option<oneshot::Sender<()>>,
    closed: bool,
}

impl SseEventConsumer {
    fn new(
        body_tx: mpsc::Sender<Result<hyper::body::Frame<Bytes>, WasiHttpErrorCode>>,
        close_tx: Option<oneshot::Sender<()>>,
    ) -> Self {
        Self {
            body_tx,
            close_tx,
            closed: false,
        }
    }

    fn signal_close(&mut self) {
        if let Some(tx) = self.close_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for SseEventConsumer {
    fn drop(&mut self) {
        self.signal_close();
    }
}

impl StreamConsumer<SharedCtx> for SseEventConsumer {
    type Item = GuestEvent;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<SharedCtx>,
        mut src: Source<Self::Item>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        use wasmtime::AsContextMut;

        if self.closed {
            return Poll::Ready(Ok(StreamResult::Dropped));
        }

        match self.body_tx.poll_ready_unpin(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => {
                tracing::debug!(err = %e, "sse client disconnected");
                self.closed = true;
                self.signal_close();
                return Poll::Ready(Ok(StreamResult::Dropped));
            }
            Poll::Ready(Ok(())) => {}
        }

        let mut buf: Vec<GuestEvent> = Vec::with_capacity(1);
        src.read(store.as_context_mut(), &mut buf)?;
        if buf.is_empty() {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let event = buf.remove(0);
        let bytes = format_sse_event(event);
        let frame = hyper::body::Frame::data(bytes);

        if let Err(e) = self.body_tx.start_send_unpin(Ok(frame)) {
            tracing::debug!(err = %e, "sse body channel closed");
            self.closed = true;
            self.signal_close();
            return Poll::Ready(Ok(StreamResult::Dropped));
        }

        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// Serialise an SSE event record into its wire-form representation per
/// the HTML SSE spec: optional `event:` / `id:` / `retry:` field lines,
/// one or more `data:` lines, terminating blank line.
fn format_sse_event(event: GuestEvent) -> Bytes {
    let mut out = String::with_capacity(event.data.len() + 32);
    if let Some(name) = event.name.as_deref()
        && !name.is_empty()
    {
        out.push_str("event: ");
        out.push_str(name);
        out.push('\n');
    }
    if let Some(id) = event.id.as_deref() {
        out.push_str("id: ");
        out.push_str(id);
        out.push('\n');
    }
    if let Some(retry) = event.retry_ms {
        out.push_str("retry: ");
        out.push_str(&retry.to_string());
        out.push('\n');
    }
    // The spec allows multi-line `data:` by emitting one line per
    // newline; the client reassembles with intervening newlines.
    for line in event.data.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    Bytes::from(out.into_bytes())
}
