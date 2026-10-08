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
`WASH_RETRIEVAL_MODEL*` variables, which win over the file.

| Flag | Environment variable | Default | |
|---|---|---|---|
| `--retrieval-database-url` | `WASH_RETRIEVAL_DATABASE_URL` | none | `postgres://` or `postgresql://` url of the plugin's own pool. Carries the password: see below. |
| `--retrieval-model` | `WASH_RETRIEVAL_MODEL` | none | A model name, or the path to a model descriptor. |
| `--retrieval-model-config` | `WASH_RETRIEVAL_MODEL_CONFIG` | none | The older, path-only way to say the same. Used when `--retrieval-model` is unset. |
| `--retrieval-model-catalog` | `WASH_RETRIEVAL_MODEL_CATALOG` | none | Where names are looked up: a directory, or an `http(s)` base url, holding `<name>.json`. |
| `--retrieval-model-cache` | `WASH_RETRIEVAL_MODEL_CACHE` | `$XDG_CACHE_HOME/genius-embed/models`, else `$HOME/.cache/genius-embed/models` | Where a named model's files are kept. |
| `--retrieval-model-mirror` | `WASH_RETRIEVAL_MODEL_MIRROR` | none | Fetch a named model's files from this base url instead of the one its descriptor gives. |
| `--retrieval-pool-size` | `WASH_RETRIEVAL_POOL_SIZE` | 8 | Connections in the pool. A transaction a component holds pins one. |
| `--retrieval-connect-timeout-secs` | `WASH_RETRIEVAL_CONNECT_TIMEOUT_SECS` | 10 | How long a checkout waits for a free connection, after which the component gets `pool-exhausted`, and how long a new connection may take to open. |
| `--retrieval-ef-search` | `WASH_RETRIEVAL_EF_SEARCH` | 200 | `hnsw.ef_search`, set on every pooled connection. |
| `--retrieval-max-scan-tuples` | `WASH_RETRIEVAL_MAX_SCAN_TUPLES` | 20000 | `hnsw.max_scan_tuples`, likewise. |
| `--retrieval-embed-threads` | `WASH_RETRIEVAL_EMBED_THREADS` | 4 | Threads one embedding uses. |

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
`connection-failed`, after the connect timeout at the latest
(`--retrieval-connect-timeout-secs`, 10 seconds unless set). `space` still
answers, because it comes from the model. A later statement works once the
database is back, with no host restart.

So a wrong database url or password no longer stops the host: it shows as
`connection-failed` on every statement. The plugin logs
`retrieval_database_reached` once, at its first connection; a host that never
logs it has never reached its database.

**The host does not start without the model.** If the model cannot be
resolved or loaded, `wash host` exits. It does not come up without the
plugin. In Kubernetes that is a crash loop that takes every workload on the
host with it.

**There is no statement timeout.** The plugin sets none, on the session or
on the client. A slow statement holds its pooled connection until Postgres
finishes it, and a component that stops waiting does not cancel it on the
server. Set `statement_timeout` on the plugin's database role.

**TLS trusts public roots only.** With `sslmode=require`, `verify-ca` or
`verify-full` in the database url, the plugin connects with TLS and verifies
the server's certificate and name against the public web roots compiled into
the binary. All three mean the same. A database whose certificate comes from
a private CA is refused, and there is no setting to add a root. With any
other `sslmode`, or none, the connection is not encrypted at all. A named
model's catalog and downloads over `https` trust the same public roots.

**No egress ceiling.** The plugin connects to Postgres and, by name, to the
catalog and the model's source, but does not enforce `allowedHosts`,
`allowedIpNameLookups` or `allowedHostLoopbackPorts`. A `host.plugins` entry
for `betty-blocks-retrieval` that sets one is refused at startup, with a
message that says the plugin "does not connect out". It does; it just does
not check. Leave those fields off the entry.

**Stopping.** On shutdown the plugin closes its pool. A transaction a
component still holds keeps its connection until the component commits or
drops it.

**What a component can and cannot read.** Rows come back as
`wasmcloud:postgres` `pg-value`s. A column with no `pg-value`, such as
`vector`, `line`, `circle`, `interval` or a range, fails that query with
`value-conversion-failed` naming the column; cast it in SQL (`::text`,
`::float4[]`). Lists bind only to array parameters.

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
