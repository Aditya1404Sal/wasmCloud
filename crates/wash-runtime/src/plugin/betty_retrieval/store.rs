//! `betty-blocks:retrieval/store`: auto-committed statements on any pooled
//! connection, `begin`, and the embedding space.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::Arc;

use futures::TryStreamExt as _;
use tokio::sync::{OwnedSemaphorePermit, mpsc, oneshot};
use wasmtime::StoreContextMut;
use wasmtime::component::{
    Access, Accessor, Destination, FutureReader, Linker, Resource, StreamProducer, StreamReader,
    StreamResult,
};

use crate::engine::ctx::{ActiveCtx, SharedCtx, extract_active_ctx};

use super::bindings::betty_blocks::retrieval::{store, types};
use super::bindings::conversions::row_to_values;
use super::bindings::wasmcloud::postgres::types::Row;
use super::cancel::Canceller;
use super::cost::{self, RowBuffer};
use super::params::{self, Bound};
use super::tx::{self, TxHandle};
use super::{BettyRetrieval, PLUGIN_BETTY_RETRIEVAL_ID, SpaceInfo, errors};

/// How many fetched rows may wait between the fetch task and the guest before
/// the task stops reading. Together they may cost at most
/// [`cost::ROW_BUFFER_MAX_BYTES`], and one at most [`cost::ROW_MAX_BYTES`].
const ROW_CHANNEL_CAPACITY: usize = 16;

/// A fetched row on its way to the guest, holding its room in the query's
/// [`RowBuffer`] until the guest takes it.
type Buffered = (Row, OwnedSemaphorePermit);

/// A statement's column names, its rows, and the outcome of fetching them.
pub(crate) type QueryResult = (
    Vec<String>,
    StreamReader<Row>,
    FutureReader<Result<(), types::Error>>,
);

/// The plugin serving the active call, `Arc`-cloned so it outlives the store
/// borrow, and the workload making it, whose share of the pool a new
/// connection counts against.
pub(crate) fn caller<U>(
    accessor: &Accessor<U, SharedCtx>,
) -> wasmtime::Result<(Arc<BettyRetrieval>, Arc<str>)> {
    accessor.with(|mut access| {
        let ctx = access.get();
        let plugin = ctx.try_get_plugin::<BettyRetrieval>(PLUGIN_BETTY_RETRIEVAL_ID)?;
        wasmtime::Result::Ok((plugin, Arc::clone(&ctx.workload_id)))
    })
}

pub(crate) async fn checkout(
    plugin: &BettyRetrieval,
) -> Result<deadpool_postgres::Client, types::Error> {
    let pool = plugin
        .pool()
        .map_err(|e| errors::connection_failed(format!("{e:#}")))?;
    pool.get().await.map_err(|e| errors::pool(&e))
}

/// A pooled connection lent to one auto-committed statement.
///
/// It goes back to the pool only through [`Lent::give_back`], once the server
/// has answered. Dropped before that (the call cancelled, a stream not read to
/// its end, rows that could not be decoded) it is closed instead: the server
/// may still be running the statement or sending its rows, and whoever
/// borrowed the connection next, from any workload, would wait behind them.
/// The server is asked to cancel the statement first, and the connection
/// keeps its place in the pool until it has (see [`Canceller`]).
///
/// It also holds one of its workload's permits, given back with it.
struct Lent {
    /// `None` once given back.
    client: Option<deadpool_postgres::Client>,
    /// `None` once given back or handed to the canceller.
    share: Option<OwnedSemaphorePermit>,
    canceller: Canceller,
}

impl Lent {
    /// One of `workload`'s permits first, so a workload over its share waits
    /// without taking a connection the others could use.
    async fn checkout(plugin: &BettyRetrieval, workload: &Arc<str>) -> Result<Self, types::Error> {
        let canceller = plugin
            .canceller()
            .map_err(|e| errors::connection_failed(format!("{e:#}")))?
            .clone();
        let share = plugin.shares().acquire(workload).await?;
        Ok(Self {
            client: Some(checkout(plugin).await?),
            share: Some(share),
            canceller,
        })
    }

    fn client(&self) -> Result<&deadpool_postgres::Client, types::Error> {
        self.client.as_ref().ok_or_else(|| {
            errors::connection_failed("this connection was already given back".to_string())
        })
    }

    /// Back to the pool, which resets the session before lending it again.
    fn give_back(mut self) {
        drop(self.client.take());
    }
}

impl Drop for Lent {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            self.canceller.close(client, self.share.take());
        }
    }
}

/// A statement's columns, and the channels its fetch task feeds.
struct Streaming {
    columns: Vec<String>,
    rows: mpsc::Receiver<Buffered>,
    done: oneshot::Receiver<Result<(), types::Error>>,
}

/// Prepare the statement for its columns, then hand the connection to a task
/// that fetches the rows.
async fn start_query(
    plugin: &BettyRetrieval,
    workload: &Arc<str>,
    sql: &str,
    params: Vec<types::Param>,
) -> Result<Streaming, types::Error> {
    let bound = params::resolve(plugin.embedder(), plugin.embed_gate(), workload, params).await?;
    let lent = Lent::checkout(plugin, workload).await?;
    let stmt = match prepare_checked(lent.client()?, sql, &bound).await {
        Ok(stmt) => stmt,
        Err(e) => {
            lent.give_back();
            return Err(e);
        }
    };
    let columns = stmt
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    let (row_tx, rows) = mpsc::channel(ROW_CHANNEL_CAPACITY);
    let (done_tx, done) = oneshot::channel();
    let read_pause = plugin.read_pause();
    tokio::spawn(async move {
        let _ = done_tx.send(drain_query(lent, stmt, bound, row_tx, read_pause).await);
    });
    Ok(Streaming {
        columns,
        rows,
        done,
    })
}

/// Prepare `sql`, refusing it before it runs if its placeholders and the bound
/// parameters differ in number.
async fn prepare_checked(
    client: &deadpool_postgres::Client,
    sql: &str,
    bound: &[Bound],
) -> Result<tokio_postgres::Statement, types::Error> {
    let stmt = client
        .prepare(sql)
        .await
        .map_err(|e| errors::postgres(&e))?;
    params::check_count(stmt.params().len(), bound.len())?;
    Ok(stmt)
}

/// Fetch every row into `row_tx`. The connection goes back to the pool once
/// the server has ended the statement; a stream that stops early, because the
/// guest dropped it or a row could not be decoded, drops `lent` and closes it.
/// A dropped stream is noticed at once, even while the server is still
/// working on the next row.
///
/// A guest that reads no row for `read_pause` (one that awaits the result
/// before reading the rows, say) would otherwise hold its connection, and a
/// permit of its share, for as long as it lived. The query is ended instead,
/// and the result says why.
///
/// The rows waiting for the guest are held to [`cost::ROW_BUFFER_MAX_BYTES`]
/// together, and a row over [`cost::ROW_MAX_BYTES`] ends the query before it
/// is decoded.
async fn drain_query(
    lent: Lent,
    stmt: tokio_postgres::Statement,
    bound: Vec<Bound>,
    row_tx: mpsc::Sender<Buffered>,
    read_pause: Option<std::time::Duration>,
) -> Result<(), types::Error> {
    let buffer = RowBuffer::new();
    let rows = match lent
        .client()?
        .query_raw(&stmt, params::as_sql(&bound))
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            lent.give_back();
            return Err(errors::postgres(&e));
        }
    };
    let mut rows = std::pin::pin!(rows);
    loop {
        let next = tokio::select! {
            next = rows.try_next() => next,
            // The guest dropped the stream.
            () = row_tx.closed() => return Ok(()),
        };
        match next {
            Ok(Some(row)) => {
                let cost = cost::row_cost(&row);
                if cost > cost::ROW_MAX_BYTES {
                    return Err(errors::other(format!(
                        "a row of the query is larger than {} bytes, the most one row may be; \
                         select less of it, or read it in pieces",
                        cost::ROW_MAX_BYTES
                    )));
                }
                let values = row_to_values(&row).map_err(errors::value_conversion)?;
                drop(row);
                let deliver = async {
                    let room = buffer.reserve(cost).await.map_err(|_| ())?;
                    row_tx.send((values, room)).await.map_err(|_| ())
                };
                let sent = match read_pause {
                    Some(pause) => tokio::time::timeout(pause, deliver).await.map_err(|_| {
                        errors::other(format!(
                            "no row of the query was read for {pause:?}, so the host ended it; \
                             read the rows before awaiting the result"
                        ))
                    })?,
                    None => deliver.await,
                };
                if sent.is_err() {
                    // The guest dropped the stream.
                    return Ok(());
                }
            }
            Ok(None) => {
                lent.give_back();
                return Ok(());
            }
            // The server ended the statement with this error.
            Err(e) => {
                lent.give_back();
                return Err(errors::postgres(&e));
            }
        }
    }
}

/// Forwards rows from the fetch task's channel to the guest, one per poll,
/// giving back each row's room in the query's buffer as the guest takes it.
/// The stream ends when the channel closes; the completion future carries
/// the outcome.
struct RowStreamProducer {
    rows: mpsc::Receiver<Buffered>,
}

impl<D: 'static> StreamProducer<D> for RowStreamProducer {
    type Item = Row;
    type Buffer = Option<Row>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, D>,
        mut dst: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let this = self.get_mut();
        if finish {
            // Dropping the receiver tells the fetch task to stop.
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        if dst.remaining(&mut store) == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        match this.rows.poll_recv(cx) {
            Poll::Ready(Some((row, _room))) => {
                dst.set_buffer(Some(row));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Ready(None) => Poll::Ready(Ok(StreamResult::Dropped)),
            Poll::Pending => Poll::Pending,
        }
    }
}

async fn run_execute(
    plugin: &BettyRetrieval,
    workload: &Arc<str>,
    sql: &str,
    params: Vec<types::Param>,
) -> Result<u64, types::Error> {
    let bound = params::resolve(plugin.embedder(), plugin.embed_gate(), workload, params).await?;
    let lent = Lent::checkout(plugin, workload).await?;
    let outcome = match prepare_checked(lent.client()?, sql, &bound).await {
        Ok(stmt) => lent
            .client()?
            .execute(&stmt, &params::as_sql(&bound))
            .await
            .map_err(|e| errors::postgres(&e)),
        Err(e) => Err(e),
    };
    lent.give_back();
    outcome
}

async fn run_batch(
    plugin: &BettyRetrieval,
    workload: &Arc<str>,
    sql: &str,
) -> Result<(), types::Error> {
    let lent = Lent::checkout(plugin, workload).await?;
    let outcome = lent
        .client()?
        .batch_execute(sql)
        .await
        .map_err(|e| errors::postgres(&e));
    lent.give_back();
    outcome
}

fn wit_space(space: &SpaceInfo) -> types::Space {
    types::Space {
        space_id: space.space_id.clone(),
        model_id: space.model_id.clone(),
        dimension: space.dimension,
    }
}

impl types::Host for ActiveCtx<'_> {}
impl store::Host for ActiveCtx<'_> {}

impl<U> store::HostWithStore<U> for SharedCtx {
    async fn query(
        accessor: &Accessor<U, Self>,
        sql: String,
        params: Vec<types::Param>,
    ) -> wasmtime::Result<Result<QueryResult, types::Error>> {
        let (plugin, workload) = caller(accessor)?;
        let Streaming {
            columns,
            rows,
            done,
        } = match start_query(&plugin, &workload, &sql, params).await {
            Ok(streaming) => streaming,
            Err(e) => return Ok(Err(e)),
        };
        accessor.with(|mut access| {
            let rows = StreamReader::new(&mut access, RowStreamProducer { rows })?;
            let done = FutureReader::new(&mut access, done)?;
            wasmtime::Result::Ok(Ok((columns, rows, done)))
        })
    }

    async fn execute(
        accessor: &Accessor<U, Self>,
        sql: String,
        params: Vec<types::Param>,
    ) -> wasmtime::Result<Result<u64, types::Error>> {
        let (plugin, workload) = caller(accessor)?;
        Ok(run_execute(&plugin, &workload, &sql, params).await)
    }

    async fn batch(
        accessor: &Accessor<U, Self>,
        sql: String,
    ) -> wasmtime::Result<Result<(), types::Error>> {
        let (plugin, workload) = caller(accessor)?;
        Ok(run_batch(&plugin, &workload, &sql).await)
    }

    async fn begin(
        accessor: &Accessor<U, Self>,
    ) -> wasmtime::Result<Result<Resource<TxHandle>, types::Error>> {
        let (plugin, workload) = caller(accessor)?;
        match tx::begin(&plugin, &workload).await {
            Ok(handle) => Ok(Ok(
                accessor.with(|mut access| access.get().table.push(handle))?
            )),
            Err(e) => Ok(Err(e)),
        }
    }

    fn space(
        mut host: Access<'_, U, Self>,
    ) -> impl Future<Output = wasmtime::Result<types::Space>> + Send {
        // `space` returns a record, not a result: with no model there is no
        // space to report, and reporting a made-up one would let a component
        // tag its data with it. The call traps instead, saying why.
        let space = host
            .get()
            .try_get_plugin::<BettyRetrieval>(PLUGIN_BETTY_RETRIEVAL_ID)
            .and_then(|plugin| match plugin.space() {
                Some(space) => Ok(wit_space(space)),
                None => Err(wasmtime::Error::msg(format!(
                    "betty-retrieval has no embedding space: the model did not load when the \
                     host started: {}",
                    plugin.model_failure().unwrap_or("no cause was recorded")
                ))),
            });
        std::future::ready(space)
    }
}

/// Link `types` and `store` into a workload's linker.
pub(crate) fn add_to_linker(linker: &mut Linker<SharedCtx>) -> wasmtime::Result<()> {
    types::add_to_linker::<_, SharedCtx>(linker, extract_active_ctx)?;
    store::add_to_linker::<_, SharedCtx>(linker, extract_active_ctx)
}
