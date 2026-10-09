//! Embedding as a capability something else borrows, not a model it owns.
//!
//! [`Adapter`](crate::adapter::Adapter) is the only real embedder and always
//! will be for serving. The trait exists so the native sidecar's incremental
//! path, and a wasmCloud host plugin's own tests, can run against a
//! deterministic double instead: the model weights are hundreds of MB,
//! git-ignored, and absent on CI, and "differential ≡ full rebuild, bit for
//! bit" is only checkable when the embedder is deterministic, which a real
//! model on a real CPU is not guaranteed to be across thread schedules.

use std::collections::HashMap;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::modelcfg::Role;

/// Anything that turns a text into a fixed-width vector.
pub trait Embedder: Send + Sync {
    fn embed(&self, text: &str, role: Role) -> Result<Vec<f32>>;
    fn dim(&self) -> usize;
}

#[cfg(feature = "adapter")]
use crate::adapter::Adapter;

#[cfg(feature = "adapter")]
impl Embedder for Adapter {
    fn embed(&self, text: &str, role: Role) -> Result<Vec<f32>> {
        Adapter::embed(self, text, role)
    }
    fn dim(&self) -> usize {
        Adapter::dim(self)
    }
}

/// Embed each distinct text once and expand back to input order.
///
/// Shared by the native store (which dedups a statement's passages before
/// binding them) and the wasmCloud host plugin: the corpus has entities with
/// identical documents, and every duplicate would otherwise be a forward pass
/// thrown away.
///
/// Sync by design — the crate that links `ort` already runs its forward pass
/// off any async reactor, and a wasmCloud host plugin has none to begin with.
/// Each caller wraps this in its own blocking context (`spawn_blocking` or
/// equivalent) rather than this function assuming one.
pub fn embed_distinct(
    embedder: &dyn Embedder,
    texts: &[String],
    role: Role,
) -> Result<Vec<Vec<f32>>> {
    let mut distinct: Vec<String> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut positions = Vec::with_capacity(texts.len());
    for t in texts {
        let next = distinct.len();
        let at = *index.entry(t.as_str()).or_insert(next);
        if at == next {
            distinct.push(t.clone());
        }
        positions.push(at);
    }

    let vectors: Vec<Vec<f32>> = distinct
        .into_iter()
        .map(|t| {
            // Named by the hash `entity.text_sha256` stores, so a failure is
            // one `WHERE` away from its entity: the model's own error does
            // not say which of thousands of texts it choked on.
            embedder
                .embed(&t, role)
                .with_context(|| format!("embed text with sha {:x}", Sha256::digest(t.as_bytes())))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(positions.into_iter().map(|i| vectors[i].clone()).collect())
}

pub fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for val in v.iter_mut() {
            *val /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A pure function of its own text and nothing else, so a transposed or
    /// reordered output is visible: on a palindrome input, "same text, same
    /// vector" and "different texts, different vectors" alone cannot see
    /// that.
    struct CountingEmbedder {
        calls: AtomicUsize,
    }

    impl CountingEmbedder {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl Embedder for CountingEmbedder {
        fn embed(&self, text: &str, _role: Role) -> Result<Vec<f32>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![text.bytes().map(f32::from).sum()])
        }

        fn dim(&self) -> usize {
            1
        }
    }

    /// THE contract of `embed_distinct`: a text repeated in the input is
    /// embedded once, not once per occurrence, and each output position
    /// carries exactly its own text's vector. Deliberately not a palindrome
    /// input — see `CountingEmbedder`.
    #[test]
    fn embeds_each_distinct_text_once_and_keeps_input_order() {
        let embedder = CountingEmbedder::new();
        let texts: Vec<String> = ["alpha", "beta", "alpha", "gamma"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let out = embed_distinct(&embedder, &texts, Role::Passage).expect("embeds");

        assert_eq!(
            embedder.calls.load(Ordering::SeqCst),
            3,
            "each distinct text must be embedded exactly once"
        );
        assert_eq!(out.len(), 4);

        // A fresh embedder — not `embedder` — so computing the expected
        // vectors here does not itself add to the call count under test.
        let oracle = CountingEmbedder::new();
        for (i, text) in texts.iter().enumerate() {
            let want = oracle.embed(text, Role::Passage).expect("embed");
            assert_eq!(
                out[i], want,
                "position {i} ({text:?}) must carry its own text's vector, not another position's"
            );
        }
    }
}
