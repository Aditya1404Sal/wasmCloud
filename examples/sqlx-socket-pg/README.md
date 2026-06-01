# sqlx-socket-pg

A wasmCloud example showing how to reach a real PostgreSQL service from inside
a sandboxed WASIP3 component. It is the Postgres twin of `examples/sqlx-socket`:
the HTTP component is unchanged, while the long-running service owns a
`sqlx::PgPool`, creates a small todo schema, and reaches Postgres through an
explicit `dev.socket_tunnels` rule.

This example exercises:

- The **`dev.socket_tunnels`** block — explicit sandbox→host TCP allowlist
- A long-lived `sqlx::PgPool` held in a service workload
- SQLx's typed `query_as` + `#[derive(FromRow)]` mapping
- A transaction (`pool.begin()`) for "create todo + attach tags" atomicity
- A many-to-many JOIN with Postgres `string_agg` for tag aggregation
- The same live UI as `sqlx-socket`: chip filters, tag input, raw-table view,
  and polling refresh

## What you're seeing

Three actors, two of them sandboxed:

```
┌──────────┐   GET /todos     ┌──────────┐   TCP 127.0.0.1:7777   ┌──────────────┐   PostgreSQL
│  browser │ ───────────────► │ http-api │ ─────────────────────► │  service-pg  │ ◄──────────►
│   (you)  │ ◄─────────────── │(component)│ ◄───────────────────── │  (service)   │
└──────────┘    JSON line     └──────────┘     JSON line          └──────────────┘
                              stateless                           long-running
                              per-request                         holds PgPool
                              scales out                          owns :7777
                                                                       │
                                                                       │ tunnel via wash:
                                                                       │ sandbox 127.0.0.1:5432
                                                                       │      ↓
                                                                       │ host    127.0.0.1:5433
                                                                       ▼
                                                                   Postgres
```

Both wasm workloads run inside a sandbox. The TCP between `http-api` and
`service-pg` (port 7777) is wash's **in-process loopback**. The TCP from
`service-pg` to Postgres is a real OS connection, gated by the tunnel rule.

## The socket-tunnel policy

The service component dials normal Postgres:

```text
postgres://postgres:Password123!@127.0.0.1:5432/todos?sslmode=require
```

But Docker exposes Postgres on host port `5433`. The rewrite lives in
`.wash/config.yaml`:

```yaml
dev:
  socket_tunnels:
    rules:
      - sandbox_port: 5432
        host_addr: "127.0.0.1:5433"
```

Read: "if a component dials `127.0.0.1:5432`, route that connection to
`127.0.0.1:5433` on the real OS network." The component never knows about the
rewrite. If todos persist in Docker, the tunnel did its job.

Without a matching rule, `service-pg`'s first Postgres connect returns
`ConnectionRefused`; the sandbox does not fall through to arbitrary host
loopback ports.

## Schema

Three tables, created by `service-pg` at startup:

```
wasi_todos                      wasi_todo_tags              wasi_tags
┌────────────────────┐          ┌──────────────┐            ┌──────────────────┐
│ id       BIGSERIAL │ ◄────┐   │ todo_id  FK  │   ┌─────►  │ id     BIGSERIAL │
│ description TEXT   │      └───│ tag_id   FK  │───┘        │ name VARCHAR(64) │
│ done        BOOL   │          │ PK(todo,tag) │            │ UNIQUE(name)     │
└────────────────────┘          └──────────────┘            └──────────────────┘
```

## Wire protocol

Line-delimited JSON over TCP. Same protocol as `sqlx-socket`.

| Command                                                            | Reply                                                              |
|--------------------------------------------------------------------|--------------------------------------------------------------------|
| `{"op":"list"}`                                                    | `{"ok":true,"todos":[{"id":1,"description":"…","done":false,"tags":["work"]}, …]}` |
| `{"op":"list","tag":"work"}`                                       | same shape, filtered to todos that carry the tag                   |
| `{"op":"create","description":"…","tags":["work","urgent"]}`       | `{"ok":true,"id":42}`                                              |
| `{"op":"done","id":42}`                                            | `{"ok":true}`                                                      |
| `{"op":"delete","id":42}`                                          | `{"ok":true}`                                                      |
| `{"op":"tags"}`                                                    | `{"ok":true,"tags":[{"name":"work","count":3}, …]}`                |

## SQLx features in play

- **`PgPoolOptions` + `PgPool`** with a single pool shared by the service.
- **`#[derive(FromRow)]`** on `TodoRow` and `TagCount`.
- **Postgres bind syntax** (`$1`, `$2`) rather than MySQL's `?`.
- **`INSERT ... RETURNING id`** for todo creation.
- **`ON CONFLICT DO NOTHING`** for idempotent tag/link insertion.
- **`string_agg(tg.name, ',' ORDER BY tg.name)`** for tag aggregation.

## WASI TLS provider

The bundled Docker Postgres runs with TLS enabled and presents a certificate
signed by `certs/ca.crt`. The SQLx URL uses `sslmode=require`, so the
component exercises `wasi:tls/client` on the Postgres connection path.

The trust decision lives in the host, not in the component. This example opts
into a temporary dev provider with:

```yaml
dev:
  wasi_tls_ca_path: certs/ca.crt
```

`wash dev` loads that CA and installs a rustls-backed provider with
`EngineBuilder::with_tls_provider(...)`. That provider is what lets the WASI
TLS handshake trust the demo Postgres certificate.

The general path for managed/private Postgres TLS is the same:

- `wash_runtime::engine::EngineBuilder::with_tls_provider(...)`
- `crates/wash-runtime/tests/common/tls.rs` contains a `TestTlsProvider`
  example that builds a custom `rustls::ClientConfig` and injects trusted root
  certificates into the WASI TLS provider.
- `crates/wash-runtime/tests/integration_tls_socket.rs` shows that provider
  wired into a P3 runtime and used by a component.

## Prerequisites

- Rust nightly with the `wasm32-wasip2` target:
  ```bash
  rustup target add wasm32-wasip2
  ```
- `wash` built from this branch with WASIP3/socket tunnel support.
- Docker for the bundled Postgres:
  ```bash
  docker compose up -d
  # tear down + wipe the volume:
  # docker compose down -v
  ```

## Quick start

```bash
docker compose up -d
wash dev
open http://localhost:8000/
```

Or hit the API directly:

```bash
curl -X POST http://localhost:8000/todos \
  -H "Content-Type: application/json" \
  -d '{"description":"buy milk","tags":["errand","weekend"]}'

curl http://localhost:8000/todos
curl "http://localhost:8000/todos?tag=errand"
curl http://localhost:8000/tags
```

## Project structure

```
sqlx-socket-pg/
├── .wash/config.yaml          # build + dev config, including socket_tunnels
├── certs/                     # dev CA + Postgres TLS certificate
├── docker-compose.yml         # TLS-enabled Postgres 17 with persistent volume
├── Cargo.toml                 # workspace root; pins sqlx to the wasip3 branch
├── wit/world.wit
│
├── service-pg/                # long-lived DB service
│   ├── src/lib.rs             # Arc<PgPool>, JSON-over-TCP loop, transactions
│   └── Cargo.toml
│
└── http-api/                  # stateless HTTP front-end
    ├── src/lib.rs             # /todos + /tags REST → JSON over TCP
    ├── ui.html                # UI served at /
    └── Cargo.toml
```

## Customizing

### Reach a different host

The tunnel rule is not a 1:1 mapping. To point the same sandbox
`127.0.0.1:5432` dial at a managed database:

```yaml
dev:
  socket_tunnels:
    rules:
      - sandbox_port: 5432
        host_addr: "db.internal:25060"
```

Hostnames are resolved once at workload start.

### Change pool size

`PgPoolOptions::new().max_connections(N)` in `service-pg/src/lib.rs`.

### Change the inter-component TCP port

`service-pg/src/lib.rs` (bind) and `http-api/src/lib.rs` (connect) must stay
in sync. No tunnel rule is needed for port 7777 because it stays inside wash's
in-process loopback.
