# The betty-blocks retrieval plugin: running a host with it

`betty-blocks-retrieval` is a native host plugin
(`crates/wash-runtime/src/plugin/betty_retrieval`). It gives a component
that imports `betty-blocks:retrieval/store@0.1.0` the two things a wasm
component cannot hold itself: a Postgres connection pool, and an embedding
model that turns `embed` bind parameters into pgvector vectors before a
statement reaches the database.

It is compiled in only with the `betty-retrieval` cargo feature, which is
off by default. The default `wash` binary and the default `wash-host` image do
not contain it.

```sh
cargo build -p wash --release --features betty-retrieval
wash host --help | grep retrieval-     # the flags are there only with the feature
```

This page is for whoever builds and operates such a host. What the component
does with the plugin is documented in the context-provider repository.

## Host settings

The plugin is registered when a database url and a model are both set. With
neither, a host built with the feature behaves as one built without it. With
only one of the two, the host refuses to start and says which is missing.

`wash host` takes flags or environment variables. `wash dev` takes the same
names as keys under `dev:` in `.wash/config.yaml` (for example
`dev.retrieval_database_url`); the four model settings can also come from the
`WASH_RETRIEVAL_MODEL*` variables, which win over the file. `wash dev` reads
those variables only for a project whose file sets the database or names a
model, so variables exported for `wash host` leave other projects alone. A
wash built without the feature refuses a config that sets any
`dev.retrieval_*` key, naming each.

| Flag | Environment variable | Default | |
|---|---|---|---|
| `--retrieval-database-url` | `WASH_RETRIEVAL_DATABASE_URL` | none | `postgres://` or `postgresql://` url of the plugin's own pool. Carries the password: see below. |
| `--retrieval-model` | `WASH_RETRIEVAL_MODEL` | none | A model name, or the path to a model descriptor. |
| `--retrieval-model-config` | `WASH_RETRIEVAL_MODEL_CONFIG` | none | The older, path-only way to say the same. Used when `--retrieval-model` is unset. |
| `--retrieval-model-catalog` | `WASH_RETRIEVAL_MODEL_CATALOG` | none | Where names are looked up: a directory, or an `http(s)` base url, holding `<name>.json`. |
| `--retrieval-model-cache` | `WASH_RETRIEVAL_MODEL_CACHE` | `$XDG_CACHE_HOME/genius-embed/models`, else `$HOME/.cache/genius-embed/models` | Where a named model's files are kept. |
| `--retrieval-model-mirror` | `WASH_RETRIEVAL_MODEL_MIRROR` | none | Fetch a named model's files from this base url instead of the one its descriptor gives. |
| `--retrieval-ca-file` | `WASH_RETRIEVAL_CA_FILE` | none | A PEM file of CA certificates to trust for the database's TLS, beside the public roots. In `wash dev`, a relative path is read from the project directory. |
| `--retrieval-pool-size` | `WASH_RETRIEVAL_POOL_SIZE` | 8 | Connections in the pool. A transaction a component holds pins one. |
| `--retrieval-connections-per-workload` | `WASH_RETRIEVAL_CONNECTIONS_PER_WORKLOAD` | half the pool, at least 1 | How many connections one workload may hold at a time: one statement, a query's rows or a whole transaction each hold one. A workload over its share waits up to the connect timeout and then gets `pool-exhausted`, while the other workloads go on. At most the pool size. |
| `--retrieval-connect-timeout-secs` | `WASH_RETRIEVAL_CONNECT_TIMEOUT_SECS` | 10 | How long a checkout waits for a free connection, after which the component gets `pool-exhausted`, how long a new connection may take to open or a pooled one to be reset, and how long the server may take to end a statement the component dropped. |
| `--retrieval-ef-search` | `WASH_RETRIEVAL_EF_SEARCH` | 200 | `hnsw.ef_search`, set on every pooled connection. |
| `--retrieval-max-scan-tuples` | `WASH_RETRIEVAL_MAX_SCAN_TUPLES` | 20000 | `hnsw.max_scan_tuples`, likewise. |
| `--retrieval-embed-threads` | `WASH_RETRIEVAL_EMBED_THREADS` | 4 | Threads one embedding uses. |
| `--retrieval-embed-max-texts` | `WASH_RETRIEVAL_EMBED_MAX_TEXTS` | 1000 | The most texts one statement may embed, over all of its `embed-text` and `embed-texts` parameters. More is `invalid-params`, before anything is embedded. |
| `--retrieval-embed-max-text-bytes` | `WASH_RETRIEVAL_EMBED_MAX_TEXT_BYTES` | 65536 (64 KiB) | The longest text a statement may embed. A longer one is `invalid-params`, named by its SHA-256. The model reads only the first `max_sequence` tokens of a text anyway. |
| `--retrieval-statement-timeout-secs` | `WASH_RETRIEVAL_STATEMENT_TIMEOUT_SECS` | 30 | `statement_timeout` on every pooled connection: the server cancels a statement that runs longer, and the component gets SQLSTATE 57014. 0 turns it off. |
| `--retrieval-transaction-query-max-bytes` | `WASH_RETRIEVAL_TRANSACTION_QUERY_MAX_BYTES` | 67108864 (64 MiB) | How much one `transaction.query` may read, counted as an upper bound of what the host holds once the rows are decoded, not their bytes on the wire: 96 bytes for each row and 96 for each value, so rows of NULLs count too, plus each value's decoded size. A `text[]` member costs 24 bytes (a `String`) and a `varchar[]` member 32, on top of their characters, where the wire has 4 bytes for an empty one (`SELECT array_fill(''::text, ARRAY[15000000])` is 60 MB on the wire and 360 MB decoded, and fails); the member count is read from the array's header. A `numeric` costs the characters it prints (`1e131071` is 10 wire bytes and 131,072 characters). A `json` or `jsonb` costs 10 times its wire bytes, a fixed factor for the tree the host builds to decode it, not a bound: a document of nothing but `[1],[1],...` builds more. Text, numbers, bytes and dates cost what they did. A transaction's rows are read before the call returns, so a larger result, or one of more than 1,000,000 rows, fails that query with `other`, and the transaction can no longer commit. |
| `--retrieval-idle-in-transaction-timeout-secs` | `WASH_RETRIEVAL_IDLE_IN_TRANSACTION_TIMEOUT_SECS` | 60 | `idle_in_transaction_session_timeout` on every pooled connection: the server ends a session that waits inside a transaction longer, which releases the transaction's locks. 0 turns it off. |

The plugin is configured only by the host. A workload that puts config on its
`betty-blocks:retrieval` interface entry is refused.

### The database url is a secret

Give it through the environment variable from a Kubernetes Secret
(`runtime.hostGroups[].envFrom` or an `env` entry with `valueFrom`), not as a
flag in `extraArgs`, where it would sit in the pod spec. `wash host --help`
does not print the variable's value, and the structs that hold the url print
`<redacted>` under `Debug`.

### What the database must be

- Postgres with pgvector 0.8 or newer. Every pooled connection runs
  `SET hnsw.iterative_scan = 'strict_order'`, which older pgvector rejects,
  and then no connection can be checked out.
- The first connection the plugin opens runs
  `CREATE EXTENSION IF NOT EXISTS vector`, before anything else. It does so
  in a transaction under an advisory lock, so hosts that start together on a
  new database wait for one another and do not fail on a duplicate extension.
  The wait for that lock is bounded by the connect timeout; a statement that
  ran out of it answers `connection-failed`, and the next one tries again.
  Install the extension beforehand if the plugin's role may not: until it
  exists, no connection can be checked out.
- Every pooled connection is set to `search_path = public`.
- One pool serves every workload on the host, so nothing one statement leaves
  on its session reaches the next borrower. Before a connection is lent
  again, the plugin rolls back a transaction that a statement left open or
  failed, cleans the session (settings, cursors, temporary tables, advisory
  locks, listeners) and applies its own settings again. That costs one round
  trip for each checkout. A connection whose statement was cancelled, or whose
  rows were not read to the end, is closed instead of pooled, once the server
  has cancelled the statement (see below). And no workload
  may hold more than its share of the pool (half of it by default), so one
  application that holds or leaks connections cannot stop the others.

## How the model's weights reach the host

The model is not in the binary or the image. For
`granite-embedding-107m-multilingual` it is two files, 417 MB on disk
together: `model.onnx` (428,102,968 bytes) and `tokenizer.json` (9,081,382
bytes). A descriptor, a small JSON file, names them and pins the SHA-256 of
each. There are two ways to say which model to run.

**By path.** `--retrieval-model` (or `--retrieval-model-config`) is the path
to a descriptor already on the host. Its `model_path` and `tokenizer_path`
are relative to the descriptor's own directory, so the files sit beside it:

```
/models/granite-embedding-107m-multilingual.json
/models/granite-embedding-107m-multilingual/model.onnx
/models/granite-embedding-107m-multilingual/tokenizer.json
```

Nothing is fetched and nothing is written, so this works from a read-only
mount. It needs no catalog, cache or mirror. The host hashes both files
against the pins on every start.

**By name.** `--retrieval-model` is a name, and `--retrieval-model-catalog`
says where `<name>.json` is: a directory, or an `http(s)` base url. The
plugin copies that descriptor into `<cache>/<name>/`, then makes sure both
files are there with the pinned digests, downloading each from the
descriptor's `source.base_url` (or from `--retrieval-model-mirror`) when it
is missing or does not match. A named descriptor must pin both digests.
Each file has 10 minutes in all to arrive, whatever its speed, so a download
slower than about 0.7 MB/s (430 MB in 600 seconds) fails and the next start
tries again. That total deadline is known and not changed.

By name needs a writable cache directory even when every file is already
there, because the descriptor is written into it on each start. A download
holds the whole file in memory before writing it, so the first start by name
needs about 430 MB more memory than a later one.

Either way the plugin logs one line, `retrieval_model_resolved`, with the
model id, the embedding space id and whether the files came from a `path`,
the `cache` or a `download`. The space id is derived from the files' bytes;
a component refuses a corpus written in another space.

A value that contains a `/` or ends in `.json` is read as a path. If no file
is there the host stops with an error rather than looking it up as a name.

## Building an image with the plugin

`./Dockerfile` builds the default image and is not changed by any of this.
`./Dockerfile.retrieval` builds the same host with the plugin:

```sh
docker build -f Dockerfile.retrieval -t wash-host:retrieval .
```

The publish workflow (`.github/workflows/bettyblocks-publish-images.yml`) has
an `image` choice for it, `wash-host-retrieval`. It publishes under the
`wash-host` name with `-retrieval` appended to the tag, for example
`ghcr.io/bettyblocks/wash-host:2.10.3-abc123def-retrieval`. It is built only
when chosen by name: `all` still builds the four default images and nothing
else.

What the variant adds, and why:

| Where | What | Why |
|---|---|---|
| target | glibc (`*-unknown-linux-gnu`), as `./Dockerfile` already is | `ort`, the ONNX Runtime binding, publishes a prebuilt runtime for x86_64 and aarch64 glibc and none for musl. |
| builder | `libstdc++-dev` | `ort-sys` links ONNX Runtime statically and asks the linker for `-lstdc++`. Nothing in the build compiles C++. |
| builder | `openssl-dev`, `pkgconf` | `ort-sys`'s build script downloads ONNX Runtime over native-tls, which is OpenSSL on Linux. Build time only. |
| builder | network access to `cdn.pyke.io` | That download: ONNX Runtime 1.22, about 21 MB, checked against a SHA-256 pinned in the `ort-sys` crate. |
| runtime | `libstdc++`, `libgcc` | ONNX Runtime is linked into `wash` statically; its C++ runtime is not. |

Going by what the build links, not by inspecting a built binary, the shared
libraries `wash` then needs at run time are `libstdc++.so.6`,
`libgcc_s.so.1`, `libm.so.6` and `libc.so.6`: what the default binary needs,
plus the C++ runtime. It links no OpenSSL and no `libonnxruntime.so`. The
x86_64 prebuilt ONNX Runtime is one static archive compiled with GCC 11.4 on
Ubuntu 22.04, and beyond libc, libm and the compiler runtime it refers only
to the C++ standard library. It needs a glibc and a libstdc++ at least that
new; Wolfi's are newer.

**This image has been built once, by hand, for linux/arm64.** That build
completed and passed both smoke tests. It was made before `tokenizers` lost
its `esaxx_fast` feature, and the image has not been built again since, nor
for amd64, nor by the publish workflow. If a build fails, these are the
likely places:

1. A Wolfi package name. `libstdc++-dev`, `openssl-dev`, `pkgconf`,
   `libstdc++` and `libgcc` are the names the arm64 build found; `apk` fails
   fast with `unable to select packages` if one is wrong for another
   architecture.
2. `cargo chef cook --features`. Unlike `./Dockerfile`, the dependency layer
   is cooked with the feature on, which is where `ort-sys` downloads and
   where `openssl-sys` looks for OpenSSL. If cooking fails and the plain
   build does not, drop `--features` from the `cook` line: the image is the
   same, only the caching is worse.
3. The download from `cdn.pyke.io` on a runner that cannot reach it. `ort`
   can link a runtime you provide instead (`ORT_LIB_LOCATION`; see its
   documentation).
4. Memory. The build compiles wasmtime and links ONNX Runtime;
   `CARGO_BUILD_JOBS` (a build argument, 4 unless set) caps how many crates
   compile at once. The arm64 build ran in a builder with 8 GiB.

The image's own smoke tests catch a missing shared library (`wash --version`
runs in the release stage) and a binary built without the feature
(`wash host --help` must list `--retrieval-database-url`).

`Dockerfile.retrieval` pins the same base image digest as `./Dockerfile`.
Bump them together.

## What the Helm chart's defaults break

The chart (`charts/runtime-operator`) runs a host as a non-root user with a
read-only root filesystem, `HOME=/tmp` on an `emptyDir`, and a 512Mi memory
limit. A host with this plugin needs three of those looked at.

**Memory: 512Mi is not enough.** The model alone is 428 MB of weights held
in memory. Measured on macOS under `wash dev`, one host with the granite
model and one component held about 815 MiB after start and 876 to 938 MiB
with a corpus loaded, and peaked at about 1.3 GiB while starting. It has not
been measured on Linux or in a container. Start a host with this plugin at
2Gi and measure. The model is native memory, not guest memory. Left
unset, the host's guest-memory budget is three quarters of the container
limit, which leaves the host a quarter: less than the model needs at 2Gi. Set
`--max-guest-memory` with the model in mind before switching
`--guest-memory-mode` to `enforce`.

**The read-only root filesystem and the model.** By path, mount the
descriptor and its two files read-only (`runtime.hostGroups[].volumes` and
`volumeMounts`) and point `WASH_RETRIEVAL_MODEL` at the descriptor. By name,
the cache has to be writable.

**Where the cache directory points.** With the chart's `HOME=/tmp`, the
default cache is `/tmp/.cache/genius-embed/models`, on the `tmp` `emptyDir`.
That is writable, so by name works out of the box, but an `emptyDir` is
empty in every new pod: each one downloads 417 MB before the host is ready,
and holds them in the node's ephemeral storage. The startup probe has to
allow for the download and for loading the model. To avoid both, mount a
volume that outlives the pod and set `WASH_RETRIEVAL_MODEL_CACHE` to it, or
use a path.

CPU matters too: an embedding runs on `--retrieval-embed-threads` threads (4
by default), and the chart requests a quarter of a core.

## Things an operator should know

**The host starts without the database.** The plugin opens no connection
when the host starts; its pool connects at the first statement. While the
database is down, or refuses the plugin's role, every statement answers
`connection-failed`. Each step of a checkout is held to the connect timeout
(`--retrieval-connect-timeout-secs`, 10 seconds unless set): the wait for the
workload's share and a free connection, opening a new connection, setting
up its session, which a server that stops answering after the handshake
would otherwise hold for ever, and resetting a pooled connection before it is
lent again: one that does not answer the reset in time is dropped, and the
next one is tried or a new one opened. TCP keepalives (after 30 seconds of
quiet, then every 10 seconds, 3 probes) and, on Linux, a 60-second TCP user
timeout close a connection the network dropped without a word within about a
minute, where TCP alone takes 15 minutes or more; the url's
`keepalives_idle`, `keepalives_interval`, `keepalives_retries` and
`tcp_user_timeout` win over these. `space` still answers, because it comes
from the model. A later statement works once the
database is back, with no host restart.

So a wrong database url or password no longer stops the host: it shows as
`connection-failed` on every statement. The plugin logs
`retrieval_database_reached` once, at its first connection; a host that never
logs it has never reached its database.

**The host starts without the model too.** If the model cannot be resolved
or loaded, the plugin logs `retrieval_model_failed` with the cause and comes
up without it. Statements that embed nothing still run. A statement with an
`embed-text` or `embed-texts` parameter fails with `embed`, saying that the
model did not load and why, and a call to `space` traps with the same cause,
since `space` returns a record and cannot answer an error. The plugin does
not try again: restart the host once the model's files are in place. Only a
setting that does not validate (one of the two missing, a bad url or CA file)
still stops the host.

**Statements and idle transactions have time limits.** By default the server
cancels a statement after 30 seconds, and ends a session that waits inside a
transaction for 60 seconds, so one application's slow query or forgotten
transaction cannot hold a connection, or the rows it locked, for ever. The
same statement limit applies to a query's rows outside a transaction: if a
component reads none of them for that long (it awaits the result before it
reads the rows, say), the plugin ends the query, closes its connection and
answers the result with `other`, naming the cause. Raise
or turn off either limit with the two settings above.

**A statement a component stops waiting for is cancelled.** When a component
drops a call, or a query's rows before the last, or a transaction in the
middle of a statement, the plugin asks Postgres to cancel the statement and
then closes the connection. Until the server has ended the statement, for the
connect timeout at most, the connection keeps its place in the pool and in
the workload's share: closing it alone would leave Postgres running the
statement on a session nothing counts, and a loop of dropped calls could fill
the server's `max_connections`. If the server does not confirm in time, the
plugin logs `retrieval_cancel_unconfirmed` and closes the connection anyway.
Known: a component that drops a query's stream right after reading its last
row can beat the plugin's own sight of the end of the rows, and the plugin
then cancels a statement that had already finished, a needless cancel and a
connection closed and replaced. It is harmless to the data, and not changed.

**A transaction held open on purpose.** A consumer that keeps a transaction
open longer than the idle-in-transaction limit on purpose, such as a lock held
for a whole sync while it fetches, should raise the limit for that
transaction alone: `SET LOCAL idle_in_transaction_session_timeout = '<time>'`
as its first statement. Otherwise the server ends the session after 60
seconds, and with it the lock. The reset before the connection is lent again
puts the host's limit back. The Genius context provider does this itself on
its lock transactions: it sets 1200 seconds (20 minutes), from its
`RETRIEVAL_LOCK_IDLE_TIMEOUT_SECS`, so the host's 60 seconds does not end a
sync's lock.

**Size the pool for the consumer's peak.** A workload holds one connection
for each statement it is running, each query whose rows it has not read to the
end and each open transaction, up to its share
(`--retrieval-connections-per-workload`, half the pool unless set); one more
waits up to the connect timeout and fails with `pool-exhausted`. Set the pool
size and the share to the most connections the consumer holds at once. The
Genius context provider holds at most 2 for each sync it runs: its sync lock,
and its statements on the same connection (its ingest lock is a savepoint on
the sync lock's connection, not a connection of its own). The peak is
2 x `RETRIEVAL_SYNC_CONCURRENCY` (default 1) x the component's warm instance
count (`poolSize`, 4 on edge), plus 2 for each sync running in a one-call
store that overflows the pool. One workload runs the syncs of every
application, so with the defaults (a pool of 8, a share of 4) a second sync at
the same time can already wait. For the Genius host group set
`WASH_RETRIEVAL_POOL_SIZE=16` and
`WASH_RETRIEVAL_CONNECTIONS_PER_WORKLOAD=12`.

**TLS.** With `sslmode=require`, `verify-ca` or `verify-full` in the database
url, the plugin connects with TLS and verifies the server's certificate chain
and its host name. All three mean the same, so `require` and `verify-ca` are
stricter here than in libpq. The trust roots are the public web roots
compiled into the binary and, for a database whose certificate comes from a
private CA, the certificates in `--retrieval-ca-file` (or the file a url's
`sslrootcert` names; the setting wins). With `disable`, `prefer` or no
`sslmode`, the connection is not encrypted at all. Any other `sslmode`, a CA
file that cannot be read, or a url option the postgres client does not know
stops the host at start, naming the option or the file but never the url. A
named model's catalog and downloads over `https` trust the public roots
only.

**No egress ceiling.** The plugin connects to Postgres and, by name, to the
catalog and the model's source, but does not enforce `allowedHosts`,
`allowedIpNameLookups` or `allowedHostLoopbackPorts`. A `host.plugins` entry
for `betty-blocks-retrieval` that sets one is refused at startup, with a
message that says the plugin "does not connect out". It does; it just does
not check. Leave those fields off the entry.

**Stopping.** On shutdown the plugin closes its pool. A transaction a
component still holds keeps its connection until the component commits or
drops it.

**Embedding is shared.** Every workload embeds with the one model, one text
at a time. So each workload embeds one statement at a time: a workload's
next statement waits for its last embedding to end, while other workloads go
on. A statement whose call is cancelled stops embedding before its next
text, and the two caps above keep one statement from holding the model for
long.

**How many rows the host holds.** Outside a transaction, a query's rows are
read from the server as the component reads them: at most 16 wait in
between, costing at most 64 MiB together, counted as for
`--retrieval-transaction-query-max-bytes` above. A row's cost is checked
before it is decoded, and the row as it came off the wire is dropped once
it is. A single row that costs
more than 64 MiB fails its query with `other` before the component gets it,
and the next statement runs as usual. The host has the whole row in memory
by then (Postgres sends each row whole, and a value can be up to 1 GB). The
bound is for each running statement, so a workload can hold up to its share of
such rows at once, one for each connection it may use. Inside a transaction, the query's rows are all
read before the call returns, so they are held to the cap above and to
1,000,000 rows.

**What a component can and cannot read.** Rows come back as
`wasmcloud:postgres` `pg-value`s. A column with no `pg-value`, such as
`vector`, `line`, `circle`, `interval` or a range, fails that query with
`value-conversion-failed` naming the column; cast it in SQL (`::text`,
`::float4[]`). Lists bind only to array parameters. A number, bool, date or
time binds only to a parameter of its own type (`timestamp` and
`timestamptz` to either), and text does not bind to a fixed-width type such
as `int4`, `float8` or `uuid`: each would be stored as a different value.
Such a parameter is `invalid-params`. Text still binds to the types Postgres
stores as text, such as `json`, `xml` and enums.

## A component has to be built for this host's wasmtime

This host runs wasmtime 48. From 48 on, wasmtime traps a guest that cancels
a stream or future read or write, or a subtask, synchronously while that
waitable is still in a waitable set. Guests built with an older `wit-bindgen`
do exactly that: 0.54.0 and 0.57.1 cancel the read of their inter-task
wakeup stream before taking it out of its set, and 0.60.0 does it the other
way round.

So a component built with `wit-bindgen` 0.54 loads, links and starts on
this host, and then every request to it fails with

```
wasm trap: waitable cannot be used synchronously while added to a waitable set
```

Run as a service, the host logs that trap only as `trigger service faulted;
max restarts reached err=trigger service driver exited`, then answers every
request `service HTTP instance is not running`. Run per request, the trap
itself is in the log.

This was seen with the context provider component: its binary built with
`wit-bindgen` 0.54.0 fails this way on this host and runs on a wasmtime 47
host. Rebuilt with `wit-bindgen` 0.60.0, which needed one rename
(`wit_bindgen::spawn` to `spawn_local`), it runs on both, and on this host
answered `/health`, `/ready`, three searches and a `peek` over the same
ingested sample byte for byte as the 0.54.0 build does on the wasmtime 47
host. Build components for this host with `wit-bindgen` 0.60 or newer.

## The embedder is vendored

`crates/genius-embed` is a copy of the embedder from the context-provider
repository. Its `README.md` records the commit it was copied at. The
component and this plugin must compute the same embedding space id, so that
copy changes only by copying a newer one in.

## Tests

```sh
# unit tests, no database
cargo test -p wash-runtime --features betty-retrieval --lib betty_retrieval
cargo test -p wash --features betty-retrieval --lib retrieval

# integration tests: a real Postgres with pgvector and a wasm guest
cargo xtask build-fixtures
BETTY_RETRIEVAL_TEST_DATABASE_URL=postgres://USER:PASSWORD@HOST:PORT/DATABASE \
  cargo test -p wash-runtime --features betty-retrieval \
  --test integration_betty_retrieval -- --ignored
```

Without `BETTY_RETRIEVAL_TEST_DATABASE_URL` the integration tests start a
`pgvector/pgvector:pg17` container. One of them,
`space_answers_while_the_database_is_down`, needs no database and runs
without `--ignored`.

One unit test loads the real model and checks its embedding space id. It is
ignored by default, and skips itself unless `BETTY_RETRIEVAL_TEST_MODEL_CONFIG`
is the path of a descriptor:

```sh
BETTY_RETRIEVAL_TEST_MODEL_CONFIG=/models/granite-embedding-107m-multilingual.json \
  cargo test -p wash-runtime --features betty-retrieval --lib betty_retrieval -- --ignored
```
