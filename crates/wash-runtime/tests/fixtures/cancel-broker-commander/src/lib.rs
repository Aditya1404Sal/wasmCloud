//! Cancellation-broker fixture (commander side).
//!
//! Writes plan cancel flags and nothing else:
//!   - `GET /start?plan=<id>`  -> `set-cancel(<id>, false)`, clearing the flag
//!     so an id from an earlier run can be reused.
//!   - `GET /cancel?plan=<id>` -> `set-cancel(<id>, true)`, cancelling the plan.
//!
//! Paired with `cancel-broker-worker`. `integration_cancellation_broker` runs
//! the two on *separate hosts* against one NATS, so a cancel issued here can
//! only reach the worker through the shared JetStream KV bucket.

mod bindings {
    wit_bindgen::generate!({
        generate_all,
        async: ["export:wasi:http/handler@0.3.0#handle"],
    });
}

use bindings::betty_blocks::cancellation_broker::broker;
use bindings::exports::wasi::http::handler::Guest as Handler;
use bindings::wasi::http::types::{ErrorCode, Fields, Request, Response};

struct Component;

impl Handler for Component {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let path = request.get_path_with_query().unwrap_or_default();
        let Some(plan) = query_param(&path, "plan") else {
            return Ok(text_response(b"missing ?plan=<id>\n".to_vec()));
        };

        let body = if path.starts_with("/cancel") {
            broker::set_cancel(&plan, true);
            "cancel-set\n"
        } else if path.starts_with("/start") {
            broker::set_cancel(&plan, false);
            "cleared\n"
        } else {
            "unknown route; use /start or /cancel\n"
        };

        Ok(text_response(body.as_bytes().to_vec()))
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
