//! Closing a connection whose statement may still be running on the server.

use std::time::Duration;

use tokio::sync::OwnedSemaphorePermit;
use tokio_postgres_rustls::MakeRustlsConnect;

/// Closes the connections whose statement may still be running: a call the
/// guest cancelled, rows it did not read to the end, a transaction dropped
/// mid-statement.
///
/// Dropping such a connection closes its socket, but Postgres runs the
/// statement on until it ends, reaches its time limit or next writes to the
/// socket. Until then its session counts against the server's
/// `max_connections` while nothing on the host counts it, so a loop of
/// cancelled calls could fill the server. Instead the server is asked to
/// cancel the statement, and the connection keeps its place in the pool and
/// its workload's permit until the server has ended it, for the connect
/// timeout at most. Then the connection is closed, never pooled.
#[derive(Clone)]
pub(crate) struct Canceller {
    /// The pool's TLS, which the cancel request is sent over too; `None`
    /// without TLS.
    tls: Option<MakeRustlsConnect>,
    limit: Duration,
}

impl Canceller {
    pub(crate) fn new(tls: Option<MakeRustlsConnect>, limit: Duration) -> Self {
        Self { tls, limit }
    }

    /// Cancel whatever `client` is running, then close it. `share`, the
    /// workload's permit for it, is given back with it.
    ///
    /// Without a runtime to run that on, the connection is closed at once, as
    /// it is if the runtime drops the task before it ends.
    pub(crate) fn close(
        &self,
        client: deadpool_postgres::Client,
        share: Option<OwnedSemaphorePermit>,
    ) {
        let closing = Closing {
            client: Some(client),
            _share: share,
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let canceller = self.clone();
            runtime.spawn(canceller.cancel(closing));
        }
    }

    async fn cancel(self, closing: Closing) {
        let Some(client) = closing.client.as_ref() else {
            return;
        };
        let token = client.cancel_token();
        let ended = async {
            let sent = match &self.tls {
                Some(tls) => token.cancel_query(tls.clone()).await,
                None => token.cancel_query(tokio_postgres::NoTls).await,
            };
            // Queued behind the cancelled statement, so answered, even with an
            // error, only once the server has ended it.
            sent.is_ok()
                && match client.batch_execute("").await {
                    Ok(()) => true,
                    Err(e) => e.as_db_error().is_some(),
                }
        };
        if !matches!(tokio::time::timeout(self.limit, ended).await, Ok(true)) {
            tracing::warn!(
                event = "retrieval_cancel_unconfirmed",
                limit = ?self.limit,
                "betty-retrieval asked the database to cancel a dropped statement and could not \
                 confirm it ended; its connection is closed"
            );
        }
    }
}

/// A connection on its way out, holding its pool slot and its workload's
/// permit. Dropped, it is taken out of the pool and closed.
struct Closing {
    client: Option<deadpool_postgres::Client>,
    _share: Option<OwnedSemaphorePermit>,
}

impl Drop for Closing {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            drop(deadpool_postgres::Object::take(client));
        }
    }
}
