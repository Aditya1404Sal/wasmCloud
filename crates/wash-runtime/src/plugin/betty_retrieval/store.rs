//! `betty-blocks:retrieval/store`: auto-committed statements on any pooled
//! connection, `begin`, and the embedding space.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::Arc;

use futures::TryStreamExt as _;
use tokio::sync::{mpsc, oneshot};
use wasmtime::StoreContextMut;
use wasmtime::component::{
    Access, Accessor, Destination, FutureReader, Linker, Resource, StreamProducer, StreamReader,
    StreamResult,
};

use crate::engine::ctx::{ActiveCtx, SharedCtx, extract_active_ctx};

use super::bindings::betty_blocks::retrieval::{store, types};
use super::bindings::conversions::row_to_values;
use super::bindings::wasmcloud::postgres::types::Row;
use super::params::{self, Bound};
use super::tx::{self, TxHandle};
use super::{BettyRetrieval, PLUGIN_BETTY_RETRIEVAL_ID, SpaceInfo, errors};

/// How many fetched rows may wait between the fetch task and the guest before
/// the task stops reading, bounding host memory.
const ROW_CHANNEL_CAPACITY: usize = 16;

/// A statement's column names, its rows, and the outcome of fetching them.
pub(crate) type QueryResult = (
    Vec<String>,
    StreamReader<Row>,
    FutureReader<Result<(), types::Error>>,
);

/// The plugin serving the active call, `Arc`-cloned so it outlives the store
/// borrow.
pub(crate) fn plugin<U>(
    accessor: &Accessor<U, SharedCtx>,
) -> wasmtime::Result<Arc<BettyRetrieval>> {
    accessor.with(|mut access| {
        access
            .get()
            .try_get_plugin::<BettyRetrieval>(PLUGIN_BETTY_RETRIEVAL_ID)
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

/// A statement's columns, and the channels its fetch task feeds.
struct Streaming {
    columns: Vec<String>,
    rows: mpsc::Receiver<Row>,
    done: oneshot::Receiver<Result<(), types::Error>>,
}

/// Prepare the statement for its columns, then hand the connection to a task
/// that fetches the rows.
async fn start_query(
    plugin: &BettyRetrieval,
    sql: &str,
    params: Vec<types::Param>,
) -> Result<Streaming, types::Error> {
    let bound = params::resolve(plugin.embedder(), params).await?;
    let client = checkout(plugin).await?;
    let stmt = prepare_checked(&client, sql, &bound).await?;
    let columns = stmt
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    let (row_tx, rows) = mpsc::channel(ROW_CHANNEL_CAPACITY);
    let (done_tx, done) = oneshot::channel();
    tokio::spawn(async move {
        let _ = done_tx.send(drain_query(client, stmt, bound, row_tx).await);
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

async fn drain_query(
    client: deadpool_postgres::Client,
    stmt: tokio_postgres::Statement,
    bound: Vec<Bound>,
    row_tx: mpsc::Sender<Row>,
) -> Result<(), types::Error> {
    let rows = client
        .query_raw(&stmt, params::as_sql(&bound))
        .await
        .map_err(|e| errors::postgres(&e))?;
    let mut rows = std::pin::pin!(rows);
    while let Some(row) = rows.try_next().await.map_err(|e| errors::postgres(&e))? {
        let values = row_to_values(&row).map_err(errors::value_conversion)?;
        if row_tx.send(values).await.is_err() {
            // The guest dropped the stream.
            return Ok(());
        }
    }
    Ok(())
}

/// Forwards rows from the fetch task's channel to the guest, one per poll.
/// The stream ends when the channel closes; the completion future carries
/// the outcome.
struct RowStreamProducer {
    rows: mpsc::Receiver<Row>,
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
            Poll::Ready(Some(row)) => {
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
    sql: &str,
    params: Vec<types::Param>,
) -> Result<u64, types::Error> {
    let bound = params::resolve(plugin.embedder(), params).await?;
    let client = checkout(plugin).await?;
    let stmt = prepare_checked(&client, sql, &bound).await?;
    client
        .execute(&stmt, &params::as_sql(&bound))
        .await
        .map_err(|e| errors::postgres(&e))
}

async fn run_batch(plugin: &BettyRetrieval, sql: &str) -> Result<(), types::Error> {
    let client = checkout(plugin).await?;
    client
        .batch_execute(sql)
        .await
        .map_err(|e| errors::postgres(&e))
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
        let plugin = plugin(accessor)?;
        let Streaming {
            columns,
            rows,
            done,
        } = match start_query(&plugin, &sql, params).await {
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
        let plugin = plugin(accessor)?;
        Ok(run_execute(&plugin, &sql, params).await)
    }

    async fn batch(
        accessor: &Accessor<U, Self>,
        sql: String,
    ) -> wasmtime::Result<Result<(), types::Error>> {
        let plugin = plugin(accessor)?;
        Ok(run_batch(&plugin, &sql).await)
    }

    async fn begin(
        accessor: &Accessor<U, Self>,
    ) -> wasmtime::Result<Result<Resource<TxHandle>, types::Error>> {
        let plugin = plugin(accessor)?;
        match tx::begin(&plugin).await {
            Ok(handle) => Ok(Ok(
                accessor.with(|mut access| access.get().table.push(handle))?
            )),
            Err(e) => Ok(Err(e)),
        }
    }

    fn space(
        mut host: Access<'_, U, Self>,
    ) -> impl Future<Output = wasmtime::Result<types::Space>> + Send {
        let space = host
            .get()
            .try_get_plugin::<BettyRetrieval>(PLUGIN_BETTY_RETRIEVAL_ID)
            .map(|plugin| wit_space(plugin.space()));
        std::future::ready(space)
    }
}

/// Link `types` and `store` into a workload's linker.
pub(crate) fn add_to_linker(linker: &mut Linker<SharedCtx>) -> wasmtime::Result<()> {
    types::add_to_linker::<_, SharedCtx>(linker, extract_active_ctx)?;
    store::add_to_linker::<_, SharedCtx>(linker, extract_active_ctx)
}
