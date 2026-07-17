//! Cancellation-broker fixture (worker/agent side).
//!
//! `GET /run?plan=<id>` starts a paced ten-step job, emitting `"1\n"`..`"10\n"`
//! one per second into the HTTP response body. Each step races the tick against
//! the host's `wait-cancel(<id>)`: if the cancel resolves first the job stops
//! early and writes `"cancelled\n"` as its last line.
//!
//! That race is the whole point. `wait-cancel` resolving *means* cancelled, so
//! the host only resolves it on a genuine cancel flag and leaves it pending on
//! any KV failure — otherwise an infrastructure blip would abort this loop.
//!
//! The flag itself is set by `cancel-broker-commander`, deliberately running on
//! a different host in `integration_cancellation_broker`: the signal travels
//! through the shared NATS JetStream KV bucket, not through this process.

mod bindings {
    wit_bindgen::generate!({
        generate_all,
        async: [
            "import:betty-blocks:cancellation-broker/broker@0.2.0#wait-cancel",
            "import:wasi:clocks/monotonic-clock@0.3.0#wait-for",
            "export:wasi:http/handler@0.3.0#handle",
        ],
    });
}

use bindings::betty_blocks::cancellation_broker::broker;
use bindings::exports::wasi::http::handler::Guest as Handler;
use bindings::wasi::clocks::monotonic_clock;
use bindings::wasi::http::types::{ErrorCode, Fields, Request, Response};
use futures::future::{Either, select};

/// One step per second, matching `cancellable-producer`'s pacing: slow enough
/// that a cancel fired mid-run lands well inside the job's lifetime.
const TICK_NS: u64 = 1_000_000_000;
const STEPS: u32 = 10;

struct Component;

impl Handler for Component {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let path = request.get_path_with_query().unwrap_or_default();
        let Some(plan) = query_param(&path, "plan") else {
            return Ok(text_response(b"missing ?plan=<id>\n".to_vec()));
        };

        let (mut tx, rx) = bindings::wit_stream::new::<u8>();
        let (trailers_tx, trailers_rx) = bindings::wit_future::new(|| todo!());

        // Stream the steps from a background task so the response (and its body
        // reader) can be returned immediately and drained as work progresses.
        wit_bindgen::spawn_local(async move {
            // Subscribe once, outside the loop: a cancel that lands mid-tick
            // must still be observed by the next step's race rather than being
            // dropped and re-subscribed (which would reopen the very race the
            // host's subscribe-then-read ordering closes).
            let cancel = broker::wait_cancel(plan);
            futures::pin_mut!(cancel);

            let mut cancelled = false;
            for step in 1..=STEPS {
                let tick = monotonic_clock::wait_for(TICK_NS);
                futures::pin_mut!(tick);
                // `cancel.as_mut()` re-borrows the pinned future so it survives
                // a losing race and keeps waiting across steps.
                match select(cancel.as_mut(), tick).await {
                    Either::Left(_) => {
                        cancelled = true;
                        break;
                    }
                    Either::Right(_) => {}
                }
                tx.write_all(format!("{step}\n").into_bytes()).await;
            }

            if cancelled {
                tx.write_all(b"cancelled\n".to_vec()).await;
            }
            drop(tx);
            if let Err(e) = trailers_tx.write(Ok(None)).await {
                let _ = e;
            }
        });

        let (response, _result) = Response::new(Fields::new(), Some(rx), trailers_rx);
        Ok(response)
    }
}

fn query_param(path: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    path.split_once('?')
        .and_then(|(_, query)| query.split('&').find_map(|kv| kv.strip_prefix(&prefix)))
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn text_response(body: Vec<u8>) -> Response {
    let (mut tx, rx) = bindings::wit_stream::new::<u8>();
    let (trailers_tx, trailers_rx) = bindings::wit_future::new(|| todo!());
    wit_bindgen::spawn_local(async move {
        tx.write_all(body).await;
        drop(tx);
        let _ = trailers_tx.write(Ok(None)).await;
    });
    let (response, _result) = Response::new(Fields::new(), Some(rx), trailers_rx);
    response
}

bindings::export!(Component with_types_in bindings);
