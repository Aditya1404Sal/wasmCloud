//! A deterministic stand-in for the model: the vector is a pure function of
//! the text, so two embeds of equal text agree bit for bit — which is what
//! `context-provider`'s incremental suite compares. Counts its calls so a
//! test can assert how much embedding actually happened.
//!
//! Copies `fnv1a` (see the function doc) but imports `l2_normalize` from
//! `embedder`, which is unconditional — no reach into `adapter` (behind the
//! `adapter` feature) needed, so `fake` stays buildable on its own. It lists
//! no other feature in Cargo.toml, and a test double that quietly required
//! the real model's dependencies would defeat the point of it.

use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::embedder::{l2_normalize, Embedder};
use crate::modelcfg::Role;

pub struct FakeEmbedder {
    calls: AtomicUsize,
    dim: usize,
}

impl Default for FakeEmbedder {
    fn default() -> Self {
        Self::new(384)
    }
}

impl FakeEmbedder {
    pub fn new(dim: usize) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            dim,
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn reset_calls(&self) {
        self.calls.store(0, Ordering::SeqCst);
    }
}

impl Embedder for FakeEmbedder {
    /// FNV-1a over `(sha, index)` per dimension, mapped into `[-1, 1]` and
    /// L2-normalised, so the fake vectors live on the unit sphere like the
    /// model's. The role is ignored on purpose: the fake models the store,
    /// not asymmetric retrieval.
    fn embed(&self, text: &str, _role: Role) -> Result<Vec<f32>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let sha = format!("{:x}", Sha256::digest(text.as_bytes()));
        let mut v: Vec<f32> = (0..self.dim)
            .map(|i| {
                let h = fnv1a(&[&sha, "\0", &i.to_string()]);
                // Top 24 bits → a float in [0, 1), then centred.
                ((h >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
            })
            .collect();
        l2_normalize(&mut v);
        Ok(v)
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

/// 64-bit FNV-1a over the concatenation of `parts`. Copied from
/// `context-provider::ids::fnv1a` — standard offset basis and prime, small
/// and stable enough that a path dependency is not worth it here.
fn fnv1a(parts: &[&str]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fake_is_deterministic_and_unit_length() {
        let fake = FakeEmbedder::default();
        let a = fake.embed("Type: Model. Name: X.", Role::Passage).unwrap();
        let b = fake.embed("Type: Model. Name: X.", Role::Passage).unwrap();
        let c = fake.embed("Type: Model. Name: Y.", Role::Passage).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "norm {norm}");
        assert_eq!(a.len(), 384);
    }
}
