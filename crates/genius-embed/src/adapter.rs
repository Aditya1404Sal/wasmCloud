//! One ONNX model, loaded and asked for vectors.
//!
//! This is `cx-embed::Embedder` with its five MiniLM assumptions turned into
//! configuration. It is a separate implementation rather than a change to that
//! crate on purpose: `cx-embed` is frozen (FREEZE.md §4, "different vectors,
//! different vector space, no comparison") and its parity assertion is what
//! makes the published study reproducible. Generalising it in place would
//! disarm the guard while the study it guards is still being cited.
//!
//! The incumbent path is held to that same standard from the other side: the
//! tests below assert this crate's pooling is bit-identical to
//! `cx_embed::mean_pool_l2`, so `all-MiniLM-L6-v2` measured here is the same
//! model measured there, and its baseline column means what it says.

use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};
use ort::session::Session;
use ort::value::Tensor;

use crate::embedder::l2_normalize;
use crate::modelcfg::{ModelConfig, Pooling, Role};

pub struct Adapter {
    /// `Session::run` takes `&mut self`. Serialised rather than pooled, which
    /// is also what makes the latency figures in stage 5 mean "one query, one
    /// forward pass" rather than "one query, contending with three others".
    session: Mutex<Session>,
    tokenizer: tokenizers::Tokenizer,
    cfg: ModelConfig,
    /// Width the model actually returned on its first forward pass. Never
    /// configured — see the note in `embed_raw`.
    dim: usize,
    /// Told how long each forward pass took, in seconds.
    ///
    /// A callback rather than a metrics call, because this crate is also
    /// linked into a wasmCloud host plugin that has no Prometheus registry.
    /// The timing scope is unchanged: tokenize, run, pool, normalise — the
    /// same span the sidecar has always reported.
    observer: Option<std::sync::Arc<dyn Fn(Role, f64) + Send + Sync>>,
}

impl Adapter {
    pub fn load(cfg: &ModelConfig) -> Result<Self> {
        Self::load_with_threads(cfg, 4, None)
    }

    /// `intra_threads` defaults to 4 to match `cx-embed` (`lib.rs:87`), so the
    /// incumbent's latency is comparable to the published 7.58 ms rather than
    /// merely similar.
    pub fn load_with_threads(
        cfg: &ModelConfig,
        intra_threads: usize,
        observer: Option<std::sync::Arc<dyn Fn(Role, f64) + Send + Sync>>,
    ) -> Result<Self> {
        let _ = ort::init().commit();

        let session = Session::builder()
            .context("ort session builder")?
            .with_intra_threads(intra_threads)
            .context("ort intra threads")?
            .commit_from_file(&cfg.model_path)
            .with_context(|| format!("load ONNX model {}", cfg.model_path.display()))?;

        let tokenizer = tokenizers::Tokenizer::from_file(&cfg.tokenizer_path)
            .map_err(|e| anyhow!("load tokenizer {}: {e}", cfg.tokenizer_path.display()))?;

        let mut me = Self {
            session: Mutex::new(session),
            tokenizer,
            cfg: cfg.clone(),
            dim: 0,
            observer,
        };

        // Establish the width from a real forward pass at load, so a caller
        // never sees `dim() == 0` and a misconfigured model fails here rather
        // than midway through a corpus.
        let probe = me.embed(" ", Role::Passage)?;
        me.dim = probe.len();
        if me.dim == 0 {
            bail!(
                "{} returned a zero-width vector on a probe input",
                cfg.model_id
            );
        }
        Ok(me)
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn config(&self) -> &ModelConfig {
        &self.cfg
    }

    /// Embed one text in one role.
    ///
    /// The role is not optional and has no default. `cx-embed::embed` takes
    /// only `&str`, which is why the study's vectors record
    /// `prefix_policy: none` — not as a finding about MiniLM but because the
    /// signature could not express anything else.
    pub fn embed(&self, text: &str, role: Role) -> Result<Vec<f32>> {
        let prefix = self.cfg.prefix_for(role);
        let prefixed;
        let input = if prefix.is_empty() {
            text
        } else {
            prefixed = format!("{prefix}{text}");
            &prefixed
        };
        // The forward pass alone, both roles, so ingest throughput and query
        // latency come from one series.
        let started = std::time::Instant::now();
        let out = self.embed_raw(input);
        if let Some(observe) = &self.observer {
            observe(role, started.elapsed().as_secs_f64());
        }
        out
    }

    fn embed_raw(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow!("tokenization failed: {e}"))?;

        let mut ids: Vec<i64> = encoding.get_ids().iter().map(|id| i64::from(*id)).collect();
        let mut mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|m| i64::from(*m))
            .collect();
        let mut types: Vec<i64> = encoding
            .get_type_ids()
            .iter()
            .map(|t| i64::from(*t))
            .collect();

        ids.truncate(self.cfg.max_sequence);
        mask.truncate(self.cfg.max_sequence);
        types.truncate(self.cfg.max_sequence);
        types.resize(ids.len(), 0);

        let seq_len = ids.len();
        if seq_len == 0 {
            // Width is only known after a successful pass. At load-probe time
            // it is still 0, which is correct: an empty encoding of a non-empty
            // corpus text is a tokenizer fault, and a zero vector would rank
            // arbitrarily rather than fail.
            return Ok(vec![0.0; self.dim]);
        }

        let ids_t = Tensor::<i64>::from_array(([1, seq_len], ids.into_boxed_slice()))
            .context("input_ids tensor")?;
        let mask_t = Tensor::<i64>::from_array(([1, seq_len], mask.clone().into_boxed_slice()))
            .context("attention_mask tensor")?;

        let (data, shape) = {
            let mut session = self
                .session
                .lock()
                .map_err(|_| anyhow!("adapter session mutex poisoned"))?;

            // Two arms rather than a dynamically built input map: these are the
            // only two shapes that occur, and feeding an input the graph does
            // not declare is a hard error from `Session::run` — loud, which is
            // what we want. `cx-embed` always feeds token_type_ids because
            // MiniLM always wants them; most E5/BGE/GTE exports omit the input
            // entirely.
            let outputs = if self.cfg.needs_token_type_ids {
                let types_t = Tensor::<i64>::from_array(([1, seq_len], types.into_boxed_slice()))
                    .context("token_type_ids tensor")?;
                session
                    .run(ort::inputs![
                        "input_ids" => ids_t,
                        "attention_mask" => mask_t,
                        "token_type_ids" => types_t,
                    ])
                    .context("ONNX inference")?
            } else {
                session
                    .run(ort::inputs![
                        "input_ids" => ids_t,
                        "attention_mask" => mask_t,
                    ])
                    .context("ONNX inference")?
            };

            let value = match &self.cfg.output_name {
                Some(name) => outputs
                    .get(name.as_str())
                    .ok_or_else(|| anyhow!("model has no output named {name:?}"))?,
                None => &outputs[0],
            };
            let array = value
                .try_extract_array::<f32>()
                .context("extract output tensor")?;
            let shape: Vec<usize> = array.shape().to_vec();
            let data: Vec<f32> = array.iter().copied().collect();
            (data, shape)
        };

        let pooled = self.pool(&data, &shape, &mask)?;

        let mut v = pooled;
        if self.cfg.normalize {
            l2_normalize(&mut v);
        }

        // Width comes from the tensor the model returned, never from
        // configuration — the same rule `cx-embed` states at `lib.rs:165-168`.
        // Trusting a configured width against a wider model reinterprets the
        // token rows at the wrong stride and yields a well-formed, normalised,
        // entirely meaningless vector with no error anywhere. Here the width is
        // a *result*, so there is nothing to mistrust.
        if self.dim != 0 && v.len() != self.dim {
            bail!(
                "{} returned a {}-wide vector after previously returning {}-wide. \
                 One model is one width; this is a graph with a data-dependent output.",
                self.cfg.model_id,
                v.len(),
                self.dim
            );
        }
        Ok(v)
    }

    fn pool(&self, data: &[f32], shape: &[usize], mask: &[i64]) -> Result<Vec<f32>> {
        match self.cfg.pooling {
            Pooling::PooledOutput => {
                let hidden = match shape {
                    [_batch, h] => *h,
                    other => bail!(
                        "{}: pooling is pooled_output, which needs a rank-2 [batch, hidden] \
                         tensor; this output is {other:?}. Naming the unpooled tensor here \
                         would pool it again and call the result the model's own pooling.",
                        self.cfg.model_id
                    ),
                };
                Ok(data[..hidden].to_vec())
            }
            strategy => {
                let (seq_len, hidden) = match shape {
                    [_batch, s, h] => (*s, *h),
                    other => bail!(
                        "{}: expected a rank-3 [batch, sequence, hidden] output, got {other:?}. \
                         If this model exports an already-pooled tensor, set \
                         \"pooling\": \"pooled_output\" and name it in output_name.",
                        self.cfg.model_id
                    ),
                };
                match strategy {
                    Pooling::Mean => Ok(pool_mean(data, mask, seq_len, hidden)),
                    Pooling::Cls => Ok(data[..hidden].to_vec()),
                    Pooling::LastToken => {
                        // Last position the mask actually admits. Taking
                        // `seq_len - 1` unconditionally reads padding on any
                        // batched or padded encoding and returns a vector that
                        // is the same for every text sharing a pad tail.
                        let last = mask
                            .iter()
                            .take(seq_len)
                            .rposition(|m| *m != 0)
                            .ok_or_else(|| {
                                anyhow!("{}: attention mask is all zero", self.cfg.model_id)
                            })?;
                        Ok(data[last * hidden..(last + 1) * hidden].to_vec())
                    }
                    Pooling::PooledOutput => unreachable!("handled above"),
                }
            }
        }
    }
}

/// Attention-masked mean over the token dimension.
///
/// Split out of normalisation so `ModelConfig::normalize` is an honest flag
/// rather than something baked into the pooling. Composed with
/// [`l2_normalize`], this is bit-identical to `cx_embed::mean_pool_l2` —
/// asserted in the tests below, which is what lets the incumbent baseline claim
/// to be the published model rather than a re-implementation of it.
pub fn pool_mean(
    token_embeddings: &[f32],
    attention_mask: &[i64],
    seq_len: usize,
    hidden_size: usize,
) -> Vec<f32> {
    let mut pooled = vec![0.0f32; hidden_size];
    let mut mask_sum = 0.0f32;

    for i in 0..seq_len {
        let mask = attention_mask[i] as f32;
        mask_sum += mask;
        for j in 0..hidden_size {
            pooled[j] += token_embeddings[i * hidden_size + j] * mask;
        }
    }

    if mask_sum > 0.0 {
        for val in &mut pooled {
            *val /= mask_sum;
        }
    }
    pooled
}

/// Truncate to a Matryoshka width and renormalise.
///
/// Refuses a width the model was not trained for. Slicing a non-Matryoshka
/// model produces a shorter vector that still ranks, still normalises and is
/// simply worse — a silent quality loss that would be attributed to the
/// dimension rather than to the mistake.
pub fn matryoshka_truncate(v: &[f32], to: usize, cfg: &ModelConfig) -> Result<Vec<f32>> {
    if !cfg.matryoshka_dims.contains(&to) {
        bail!(
            "{} does not declare Matryoshka width {to} (declares {:?}). Truncating a model not \
             trained for it degrades quality with no error, and the loss would be read as a \
             property of the dimension.",
            cfg.model_id,
            cfg.matryoshka_dims
        );
    }
    if to > v.len() {
        bail!("cannot truncate a {}-wide vector to {to}", v.len());
    }
    let mut out = v[..to].to_vec();
    l2_normalize(&mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim `cx_embed::mean_pool_l2`, present ONLY as a test oracle.
    ///
    /// Copied rather than depended on for the reason `cx-embed` gives for its
    /// own copy of the ruvector-core original: the point is to hold the pooling
    /// identical, which a 20-line copy makes checkable by eye. Depending on
    /// `cx-embed` here would drag a second `ort` link into this crate's graph.
    fn oracle_mean_pool_l2(
        token_embeddings: &[f32],
        attention_mask: &[i64],
        seq_len: usize,
        hidden_size: usize,
    ) -> Vec<f32> {
        let mut pooled = vec![0.0f32; hidden_size];
        let mut mask_sum = 0.0f32;
        for i in 0..seq_len {
            let mask = attention_mask[i] as f32;
            mask_sum += mask;
            for j in 0..hidden_size {
                pooled[j] += token_embeddings[i * hidden_size + j] * mask;
            }
        }
        if mask_sum > 0.0 {
            for val in &mut pooled {
                *val /= mask_sum;
            }
        }
        let norm: f32 = pooled.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for val in &mut pooled {
                *val /= norm;
            }
        }
        pooled
    }

    /// Deterministic, dependency-free, and stable across versions — the same
    /// reason `cx-bench` refuses `rand` for its churn schedule.
    fn splitmix(state: &mut u64) -> f32 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // Symmetric about zero, so cancellation in the mean is exercised.
        ((z >> 40) as f32 / 8_388_608.0) - 1.0
    }

    #[test]
    fn mean_pooling_is_bit_identical_to_cx_embed() {
        let mut state = 0x5EED_1234_u64;
        for (seq_len, hidden) in [(1usize, 4usize), (7, 16), (63, 384), (256, 384)] {
            let data: Vec<f32> = (0..seq_len * hidden)
                .map(|_| splitmix(&mut state))
                .collect();
            // A realistic mask: a run of ones then padding.
            let keep = (seq_len / 2).max(1);
            let mask: Vec<i64> = (0..seq_len).map(|i| if i < keep { 1 } else { 0 }).collect();

            let mut ours = pool_mean(&data, &mask, seq_len, hidden);
            l2_normalize(&mut ours);
            let theirs = oracle_mean_pool_l2(&data, &mask, seq_len, hidden);

            assert_eq!(
                ours, theirs,
                "pooling diverged from cx-embed at seq_len={seq_len} hidden={hidden}. \
                 The incumbent baseline would no longer be the published model."
            );
        }
    }

    #[test]
    fn both_zero_guards_hold() {
        // Empty mask: mask_sum == 0, so the mean must not divide by zero.
        let data = vec![1.0f32; 8];
        let mask = vec![0i64; 2];
        let ours = pool_mean(&data, &mask, 2, 4);
        assert_eq!(ours, vec![0.0; 4]);
        assert_eq!(ours, oracle_mean_pool_l2(&data, &mask, 2, 4));

        // All-zero embeddings: norm == 0, so normalisation must not divide by
        // zero and must leave the vector alone rather than producing NaN.
        let mut z = vec![0.0f32; 4];
        l2_normalize(&mut z);
        assert!(z.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn matryoshka_refuses_undeclared_widths() {
        let cfg = ModelConfig {
            model_id: "t".into(),
            model_path: ".".into(),
            tokenizer_path: ".".into(),
            artifact_sha256: None,
            tokenizer_sha256: None,
            pooling: Pooling::Mean,
            normalize: true,
            query_prefix: String::new(),
            passage_prefix: String::new(),
            needs_token_type_ids: false,
            output_name: None,
            max_sequence: 256,
            matryoshka_dims: vec![128, 256],
            license: "x".into(),
            notes: String::new(),
            source: None,
        };
        let v = vec![0.5f32; 512];
        assert!(matryoshka_truncate(&v, 128, &cfg).is_ok());
        assert!(matryoshka_truncate(&v, 192, &cfg).is_err());

        let out = matryoshka_truncate(&v, 128, &cfg).unwrap();
        assert_eq!(out.len(), 128);
        let norm: f32 = out.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "truncation must renormalise");
    }
}
