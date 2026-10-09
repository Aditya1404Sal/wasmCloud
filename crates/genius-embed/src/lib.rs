//! The embedding function, as one crate.
//!
//! Both the native sidecar and the wasmCloud host plugin embed with this code
//! against the same model file, so the `space_id` they compute is identical by
//! construction rather than by two copies being kept in step. That identity is
//! the only thing standing between a corpus and vectors from another model
//! that are the same width, unit-norm, and meaningless.
//!
//! `adapter` (the real, `ort`-backed model) and `fake` (a deterministic
//! stand-in for tests) are each optional, so a wasm32 component can take
//! `embedder` and `modelcfg` alone: no onnxruntime, no tokenizer.

#[cfg(feature = "adapter")]
pub mod adapter;
pub mod embedder;
#[cfg(feature = "fake")]
pub mod fake;
pub mod modelcfg;
#[cfg(feature = "fetch")]
pub mod resolve;
pub mod space;

#[cfg(feature = "adapter")]
pub use adapter::{matryoshka_truncate, pool_mean, Adapter};
pub use embedder::{embed_distinct, l2_normalize, Embedder};
#[cfg(feature = "fake")]
pub use fake::FakeEmbedder;
pub use modelcfg::{sha256_file, ModelConfig, ModelSource, Pooling, Role};
#[cfg(feature = "fetch")]
pub use resolve::{resolve, Origin, Request as ResolveRequest, Resolved};
pub use space::{artifact_digests, space_identity, SpaceIdentity};
