//! What a candidate model IS, as data rather than as constants.
//!
//! `cx-embed` encodes all-MiniLM-L6-v2's shape as compile-time facts: mean
//! pooling is hard-coded (`lib.rs:184` calls `mean_pool_l2` unconditionally
//! while `PoolingStrategy` is recorded and never branched on), `token_type_ids`
//! is always fed, the output must be rank-3, and `DIMENSIONS` is a `const`.
//! Every one of those is correct for the model the study froze and wrong for at
//! least one serious candidate. This struct is that same information, moved
//! from the type system into a file, because here it is the variable.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How a `[batch, seq, hidden]` tensor becomes one vector.
///
/// The reason this exists as a real branch rather than a recorded string:
/// applying mean pooling to a model trained for CLS pooling does not fail. It
/// returns a finite, unit-norm, correctly-shaped, entirely meaningless vector,
/// and every downstream check — norms, dimension, cache digests — passes. The
/// only thing that catches it is the positive control in `controls.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pooling {
    /// Attention-masked mean over the token dimension. MiniLM, E5, GTE, BGE.
    Mean,
    /// First token. Some BERT-family retrieval models.
    Cls,
    /// Last non-padding token. Decoder-style embedders (Qwen3-Embedding).
    LastToken,
    /// The model already pooled: take a rank-2 `[batch, hidden]` output as-is.
    PooledOutput,
}

/// Which prefix a text gets, and therefore whether the model can tell a
/// question from a thing being asked about.
///
/// `cx-embed::Embedder::embed(&self, text: &str)` takes no role at all
/// (`lib.rs:110`), so it *physically cannot* apply one — and
/// `PROMPT_TEMPLATE_BUNDLE` (`lib.rs:279`) is only ever hashed into the space
/// id, never used as a format string. Omitting E5's `query: ` / `passage: `
/// costs recall silently, which is the second reason for the positive control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Query,
    Passage,
}

impl Role {
    /// The metric label for this role. Lowercase because Prometheus label
    /// values are compared byte-for-byte and every dashboard would otherwise
    /// spell it two ways.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Query => "query",
            Role::Passage => "passage",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Stable name used in results, cache keys and report tables.
    pub model_id: String,

    /// The `.onnx` file itself, not its directory. `cx-embed` takes a directory
    /// and joins `model.onnx` (`lib.rs:59`) because that is the shape the
    /// wasip2 models tree has; candidates come from many places and name their
    /// exports many things (`model.onnx`, `model_quantized.onnx`, `onnx/…`).
    pub model_path: PathBuf,
    pub tokenizer_path: PathBuf,

    /// SHA-256 of the ONNX artifact the research measured, when pinned.
    ///
    /// Pinned in the descriptor rather than in `fetch-model`'s source so the
    /// file that names the model also names its bytes: the embedding-space
    /// id is derived from those bytes, and a descriptor that can point at a
    /// different artifact than the one validated is exactly how a corpus
    /// becomes silently incomparable with every published number. Optional
    /// so a descriptor for a candidate still under evaluation, which has no
    /// pin yet, keeps parsing; `verify_artifacts` then has nothing to check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokenizer_sha256: Option<String>,

    pub pooling: Pooling,

    /// L2-normalise after pooling. Cosine ranking is invariant to this, but the
    /// stored vectors and any reported distance are not.
    #[serde(default = "yes")]
    pub normalize: bool,

    /// Prepended verbatim, including any trailing space. Empty means none.
    #[serde(default)]
    pub query_prefix: String,
    #[serde(default)]
    pub passage_prefix: String,

    /// Whether to feed `token_type_ids`. Most E5/BGE/GTE exports omit this
    /// input, and feeding an input the graph does not declare is a hard error
    /// from `Session::run` — which is the good kind of failure.
    #[serde(default)]
    pub needs_token_type_ids: bool,

    /// Which output tensor holds the embedding. `None` takes index 0, matching
    /// `cx-embed` (`lib.rs:169`). Models exporting both `last_hidden_state` and
    /// `pooler_output` need this named, or the pooling silently reads the wrong
    /// tensor.
    #[serde(default)]
    pub output_name: Option<String>,

    #[serde(default = "default_max_sequence")]
    pub max_sequence: usize,

    /// Matryoshka widths this model was trained to be truncated to. Empty means
    /// truncation is not supported and must not be attempted — slicing a model
    /// that was not trained for it degrades quality without any error.
    #[serde(default)]
    pub matryoshka_dims: Vec<usize>,

    /// Recorded so stage 0's licence check is an artefact rather than someone's
    /// recollection. Pre-registered failure 11 excludes a model on this field
    /// whatever it scored.
    pub license: String,

    #[serde(default)]
    pub notes: String,

    /// Where a resolver may download this model's artifacts from, when they
    /// are not already on disk. See [`ModelSource`].
    ///
    /// Optional, and absent from every descriptor written before resolving by
    /// name existed: such a descriptor still parses, and still works for a
    /// caller that points at it by path. Only a caller asking for a model BY
    /// NAME needs it, because only that caller may have to fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ModelSource>,
}

/// Where a model's artifacts are published.
///
/// Held here rather than in the code that downloads, so that adding a model to
/// a catalog is writing one file rather than editing a program: the descriptor
/// that names the model, its settings and its digests also names where its
/// bytes come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSource {
    /// Joined with each artifact's path as this descriptor writes it:
    /// `{base_url}/{model_path}`. Published repositories really do keep a
    /// graph in `onnx/` and its tokenizer at the root, so the paths carry
    /// that shape and one base covers both. On disk the artifacts land flat
    /// beside the descriptor, whatever depth they were published at.
    pub base_url: String,
}

fn yes() -> bool {
    true
}

/// 256, matching `cx-embed`'s `MAX_SEQUENCE` (`lib.rs:53`) so the incumbent
/// baseline reproduces the published space. Our entity texts run 15-60 tokens,
/// far inside it; the value matters only for a long-context candidate.
fn default_max_sequence() -> usize {
    256
}

/// SHA-256 of a file, streamed. The ONNX graph is over 400 MB and need not be
/// resident just to be named.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

impl ModelConfig {
    pub fn prefix_for(&self, role: Role) -> &str {
        match role {
            Role::Query => &self.query_prefix,
            Role::Passage => &self.passage_prefix,
        }
    }

    /// Is this model asymmetric — does it distinguish the two sides?
    ///
    /// Reported per model, because "we ran an asymmetric model symmetrically"
    /// is the single most likely way to publish an unfairly low score for a
    /// strong candidate.
    pub fn is_asymmetric(&self) -> bool {
        self.query_prefix != self.passage_prefix
    }

    /// Parse a descriptor and validate it, with artifact paths taken as
    /// written. Kept for callers whose working directory IS the models
    /// directory; the service uses `load_anchored`.
    pub fn load(path: &Path) -> Result<Self> {
        let cfg = Self::parse(path)?;
        cfg.validate(path)?;
        Ok(cfg)
    }

    /// Parse a descriptor, re-anchoring its relative artifact paths to the
    /// descriptor's own directory, and validate.
    ///
    /// MODEL.json carries paths relative to itself so the shipped config
    /// works from any working directory; a bare `load` would test those
    /// paths against the cwd and refuse a perfectly good install.
    pub fn load_anchored(path: &Path) -> Result<Self> {
        let cfg = Self::parse_anchored(path)?;
        cfg.validate(path)?;
        Ok(cfg)
    }

    /// `load_anchored` without the existence checks — for tooling that is
    /// about to CREATE the artifacts and only needs to know where they go
    /// and what bytes they must have.
    pub fn parse_anchored(path: &Path) -> Result<Self> {
        let mut cfg = Self::parse(path)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            if cfg.model_path.is_relative() {
                cfg.model_path = dir.join(&cfg.model_path);
            }
            if cfg.tokenizer_path.is_relative() {
                cfg.tokenizer_path = dir.join(&cfg.tokenizer_path);
            }
        }
        Ok(cfg)
    }

    /// Parse a descriptor with its paths exactly as written — for a caller
    /// that resolves them itself, such as a catalog fetch, where the written
    /// path says where the artifact lives under its SOURCE rather than here.
    pub fn parse(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("read model config {}: {e}", path.display()))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("parse model config {}: {e}", path.display()))
    }

    /// The digests this descriptor pins, paired with the file each governs.
    fn pins(&self) -> [(&'static str, &Path, Option<&str>); 2] {
        [
            ("model", &self.model_path, self.artifact_sha256.as_deref()),
            (
                "tokenizer",
                &self.tokenizer_path,
                self.tokenizer_sha256.as_deref(),
            ),
        ]
    }

    /// Check the artifacts on disk against the pinned digests.
    ///
    /// Run at startup, before the model is loaded: a wrong artifact loads
    /// fine, embeds fine, and writes vectors that nothing downstream can tell
    /// apart from the right ones. This is the only place the substitution is
    /// visible. An unpinned descriptor passes — there is nothing to check
    /// against — which is why the shipped descriptor pins both.
    pub fn verify_artifacts(&self) -> Result<()> {
        for (what, path, expected) in self.pins() {
            let Some(expected) = expected else {
                continue;
            };
            let actual = sha256_file(path)
                .with_context(|| format!("digest the {what} artifact for {}", self.model_id))?;
            if actual != expected {
                bail!(
                    "{what} artifact {} does not match the digest pinned for {}\n  \
                     expected {expected}\n  actual   {actual}\n\
                     The embedding-space id is derived from these bytes, so a different \
                     artifact makes every corpus silently incomparable. Re-run \
                     `cargo r --bin fetch-model` to restore the measured artifact; if the \
                     change is intended, re-validate the model and update \
                     {what_key} in its descriptor.",
                    path.display(),
                    self.model_id,
                    what_key = if what == "model" {
                        "artifact_sha256"
                    } else {
                        "tokenizer_sha256"
                    },
                );
            }
        }
        Ok(())
    }

    fn validate(&self, from: &Path) -> Result<()> {
        let here = from.display();
        if self.model_id.trim().is_empty() {
            bail!("{here}: model_id is empty; it names every result row this model produces");
        }
        if !self.model_path.exists() {
            bail!(
                "{here}: model_path {} does not exist — run `cargo r --bin fetch-model` first",
                self.model_path.display()
            );
        }
        if !self.tokenizer_path.exists() {
            bail!(
                "{here}: tokenizer_path {} does not exist — run `cargo r --bin fetch-model` first",
                self.tokenizer_path.display()
            );
        }
        for (what, _, pin) in self.pins() {
            if let Some(pin) = pin {
                if pin.len() != 64 || !pin.bytes().all(|b| b.is_ascii_hexdigit()) {
                    bail!(
                        "{here}: the {what} digest {pin:?} is not a SHA-256 hex string; \
                         a pin that can never match would refuse every artifact"
                    );
                }
            }
        }
        if self.license.trim().is_empty() {
            bail!(
                "{here}: license is empty. Stage 0 exists to settle this before a model is \
                 measured, so that a licence finding cannot arrive after a score everyone likes."
            );
        }
        if self.max_sequence == 0 {
            bail!("{here}: max_sequence is 0");
        }
        // A pooled-output model has no token dimension to pool over, so any
        // other strategy names an operation that cannot be performed.
        if self.pooling == Pooling::PooledOutput && self.output_name.is_none() {
            bail!(
                "{here}: pooling is pooled_output but output_name is unset. A model exporting a \
                 pooled tensor almost always exports last_hidden_state as output 0 as well, and \
                 taking index 0 by default would read the unpooled one and call it pooled."
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory that disappears with its handle, so a failed assertion
    /// does not leave descriptors behind for the next run to find.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "genius-modelcfg-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Self(dir)
        }

        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).expect("write scratch file");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn descriptor(dir: &Scratch, artifact: Option<&str>, tokenizer: Option<&str>) -> PathBuf {
        let mut json = serde_json::json!({
            "model_id": "t",
            "model_path": "t/model.onnx",
            "tokenizer_path": "t/tokenizer.json",
            "pooling": "cls",
            "license": "Apache-2.0"
        });
        if let Some(a) = artifact {
            json["artifact_sha256"] = serde_json::Value::String(a.to_string());
        }
        if let Some(t) = tokenizer {
            json["tokenizer_sha256"] = serde_json::Value::String(t.to_string());
        }
        dir.write("t.json", json.to_string().as_bytes())
    }

    #[test]
    fn a_descriptor_without_pins_still_parses_and_verifies_trivially() {
        let dir = Scratch::new("unpinned");
        std::fs::create_dir_all(dir.0.join("t")).expect("mkdir");
        dir.write("t/model.onnx", b"graph");
        dir.write("t/tokenizer.json", b"{}");
        let path = descriptor(&dir, None, None);
        let cfg = ModelConfig::load_anchored(&path).expect("parse an unpinned descriptor");
        assert!(cfg.artifact_sha256.is_none());
        cfg.verify_artifacts()
            .expect("nothing pinned means nothing to check");
    }

    #[test]
    fn paths_are_anchored_to_the_descriptor_directory_not_the_cwd() {
        let dir = Scratch::new("anchor");
        std::fs::create_dir_all(dir.0.join("t")).expect("mkdir");
        dir.write("t/model.onnx", b"graph");
        dir.write("t/tokenizer.json", b"{}");
        let path = descriptor(&dir, None, None);
        let cfg = ModelConfig::load_anchored(&path).expect("load");
        assert_eq!(cfg.model_path, dir.0.join("t/model.onnx"));
        assert!(
            ModelConfig::load(&path).is_err(),
            "the unanchored loader tests paths against the cwd and must refuse"
        );
    }

    #[test]
    fn matching_pins_pass_and_a_substituted_artifact_is_named_with_both_digests() {
        let dir = Scratch::new("pins");
        std::fs::create_dir_all(dir.0.join("t")).expect("mkdir");
        dir.write("t/model.onnx", b"graph");
        dir.write("t/tokenizer.json", b"{}");
        let good = descriptor(&dir, Some(&sha256_hex(b"graph")), Some(&sha256_hex(b"{}")));
        ModelConfig::load_anchored(&good)
            .expect("load")
            .verify_artifacts()
            .expect("both digests match");

        let wrong = sha256_hex(b"a different graph");
        let bad = descriptor(&dir, Some(&wrong), Some(&sha256_hex(b"{}")));
        let err = ModelConfig::load_anchored(&bad)
            .expect("load")
            .verify_artifacts()
            .expect_err("a wrong pin must fail")
            .to_string();
        assert!(err.contains("model artifact"), "{err}");
        assert!(err.contains(&wrong), "expected digest missing: {err}");
        assert!(
            err.contains(&sha256_hex(b"graph")),
            "actual digest missing: {err}"
        );
        assert!(
            err.contains("fetch-model"),
            "must say how to recover: {err}"
        );
    }

    #[test]
    fn a_pin_that_is_not_a_sha256_is_refused_before_it_can_reject_every_artifact() {
        let dir = Scratch::new("badpin");
        std::fs::create_dir_all(dir.0.join("t")).expect("mkdir");
        dir.write("t/model.onnx", b"graph");
        dir.write("t/tokenizer.json", b"{}");
        let path = descriptor(&dir, Some("not-a-digest"), None);
        let err = ModelConfig::load_anchored(&path)
            .expect_err("malformed pin")
            .to_string();
        assert!(err.contains("not a SHA-256"), "{err}");
    }

    #[test]
    fn streamed_digest_agrees_with_the_one_shot_digest() {
        let dir = Scratch::new("stream");
        // Larger than the read buffer, so more than one chunk is hashed.
        let bytes: Vec<u8> = (0..(3 << 20)).map(|i| (i % 251) as u8).collect();
        let path = dir.write("big.bin", &bytes);
        assert_eq!(sha256_file(&path).expect("hash"), sha256_hex(&bytes));
    }
}
