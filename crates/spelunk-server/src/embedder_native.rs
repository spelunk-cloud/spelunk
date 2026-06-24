use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::{
    embedding, linear_b, linear_no_bias, rms_norm, rotary_emb::rope, Activation, Embedding,
    Linear, Module, RmsNorm, VarBuilder,
};
use candle_transformers::models::qwen3::Config as Qwen3Config;
use hf_hub::{Repo, RepoType, api::sync::ApiBuilder};
use tokenizers::Tokenizer;

/// Embedding dimension produced by F2LLM-v2-330M (hidden_size = 896).
pub const DIM: usize = 896;

const MODEL_ID: &str = "codefuse-ai/F2LLM-v2-330M";
/// Hard ceiling for token sequences (max_position_embeddings).
const MAX_SEQ_LEN: usize = 40960;
/// Number of sequences processed in one padded forward pass.
/// Sequences within a sub-batch are sorted by length first so padding waste
/// stays bounded.  Tune upward if BLAS efficiency plateaus at larger matrices.
const EMBED_BATCH_SIZE: usize = 8;

pub struct NativeEmbedder {
    inner: Arc<Mutex<EmbedderInner>>,
}

struct EmbedderInner {
    weights: Qwen3EmbedWeights,
    tokenizer: Tokenizer,
}

// ── no-KV-cache Qwen3 embedder ────────────────────────────────────────────────

/// Qwen3 transformer weights loaded once; `forward_one` produces embeddings
/// with no KV cache and no generation state.  Each text gets a fresh causal
/// mask so no state leaks between texts in a batch.
///
/// This avoids `Qwen3Model::clear_kv_cache`, which is `pub(crate)` in candle
/// 0.10.2, while also eliminating the per-text model-recreation overhead.
struct Qwen3EmbedWeights {
    embed_tokens: Embedding,
    layers: Vec<EmbedLayer>,
    final_norm: RmsNorm,
    rope_cos: Tensor, // (MAX_SEQ_LEN, head_dim/2)
    rope_sin: Tensor,
    n_head: usize,
    n_kv_head: usize,
    head_dim: usize,
    device: Device,
}

struct EmbedLayer {
    attn_norm: RmsNorm,
    q: Linear,
    k: Linear,
    v: Linear,
    o: Linear,
    q_norm: RmsNorm,
    k_norm: RmsNorm,
    gate: Linear,
    up: Linear,
    down: Linear,
    post_norm: RmsNorm,
}

impl Qwen3EmbedWeights {
    fn load(cfg: &Qwen3Config, vb: VarBuilder) -> Result<Self> {
        let embed_tokens =
            embedding(cfg.vocab_size, cfg.hidden_size, vb.pp("model.embed_tokens"))?;

        // Pre-compute RoPE sin/cos tables once (matches Qwen3RotaryEmbedding in candle).
        let half_hd = cfg.head_dim / 2;
        let inv_freq: Vec<f32> = (0..half_hd)
            .map(|i| {
                1.0f32 / (cfg.rope_theta as f32).powf(2.0 * i as f32 / cfg.head_dim as f32)
            })
            .collect();
        let inv_freq_t =
            Tensor::from_vec(inv_freq, (1, half_hd), vb.device())?.to_dtype(DType::F32)?;
        let t = Tensor::arange(0u32, MAX_SEQ_LEN as u32, vb.device())?
            .to_dtype(DType::F32)?
            .reshape((MAX_SEQ_LEN, 1))?;
        let freqs = t.matmul(&inv_freq_t)?; // (MAX_SEQ_LEN, head_dim/2)
        let dtype = vb.dtype();
        let rope_cos = freqs.cos()?.to_dtype(dtype)?;
        let rope_sin = freqs.sin()?.to_dtype(dtype)?;

        let bias = cfg.attention_bias;
        let h = cfg.hidden_size;
        let inter = cfg.intermediate_size;
        let nh = cfg.num_attention_heads;
        let nkv = cfg.num_key_value_heads;
        let hd = cfg.head_dim;
        let eps = cfg.rms_norm_eps;
        let vb_l = vb.pp("model.layers");

        let mut layers = Vec::with_capacity(cfg.num_hidden_layers);
        for i in 0..cfg.num_hidden_layers {
            let vb_i = vb_l.pp(i);
            let vb_a = vb_i.pp("self_attn");
            let vb_m = vb_i.pp("mlp");
            layers.push(EmbedLayer {
                attn_norm: rms_norm(h,         eps, vb_i.pp("input_layernorm"))?,
                q:         linear_b(h, nh * hd, bias, vb_a.pp("q_proj"))?,
                k:         linear_b(h, nkv * hd, bias, vb_a.pp("k_proj"))?,
                v:         linear_b(h, nkv * hd, bias, vb_a.pp("v_proj"))?,
                o:         linear_b(nh * hd, h,  bias, vb_a.pp("o_proj"))?,
                q_norm:    rms_norm(hd,        eps, vb_a.pp("q_norm"))?,
                k_norm:    rms_norm(hd,        eps, vb_a.pp("k_norm"))?,
                gate:      linear_no_bias(h, inter, vb_m.pp("gate_proj"))?,
                up:        linear_no_bias(h, inter, vb_m.pp("up_proj"))?,
                down:      linear_no_bias(inter, h, vb_m.pp("down_proj"))?,
                post_norm: rms_norm(h,         eps, vb_i.pp("post_attention_layernorm"))?,
            });
        }

        let final_norm = rms_norm(h, eps, vb.pp("model.norm"))?;
        let device = vb.device().clone();

        Ok(Self {
            embed_tokens,
            layers,
            final_norm,
            rope_cos,
            rope_sin,
            n_head: nh,
            n_kv_head: nkv,
            head_dim: hd,
            device,
        })
    }

    /// Forward pass for one text; returns `[1, seq_len, hidden_size]`.
    fn forward_one(&self, ids: &[u32]) -> Result<Tensor> {
        let seq = ids.len();
        let ids_t = Tensor::new(ids, &self.device)?.unsqueeze(0)?; // [1, seq]
        let mut h = self.embed_tokens.forward(&ids_t)?; // [1, seq, hidden]

        let mask = causal_mask(seq, h.dtype(), &self.device)?; // [1, 1, seq, seq]
        for layer in &self.layers {
            h = self.layer_fwd(&h, layer, &mask)?;
        }
        Ok(self.final_norm.forward(&h)?)
    }

    /// Padded batch forward pass: all sequences in `batch_ids` are right-padded
    /// to the longest sequence in the batch.  Returns one L2-normalised
    /// embedding vector per input sequence in the same order.
    ///
    /// On CPU, sequences are forwarded together as a single BLAS call (batch
    /// dim > 1), amortising per-call overhead.  On Metal/GPU the sequential
    /// path is used instead: Metal's buffer pool grows unboundedly with batched
    /// inference because `(b × n_head × seq²)` attention tensors are never
    /// compacted between forward passes, causing OOM for large codebases.
    /// Sequential GPU inference was the pre-batching baseline and is still fast.
    fn embed_batch(&self, batch_ids: &[&[u32]]) -> Result<Vec<Vec<f32>>> {
        let b = batch_ids.len();
        assert!(b > 0);

        // Sequential path: single sequences, or any GPU/Metal device.
        if b == 1 || !matches!(self.device, Device::Cpu) {
            let mut out = Vec::with_capacity(b);
            for ids in batch_ids {
                let hidden = self.forward_one(ids)?;
                let last = hidden.i((0, ids.len() - 1))?;
                let mut v: Vec<f32> = last.to_dtype(DType::F32)?.to_vec1()?;
                l2_normalise(&mut v);
                anyhow::ensure!(
                    v.iter().all(|x| x.is_finite()),
                    "embedding vector contains NaN/inf — check model weights or sequence length"
                );
                out.push(v);
            }
            return Ok(out);
        }

        let seq_lens: Vec<usize> = batch_ids.iter().map(|ids| ids.len()).collect();
        let max_seq = *seq_lens.iter().max().unwrap();

        // Build flat right-padded buffer [B × max_seq] with pad id = 0.
        let mut flat: Vec<u32> = Vec::with_capacity(b * max_seq);
        for ids in batch_ids {
            flat.extend_from_slice(ids);
            flat.extend(std::iter::repeat(0u32).take(max_seq - ids.len()));
        }
        let ids_t = Tensor::from_slice(&flat, (b, max_seq), &self.device)?;

        // Forward pass: all layers work naturally with batch dim > 1.
        let mut h = self.embed_tokens.forward(&ids_t)?; // [b, max_seq, hidden]
        let mask = causal_mask(max_seq, h.dtype(), &self.device)?; // [1, 1, max_seq, max_seq]
        for layer in &self.layers {
            h = self.layer_fwd(&h, layer, &mask)?;
        }
        let h = self.final_norm.forward(&h)?; // [b, max_seq, hidden]

        // Extract last real token per sequence → L2-normalise.
        let mut out = Vec::with_capacity(b);
        for (i, &seq_len) in seq_lens.iter().enumerate() {
            let last = h.i((i, seq_len - 1))?; // [hidden]
            let mut v: Vec<f32> = last.to_dtype(DType::F32)?.to_vec1()?;
            l2_normalise(&mut v);
            anyhow::ensure!(
                v.iter().all(|x| x.is_finite()),
                "embedding vector contains NaN/inf — check model weights or sequence length"
            );
            out.push(v);
        }
        Ok(out)
    }

    fn layer_fwd(&self, x: &Tensor, layer: &EmbedLayer, mask: &Tensor) -> Result<Tensor> {
        let h = layer.attn_norm.forward(x)?;
        let h = self.attn(&h, layer, mask)?;
        let x = (x + h)?;
        let h = layer.post_norm.forward(&x)?;
        let h = self.mlp(&h, layer)?;
        Ok((x + h)?)
    }

    fn attn(&self, x: &Tensor, layer: &EmbedLayer, mask: &Tensor) -> Result<Tensor> {
        let (b, seq, _) = x.dims3()?;

        let q = layer.q.forward(x)?; // [b, seq, nh*hd]
        let k = layer.k.forward(x)?; // [b, seq, nkv*hd]
        let v = layer.v.forward(x)?; // [b, seq, nkv*hd]

        // Reshape and transpose to [b, n_heads, seq, head_dim]
        let q = q.reshape((b, seq, self.n_head, self.head_dim))?.transpose(1, 2)?;
        let k = k.reshape((b, seq, self.n_kv_head, self.head_dim))?.transpose(1, 2)?;
        let v = v.reshape((b, seq, self.n_kv_head, self.head_dim))?.transpose(1, 2)?;

        // QK RMSNorm (Qwen3 adds a per-head norm before RoPE)
        let q = layer.q_norm.forward(&q)?;
        let k = layer.k_norm.forward(&k)?;

        // RoPE
        let cos = self.rope_cos.narrow(0, 0, seq)?;
        let sin = self.rope_sin.narrow(0, 0, seq)?;
        let q = rope(&q.contiguous()?, &cos, &sin)?;
        let k = rope(&k.contiguous()?, &cos, &sin)?;

        // Grouped query attention: expand K, V from n_kv_heads to n_heads
        let n_rep = self.n_head / self.n_kv_head;
        let k = k.repeat(&[1, n_rep, 1, 1])?;
        let v = v.repeat(&[1, n_rep, 1, 1])?;

        // Scaled dot-product attention + causal mask
        let scale = (self.head_dim as f64).powf(-0.5);
        let scores = (q.matmul(&k.transpose(2, 3)?)? * scale)?;
        let scores = scores.broadcast_add(mask)?;
        let probs = candle_nn::ops::softmax_last_dim(&scores)?;
        let out = probs.matmul(&v)?; // [b, n_head, seq, hd]

        // Merge heads: [b, seq, n_head*hd]
        let out = out
            .transpose(1, 2)?
            .contiguous()?
            .reshape((b, seq, self.n_head * self.head_dim))?;
        Ok(layer.o.forward(&out)?)
    }

    fn mlp(&self, x: &Tensor, layer: &EmbedLayer) -> Result<Tensor> {
        let gate = layer.gate.forward(x)?.apply(&Activation::Silu)?;
        let up = layer.up.forward(x)?;
        Ok(layer.down.forward(&(gate * up)?)?)
    }
}

/// Upper-triangular causal mask: 0.0 where attention is allowed, -∞ otherwise.
fn causal_mask(seq: usize, dtype: DType, device: &Device) -> Result<Tensor> {
    let mask: Vec<f32> = (0..seq)
        .flat_map(|i| (0..seq).map(move |j| if j <= i { 0.0f32 } else { f32::NEG_INFINITY }))
        .collect();
    Ok(Tensor::from_slice(&mask, (1, 1, seq, seq), device)?.to_dtype(dtype)?)
}

// ── NativeEmbedder ────────────────────────────────────────────────────────────

impl NativeEmbedder {
    /// Load (or download) the F2LLM-v2-330M model from HuggingFace Hub.
    ///
    /// On first call, weights (~650 MB safetensors) are downloaded into
    /// `~/.local/share/spelunk/models/` via hf-hub cache. Subsequent calls
    /// use the local cache with no network access. Uses Metal/GPU on macOS
    /// when built with the `metal` cargo feature, CPU otherwise.
    pub fn load() -> Result<Self> {
        let cache_dir = model_cache_dir()?;
        std::fs::create_dir_all(&cache_dir)
            .with_context(|| format!("creating model cache dir {}", cache_dir.display()))?;

        let device = select_device();
        let on_gpu = !matches!(device, Device::Cpu);
        // BF16 matmul is not supported on candle's CPU backend; use F32 there.
        let dtype = if on_gpu { DType::BF16 } else { DType::F32 };
        tracing::info!(
            "loading F2LLM-v2-330M via candle on {} (cache: {})",
            if on_gpu { "Metal/GPU" } else { "CPU" },
            cache_dir.display()
        );

        let api = ApiBuilder::new()
            .with_cache_dir(cache_dir)
            .build()
            .context("building HuggingFace Hub API client")?;
        let repo = api.repo(Repo::new(MODEL_ID.to_string(), RepoType::Model));

        let tokenizer_path = repo
            .get("tokenizer.json")
            .context("downloading F2LLM-v2-330M tokenizer.json")?;
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("loading tokenizer: {e}"))?;

        let config_path = repo
            .get("config.json")
            .context("downloading F2LLM-v2-330M config.json")?;
        let config: Qwen3Config = serde_json::from_str(
            &std::fs::read_to_string(&config_path).context("reading F2LLM-v2-330M config.json")?,
        )
        .context("parsing F2LLM-v2-330M config.json")?;

        let weight_paths = download_weights(&repo)?;
        // F2LLM-v2-330M uses architecture class Qwen3Model (not Qwen3ForCausalLM), so its
        // safetensors keys have no "model." prefix (e.g. "embed_tokens.weight" not
        // "model.embed_tokens.weight"). Qwen3EmbedWeights::load expects the prefixed form,
        // so we add it here while loading.
        let vb = load_weights_prefixed(&weight_paths, dtype, &device)
            .context("loading F2LLM-v2-330M weights")?;

        let weights =
            Qwen3EmbedWeights::load(&config, vb).context("building F2LLM-v2-330M model")?;

        tracing::info!(
            "F2LLM-v2-330M ready (dim={DIM}, device={})",
            if on_gpu { "Metal/GPU" } else { "CPU" }
        );

        Ok(Self {
            inner: Arc::new(Mutex::new(EmbedderInner { weights, tokenizer })),
        })
    }
}

/// Choose the inference device.
///
/// With the `metal` cargo feature (macOS builds), tries Metal first and falls
/// back to CPU on failure. Without `metal`, always returns CPU.
fn select_device() -> Device {
    #[cfg(feature = "metal")]
    {
        match Device::new_metal(0) {
            Ok(d) => return d,
            Err(e) => tracing::warn!("Metal GPU unavailable ({e}); falling back to CPU"),
        }
    }
    Device::Cpu
}

/// Download safetensors weights, handling both single-file and sharded layouts.
fn download_weights(repo: &hf_hub::api::sync::ApiRepo) -> Result<Vec<PathBuf>> {
    if let Ok(index_path) = repo.get("model.safetensors.index.json") {
        let index: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&index_path).context("reading safetensors index")?,
        )
        .context("parsing safetensors index")?;
        let mut shards: Vec<String> = index["weight_map"]
            .as_object()
            .map(|m| {
                m.values()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        shards.sort();
        shards.dedup();
        anyhow::ensure!(!shards.is_empty(), "safetensors index has no weight shards");
        return shards
            .iter()
            .map(|s| {
                repo.get(s)
                    .with_context(|| format!("downloading shard {s}"))
            })
            .collect();
    }
    Ok(vec![
        repo.get("model.safetensors")
            .context("downloading model.safetensors")?,
    ])
}

/// Load safetensors weights and prefix every key with "model." so that
/// Qwen3EmbedWeights::load (which calls vb.pp("model.embed_tokens") etc.) can
/// find them. F2LLM stores keys without this prefix because it is saved as a
/// plain Qwen3Model rather than a Qwen3ForCausalLM wrapper.
fn load_weights_prefixed(
    paths: &[PathBuf],
    dtype: DType,
    device: &Device,
) -> Result<VarBuilder<'static>> {
    let mut tensors: HashMap<String, candle_core::Tensor> = HashMap::new();
    for path in paths {
        let file_tensors = candle_core::safetensors::load(path, device)
            .with_context(|| format!("loading weights from {}", path.display()))?;
        for (k, v) in file_tensors {
            tensors.insert(format!("model.{k}"), v.to_dtype(dtype)?);
        }
    }
    Ok(VarBuilder::from_tensors(tensors, dtype, device))
}

fn model_cache_dir() -> Result<PathBuf> {
    dirs::data_local_dir()
        .map(|d| d.join("spelunk").join("models"))
        .ok_or_else(|| anyhow::anyhow!("could not determine local data directory"))
}

fn l2_normalise(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

#[async_trait::async_trait]
impl spelunk_core::embeddings::EmbeddingBackend for NativeEmbedder {
    /// Embed a batch of strings using F2LLM-v2-330M.
    ///
    /// Texts are tokenized, sorted by token length (to minimise padding waste),
    /// then forwarded through the Qwen3 decoder in padded sub-batches of
    /// `EMBED_BATCH_SIZE`.  Each sub-batch is one BLAS call with batch dim > 1,
    /// which amortises the per-call overhead.  The last token's hidden state is
    /// L2-normalised to produce a 896-dim embedding; results are returned in the
    /// original input order.
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        let owned: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        let inner = Arc::clone(&self.inner);

        tokio::task::spawn_blocking(move || {
            let guard = inner
                .lock()
                .map_err(|_| anyhow::anyhow!("native embedder lock poisoned"))?;

            // 1. Tokenize all texts upfront.
            let mut id_vecs: Vec<Vec<u32>> = Vec::with_capacity(owned.len());
            for text in &owned {
                let encoding = guard
                    .tokenizer
                    .encode(text.as_str(), true) // add_special_tokens=true → appends EOS
                    .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;
                let ids: Vec<u32> = encoding
                    .get_ids()
                    .iter()
                    .take(MAX_SEQ_LEN)
                    .copied()
                    .collect();
                anyhow::ensure!(!ids.is_empty(), "empty token sequence after tokenization");
                id_vecs.push(ids);
            }

            // 2. Sort by token length so sequences in the same sub-batch have
            //    similar lengths, minimising padding waste.
            let mut indexed: Vec<(usize, Vec<u32>)> =
                id_vecs.into_iter().enumerate().collect();
            indexed.sort_unstable_by_key(|(_, ids)| ids.len());

            // 3. Process in sub-batches; reassemble into original order.
            let mut results: Vec<Vec<f32>> = vec![Vec::new(); owned.len()];
            for sub_batch in indexed.chunks(EMBED_BATCH_SIZE) {
                let batch_ids: Vec<&[u32]> =
                    sub_batch.iter().map(|(_, ids)| ids.as_slice()).collect();
                let vecs = guard.weights.embed_batch(&batch_ids)?;
                for ((orig_idx, _), vec) in sub_batch.iter().zip(vecs) {
                    results[*orig_idx] = vec;
                }
            }

            Ok(results)
        })
        .await
        .context("spawn_blocking panicked in native embedder")?
    }

    fn dimension(&self) -> usize {
        DIM
    }
}
