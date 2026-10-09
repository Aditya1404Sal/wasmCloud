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

- Change this crate in the source repository first, then copy it here. Do not
  edit `src/` in this repository.
- When copying, update the commit ids in the table above in the same commit.
- `src/` is the source byte for byte, except the one comment listed below. To
  check, with the source repository checked out at the commit above:

  ```sh
  diff -r crates/genius-embed/src <context-provider>/genius-embed/src
  ```

## What differs from the source, on purpose

Only files the source does not have, three lines of `Cargo.toml`, and one
comment:

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
