# genius-embed (vendored)

The embedder behind the `betty-retrieval` host plugin
(`crates/wash-runtime/src/plugin/betty_retrieval`): model descriptors, the
ONNX Runtime backed `Adapter`, the embedding space identity, and the resolver
that turns a model name into verified files on disk.

## Where this came from

| | |
|---|---|
| Source repository | the context-provider repository (not public) |
| Directory there | `genius-embed/` |
| Copied at commit | `211053653e13175c7d6aee9e75d6d8c8c16e0b0a` (branch `master`, 2026-09-23) |
| Crate last changed in | `1b4ce0fe7cae66319bf00b0ad7474b417d9317a0` (2026-09-21) |
| Git tree of `genius-embed/` | `574d2967a234a397f72ff2ae266090a047e5a436` |

It is a copy, not a submodule or a registry dependency: the source repository
is private and publishes no crate, and a path dependency reaching outside this
repository breaks the whole workspace on any machine without that checkout.

## The two copies must be kept in step

The context provider component and this host plugin must embed a text into the
same vector. The component tags every corpus with the embedding space
identity this crate computes, and refuses a corpus written in another one. A
change to the tokenizer settings, the pooling, the normalisation, the
`ort`/`tokenizers` pins or the space identity that lands in one copy and not
the other shows up as refused corpora at best and as quietly worse search at
worst.

So:

- Change what decides a vector (the tokenizer settings, the pooling, the
  normalisation, the `ort`/`tokenizers` pins, the space identity) in the
  source repository first, then copy it here.
- When copying, update the commit ids in the table above in the same commit,
  and keep the differences listed below.
- `src/` is the source byte for byte, except the changes listed below, none of
  which changes a vector or the space identity. To check, with the source
  repository checked out at the commit above:

  ```sh
  diff -r crates/genius-embed/src <context-provider>/genius-embed/src
  ```

## What differs from the source, on purpose

Only files the source does not have, a few lines of `Cargo.toml`, one
comment, and how a model reaches the disk:

- `Cargo.toml`: `default = []` where the source has `default = ["adapter"]`.
  As a workspace member, a default-on `adapter` would make every
  `cargo test --workspace` and `cargo clippy --workspace` download ONNX
  Runtime and build `tokenizers`. `wash-runtime` asks for `adapter` and
  `fetch` by name. Every consumer in the source repository already sets
  `default-features = false`, so the same change can be made there.
- `Cargo.toml`: `publish = false`.
- `Cargo.toml`: `tokenizers` without its default features, with `onig` only.
  The default `esaxx_fast` compiles C++ against the static C runtime, and the
  Windows linker refuses that beside `ort-sys`. It and `progressbar` serve
  only training a tokenizer; the vectors are the same bytes either way. The
  same change can be made in the source.
- `src/space.rs`: the doc comment on `artifact_digests` says "another
  repository" where the source names one that is not public. No code differs.
- `src/modelcfg.rs`: `ModelConfig::parse` lowers the case of the two pinned
  digests. An upper-case pin, as PowerShell's `Get-FileHash` prints it, refused
  the very bytes it named, and a cached model was deleted and fetched again at
  every start.
- `src/resolve.rs`, which decides only where the bytes come from and whether
  they are accepted, never what they embed to:
  - A catalog, mirror or source on another machine must use `https`; plain
    `http` is refused before any request, and an `https` address is not
    followed to `http`. The catalog's digests are what every download is
    checked against, so over plain http anything on the path could supply
    both. Loopback `http` is still allowed.
  - When the catalog cannot be reached, the descriptor a previous fetch left in
    the cache is used, if both artifacts in the cache still match its pins
    (`Origin::OfflineCache`). Before, a catalog outage stopped every restart.
  - A download is written to a file of its own (process id and time in the
    name), checked against its pin, and only then renamed into place; the
    descriptor is replaced the same way. Two hosts sharing one cache no longer
    write into each other's files, and a mismatch no longer deletes a file
    another host just put in place.
  - Every request has a connect (10 s), response (30 s) and whole-body
    (10 min) timeout, so a stalled catalog, mirror or source fails the
    resolve instead of hanging host startup.
  - An artifact is streamed into its temp file while its SHA-256 is computed,
    with a hard cap of 2 GiB (a catalog entry: 1 MiB); it is never held in
    memory. A stale cached artifact is replaced by the rename, not deleted
    first, and the descriptor is parsed from the bytes fetched (or read once
    from the cache), not re-read from the shared path. Temp files are removed
    on every error path, and `.partial` files over an hour old are swept.
  - Catalog, mirror and source addresses are parsed with the `url` crate, so
    the plain-http check looks at the real host and the scheme is
    case-insensitive. `redact_url` strips userinfo, query and fragment from
    every error, and `Request`'s `Debug` uses it.
  - A model name made only of dots is refused.
- `src/space.rs`: `space_identity` hashes the two artifacts by streaming
  (`sha256_file`) rather than reading them whole. The digests are the same.
- `src/modelcfg.rs`: `ModelConfig::parse_bytes`, which `parse` now calls.
- `Cargo.toml`: `fetch` also enables the optional `url` dependency.
- `README.md` (this file) and `rustfmt.toml`, which keeps the crate on
  rustfmt's defaults so that `cargo fmt` here does not reformat it away from
  the source.

The crate does not opt into this workspace's lints (`[lints] workspace = true`)
for the same reason: satisfying them would mean editing `src/`.

## Build requirements of the `adapter` feature

`ort` is pinned to `=2.0.0-rc.10` and `tokenizers` to `=0.20.4`; the vectors
the model research measured were produced with exactly these. With `adapter`
on, `ort-sys` downloads a prebuilt static ONNX Runtime 1.22 at build time and
links the C++ standard library. See
`crates/wash-runtime/docs/BETTY_RETRIEVAL_PLUGIN.md` for what that means for
an image build.
