//! Generated bindings for `betty-blocks:retrieval`, plus the tokio-postgres
//! value conversions the `store` interface needs once statement execution
//! lands.

// Plugin-local rather than under the shared `wit/` tree: any interface there
// that `use`s `wasmcloud:postgres/types@0.2.0` makes the stock postgres
// plugin's `bindgen!` panic.
crate::wasmtime::component::bindgen!({
    path: "src/plugin/betty_retrieval/wit",
    world: "plugin-imports",
    imports: { default: store | async | trappable | tracing },
});

/// The same `pg-value` <-> tokio-postgres conversions as the stock postgres
/// plugin (see `wasmcloud_postgres/conversions.rs`'s header), applied to this
/// plugin's own generated types.
pub(crate) mod conversions {
    use super::wasmcloud::postgres::types::{
        Date, HashableF64, MacAddressEui48, MacAddressEui64, Numeric, Offset, PgValue, Time,
        Timestamp, TimestampTz,
    };

    include!("../wasmcloud_postgres/conversions.rs");

    /// Convert one fetched row into WIT `pg-value`s, in column order. A
    /// `String` error names the column that failed to convert; the next
    /// task maps it onto `betty-blocks:retrieval/types.error` and calls this
    /// from `store`'s query execution.
    #[allow(dead_code)]
    pub(crate) fn row_to_values(r: &Row) -> Result<Vec<PgValue>, String> {
        (0..r.len())
            .map(|idx| r.try_get(idx).map_err(|e| format!("column {idx}: {e}")))
            .collect()
    }
}
