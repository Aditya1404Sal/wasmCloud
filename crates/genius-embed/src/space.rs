//! Embedding-space identity, generalised over [`ModelConfig`].
//!
//! The 17-field schema, its serde spellings, the canonical-JSON form and the
//! domain-separated id computation are reproduced from the research
//! workspace's `cx-embed` (itself mirroring `ruvector-core`'s closed
//! `EmbeddingSpaceIdentity` contract, pinned by
//! `schemas/embedding-space-identity-v1.json` at exactly 17 properties). What
//! changes here is only the *inputs*: `cx-embed` hardcoded the MiniLM answers
//! as consts; this derives every field from the loaded [`ModelConfig`] plus
//! the artifact bytes on disk.
//!
//! Why this exists at all: granite and MiniLM are both 384-dimensional, so
//! width checks, norms and digests catch nothing when a corpus written by one
//! model is queried through the other — the result is finite, unit-norm,
//! correctly-shaped, entirely meaningless similarity with no error anywhere.
//! The space id is the only guard. The store records it at ingest and the
//! service refuses to search a corpus whose recorded space differs from its
//! own (`store.rs`), which is precisely the "records its model" requirement
//! the store benchmark scored as E.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::modelcfg::{ModelConfig, Pooling};

/// Domain separation tag, byte-for-byte from ruvector-core, trailing NUL
/// included — it keeps the tag from running into the JSON's leading `{`.
const SPACE_DOMAIN: &[u8] = b"ruvector.embedding-space.v1\0";

/// Deliberately contains no crate version and no `ort` version: the id must be
/// a function of the embedding function alone, or a routine dependency bump
/// would re-key every persisted corpus without a single float changing.
const RUNTIME_REVISION: &str = "ort/space-rev-1";

const PROVIDER: &str = "ort";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RolePolicy {
    Symmetric,
    Asymmetric,
}

/// Untagged on purpose, mirroring ruvector-core: a named strategy serialises
/// to the bare string `"mean"`, and only a custom one grows an object. A
/// tagged representation would change the canonical JSON and therefore every id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PoolingStrategy {
    Named(PoolingStrategyName),
    Custom {
        custom: String,
        implementation_revision: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PoolingStrategyName {
    Mean,
    Cls,
    LastToken,
    WeightedMean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputDtype {
    F32,
    F16,
    Bf16,
    I8,
    U8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DistanceMetric {
    Cosine,
    Dot,
    Euclidean,
    Manhattan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrefixPolicy {
    None,
    Required,
    QueryRecommended,
    Custom,
}

/// Complete immutable identity of a retrieval vector space.
///
/// Seventeen fields, no more: the schema is closed, and every one of them is
/// hashed into [`SpaceIdentity::space_id`]. Adding a field silently re-keys
/// every corpus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceIdentity {
    pub schema_version: u16,
    pub provider: String,
    pub model_id: String,
    pub model_artifact_sha256: String,
    pub model_graph_sha256: String,
    pub tokenizer_sha256: String,
    pub prompt_template_sha256: String,
    pub pooling_strategy: PoolingStrategy,
    pub normalize: bool,
    pub truncation_tokens: u32,
    pub output_dimension: u32,
    pub output_dtype: OutputDtype,
    pub runtime_revision: String,
    pub distance_metric: DistanceMetric,
    pub role_policy: RolePolicy,
    pub prefix_policy: PrefixPolicy,
    pub prefix_policy_version: u32,
}

impl SpaceIdentity {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            bail!(
                "unsupported embedding identity schema version {}",
                self.schema_version
            );
        }
        if self.output_dimension == 0
            || self.truncation_tokens == 0
            || self.prefix_policy_version == 0
        {
            bail!("embedding identity dimensions, truncation, and policy version must be positive");
        }
        for (name, value) in [
            ("provider", self.provider.as_str()),
            ("model_id", self.model_id.as_str()),
            ("runtime_revision", self.runtime_revision.as_str()),
        ] {
            if value.is_empty() {
                bail!("embedding identity {name} must be non-empty");
            }
        }
        // The four digests are the only artifact provenance in the schema. A
        // placeholder here produces a well-formed id for a vector space nobody
        // can identify, which is worse than an error.
        for (name, value) in [
            ("model_artifact_sha256", self.model_artifact_sha256.as_str()),
            ("model_graph_sha256", self.model_graph_sha256.as_str()),
            ("tokenizer_sha256", self.tokenizer_sha256.as_str()),
            (
                "prompt_template_sha256",
                self.prompt_template_sha256.as_str(),
            ),
        ] {
            let hex = value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !hex {
                bail!("embedding identity {name} must be lowercase SHA-256 hex");
            }
        }
        Ok(())
    }

    /// RFC-8785-style canonical JSON: the schema contains no non-integer
    /// numbers, so recursively sorting object keys is sufficient.
    pub fn canonical_json(&self) -> String {
        // serde_json owns the escaping. Hand-rolling it is precisely how two
        // canonicalisers end up disagreeing on a control character nobody
        // thought to test, and here that disagreement is a changed space id.
        fn push_string(s: &str, out: &mut String) {
            out.push_str(&serde_json::Value::String(s.to_owned()).to_string());
        }
        fn canonical(value: &serde_json::Value, out: &mut String) {
            match value {
                serde_json::Value::Null => out.push_str("null"),
                serde_json::Value::Bool(v) => out.push_str(if *v { "true" } else { "false" }),
                serde_json::Value::Number(v) => out.push_str(&v.to_string()),
                serde_json::Value::String(v) => push_string(v, out),
                serde_json::Value::Array(values) => {
                    out.push('[');
                    for (index, item) in values.iter().enumerate() {
                        if index > 0 {
                            out.push(',');
                        }
                        canonical(item, out);
                    }
                    out.push(']');
                }
                serde_json::Value::Object(values) => {
                    out.push('{');
                    let mut keys: Vec<_> = values.keys().collect();
                    keys.sort();
                    for (index, key) in keys.iter().enumerate() {
                        if index > 0 {
                            out.push(',');
                        }
                        push_string(key, out);
                        out.push(':');
                        canonical(&values[*key], out);
                    }
                    out.push('}');
                }
            }
        }
        // Infallible by construction: every field is a String, a bool, an
        // integer or a plain derived enum.
        let value =
            serde_json::to_value(self).expect("SpaceIdentity is plain data serde cannot fail on");
        let mut output = String::new();
        canonical(&value, &mut output);
        output
    }

    /// `sha256(domain-tag || canonical JSON)`, 64 lowercase hex.
    pub fn space_id(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(SPACE_DOMAIN);
        hash.update(self.canonical_json().as_bytes());
        format!("{:x}", hash.finalize())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The identity of the vector space this configuration + these artifact bytes
/// produce.
///
/// `output_dimension` is passed in from the loaded adapter — the adapter
/// learns it from a real forward pass at load rather than trusting
/// configuration, and the identity must describe what the code does.
pub fn space_identity(cfg: &ModelConfig, output_dimension: usize) -> Result<SpaceIdentity> {
    let model_bytes = std::fs::read(&cfg.model_path)
        .with_context(|| format!("hash ONNX model {}", cfg.model_path.display()))?;
    let tokenizer_bytes = std::fs::read(&cfg.tokenizer_path)
        .with_context(|| format!("hash tokenizer {}", cfg.tokenizer_path.display()))?;

    // Both digests are the artifact digest for a single-file ONNX export: the
    // artifact *is* the graph. Two fields exist for runtimes that compile a
    // graph separately from its weights.
    let artifact = sha256_hex(&model_bytes);

    // The bundle format is ruvector-core's `PromptTemplates` preimage. For a
    // symmetric, unprefixed model this reproduces the exact MiniLM bundle
    // (`query={text}\0passage={text}`), so the incumbent's space id is
    // unchanged by this generalisation; a prefixed model hashes its actual
    // prefixes in, so changing a prefix changes the space — as it must, since
    // it changes every vector.
    let template_bundle = format!(
        "query={}{{text}}\0passage={}{{text}}",
        cfg.query_prefix, cfg.passage_prefix
    );

    let asymmetric = cfg.is_asymmetric();
    let prefixed = !cfg.query_prefix.is_empty() || !cfg.passage_prefix.is_empty();

    let identity = SpaceIdentity {
        schema_version: 1,
        provider: PROVIDER.to_string(),
        model_id: cfg.model_id.clone(),
        model_artifact_sha256: artifact.clone(),
        model_graph_sha256: artifact,
        tokenizer_sha256: sha256_hex(&tokenizer_bytes),
        prompt_template_sha256: sha256_hex(template_bundle.as_bytes()),
        pooling_strategy: match cfg.pooling {
            Pooling::Mean => PoolingStrategy::Named(PoolingStrategyName::Mean),
            Pooling::Cls => PoolingStrategy::Named(PoolingStrategyName::Cls),
            Pooling::LastToken => PoolingStrategy::Named(PoolingStrategyName::LastToken),
            // Not one of the named strategies in the closed schema: the model
            // ships its own pooling inside the graph. Identified by the output
            // it is read from, which is what distinguishes two such models.
            Pooling::PooledOutput => PoolingStrategy::Custom {
                custom: "pooled-output".to_string(),
                implementation_revision: cfg
                    .output_name
                    .clone()
                    .unwrap_or_else(|| "output-0".to_string()),
            },
        },
        normalize: cfg.normalize,
        // What this code does, not what the model card claims: the adapter
        // truncates at `max_sequence`.
        truncation_tokens: u32::try_from(cfg.max_sequence).context("max_sequence exceeds u32")?,
        output_dimension: u32::try_from(output_dimension).context("dimension exceeds u32")?,
        output_dtype: OutputDtype::F32,
        runtime_revision: RUNTIME_REVISION.to_string(),
        // Cosine distance, ascending — what pgvector's `<=>` under
        // `vector_cosine_ops` returns and what the store orders by.
        distance_metric: DistanceMetric::Cosine,
        role_policy: if asymmetric {
            RolePolicy::Asymmetric
        } else {
            RolePolicy::Symmetric
        },
        prefix_policy: if prefixed {
            PrefixPolicy::Required
        } else {
            PrefixPolicy::None
        },
        prefix_policy_version: 1,
    };
    identity.validate()?;
    Ok(identity)
}

/// Hash the artifacts named by a config without loading a session — used by
/// tooling that wants to name a space cheaply. Same construction as
/// [`space_identity`] but the dimension must be supplied by the caller.
///
/// No caller in this workspace today. Kept because this module is mirrored in
/// another repository and the two are reconciled by hand, so a
/// deletion here is a divergence rather than a saving; searched for and
/// recorded so the next reader does not repeat the search.
pub fn artifact_digests(cfg: &ModelConfig) -> Result<(String, String)> {
    let model_bytes = std::fs::read(&cfg.model_path)
        .with_context(|| format!("hash ONNX model {}", cfg.model_path.display()))?;
    let tokenizer_bytes = std::fs::read(&cfg.tokenizer_path)
        .with_context(|| format!("hash tokenizer {}", cfg.tokenizer_path.display()))?;
    Ok((sha256_hex(&model_bytes), sha256_hex(&tokenizer_bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modelcfg::ModelConfig;
    use std::io::Write;

    fn cfg_with(pooling: &str, query_prefix: &str) -> (tempdir::Dir, ModelConfig) {
        // Two tiny files stand in for the artifacts; the identity hashes
        // bytes, not ONNX structure.
        let dir = tempdir::Dir::new("space-test");
        let model = dir.path.join("model.onnx");
        let tokenizer = dir.path.join("tokenizer.json");
        std::fs::File::create(&model)
            .and_then(|mut f| f.write_all(b"model-bytes"))
            .expect("write model stub");
        std::fs::File::create(&tokenizer)
            .and_then(|mut f| f.write_all(b"tokenizer-bytes"))
            .expect("write tokenizer stub");
        let json = format!(
            r#"{{
                "model_id": "test-model",
                "model_path": {model:?},
                "tokenizer_path": {tokenizer:?},
                "pooling": "{pooling}",
                "query_prefix": "{query_prefix}",
                "license": "Apache-2.0"
            }}"#,
            model = model,
            tokenizer = tokenizer,
        );
        let cfg: ModelConfig = serde_json::from_str(&json).expect("parse test config");
        (dir, cfg)
    }

    /// Minimal self-cleaning temp dir; avoids a dev-dependency for two tests.
    /// Unique per call, not per process: tests run in parallel threads of one
    /// process, and a shared directory lets one test's cleanup race another's
    /// reads.
    mod tempdir {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        pub struct Dir {
            pub path: std::path::PathBuf,
        }
        impl Dir {
            pub fn new(tag: &str) -> Self {
                let path = std::env::temp_dir().join(format!(
                    "genius-context-provider-{tag}-{}-{}",
                    std::process::id(),
                    SEQ.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).expect("create temp dir");
                Self { path }
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }

    #[test]
    fn space_id_is_stable_and_64_hex() {
        let (_d, cfg) = cfg_with("cls", "");
        let a = space_identity(&cfg, 384).expect("identity");
        let b = space_identity(&cfg, 384).expect("identity");
        assert_eq!(a.space_id(), b.space_id());
        assert_eq!(a.space_id().len(), 64);
        assert!(a.space_id().bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn pooling_changes_the_space() {
        let (_d, cls) = cfg_with("cls", "");
        let (_d2, mean) = cfg_with("mean", "");
        let a = space_identity(&cls, 384).expect("identity");
        let b = space_identity(&mean, 384).expect("identity");
        // Same artifacts, same dimension — only pooling differs. If these ids
        // agreed, a CLS corpus queried with mean pooling would be served
        // without complaint, which is the exact silent failure the id guards.
        assert_ne!(a.space_id(), b.space_id());
    }

    #[test]
    fn prefixes_change_the_space() {
        let (_d, plain) = cfg_with("mean", "");
        let (_d2, prefixed) = cfg_with("mean", "query: ");
        let a = space_identity(&plain, 384).expect("identity");
        let b = space_identity(&prefixed, 384).expect("identity");
        assert_ne!(a.space_id(), b.space_id());
        assert_eq!(a.prefix_policy, PrefixPolicy::None);
        assert_eq!(b.prefix_policy, PrefixPolicy::Required);
        assert_eq!(b.role_policy, RolePolicy::Asymmetric);
    }

    #[test]
    fn named_pooling_serialises_to_bare_string() {
        // The untagged representation is part of the cross-language contract;
        // a tagged one would change the canonical JSON and every id.
        let s = serde_json::to_string(&PoolingStrategy::Named(PoolingStrategyName::Cls))
            .expect("serialise");
        assert_eq!(s, "\"cls\"");
    }
}
