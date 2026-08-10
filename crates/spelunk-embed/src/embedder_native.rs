use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use candle_core::quantized::{QMatMul, QTensor, gguf_file};
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::{Activation, Embedding, Module, RmsNorm, rotary_emb::rope};
use tokenizers::Tokenizer;

use crate::error::EmbedError;

/// Embedding dimension produced by F2LLM-v2-330M (hidden_size = 896).
pub const DIM: usize = 896;

/// Qwen3 `config.json` shape, deserialized directly from the model's config
/// file. Copied from `candle_transformers::models::qwen3::Config` rather than
/// depending on the `candle-transformers` crate for it: that crate bundles
/// ~125 unrelated model architectures (llama, whisper, stable-diffusion,
/// clip, ...) with no per-model feature gating, so pulling it in just for this
/// struct and `repeat_kv` below costs real compile time and `target/` disk
/// space even though the unused code is eliminated from the final linked
/// binary by LTO. Field set must stay in sync with upstream if ever bumped.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
struct Config {
    vocab_size: usize,
    hidden_size: usize,
    intermediate_size: usize,
    num_hidden_layers: usize,
    num_attention_heads: usize,
    head_dim: usize,
    attention_bias: bool,
    num_key_value_heads: usize,
    max_position_embeddings: usize,
    sliding_window: Option<usize>,
    max_window_layers: usize,
    tie_word_embeddings: bool,
    rope_theta: f64,
    rms_norm_eps: f64,
    use_sliding_window: bool,
    hidden_act: Activation,
}

/// Repeats each of `xs`'s key/value heads `n_rep` times along the head axis
/// (grouped-query attention). Copied from `candle_transformers::utils::repeat_kv`;
/// see the `Config` doc comment above for why this isn't a dependency.
fn repeat_kv(xs: Tensor, n_rep: usize) -> Result<Tensor> {
    if n_rep == 1 {
        Ok(xs)
    } else {
        let (b_sz, n_kv_head, seq_len, head_dim) = xs.dims4()?;
        Ok(Tensor::cat(&vec![&xs; n_rep], 2)?.reshape((
            b_sz,
            n_kv_head * n_rep,
            seq_len,
            head_dim,
        ))?)
    }
}

/// Hard ceiling for token sequences (max_position_embeddings).
const MAX_SEQ_LEN: usize = 40960;
/// Number of sequences processed in one padded forward pass.
/// Sequences within a sub-batch are sorted by length first so padding waste
/// stays bounded.  Tune upward if BLAS efficiency plateaus at larger matrices.
const EMBED_BATCH_SIZE: usize = 8;
/// Longest sequence (tokens) allowed in a batched forward pass.
/// The attention score tensor is (b × n_head × max_seq²) so it grows
/// quadratically; beyond this threshold we fall back to forward_one to
/// avoid multi-GB allocations on long code chunks.
/// At 512 tokens: 8 × 16 × 512² × 4 bytes ≈ 134 MB — well within budget.
const BATCH_MAX_SEQ: usize = 512;

/// Number of attention heads in F2LLM-v2-330M (Qwen3, `num_attention_heads`).
/// The single-chunk attention score / probability tensors are
/// `[1, N_HEAD, seq, seq]` f32, so peak scratch grows as `N_HEAD × seq² × 4`.
/// This must match the model `config.json`; it is asserted against the loaded
/// config at startup (see `NativeEmbedder::from_files`).
const N_HEAD: usize = 16;

/// Memory budget (bytes) for the single-chunk attention scratch on hosts with
/// ≤ 16 GiB of total RAM. Conservative default so a full CPU index never OOMs.
const SINGLE_CHUNK_BUDGET_SMALL: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB

/// Memory budget (bytes) for hosts with > 16 GiB of total RAM.
const SINGLE_CHUNK_BUDGET_LARGE: u64 = 4 * 1024 * 1024 * 1024; // 4 GiB

/// RAM threshold (bytes) above which the larger single-chunk budget is used.
const LARGE_RAM_THRESHOLD: u64 = 16 * 1024 * 1024 * 1024; // 16 GiB

/// Pick the single-chunk attention memory budget from total system RAM:
/// 2 GiB by default, 4 GiB when the host has more than 16 GiB of RAM.
fn single_chunk_budget(total_ram_bytes: u64) -> u64 {
    if total_ram_bytes > LARGE_RAM_THRESHOLD {
        SINGLE_CHUNK_BUDGET_LARGE
    } else {
        SINGLE_CHUNK_BUDGET_SMALL
    }
}

/// Derive the maximum single-chunk token count that keeps the attention scratch
/// within `budget_bytes`. In `forward_one` (batch = 1) the dominant allocation
/// is the attention score tensor `[1, n_head, seq, seq]` in f32, so peak scratch
/// is `n_head × seq² × 4 bytes`. Solving for `seq` gives the cap, clamped to
/// `MAX_SEQ_LEN`. At 2 GiB → ~5 792 tokens; at 4 GiB → ~8 192.
fn derive_token_cap(budget_bytes: u64, n_head: usize) -> usize {
    let bytes_per_seq_sq = (n_head as u64) * 4;
    // seq <= sqrt(budget / (n_head * 4))
    let seq_sq = budget_bytes / bytes_per_seq_sq;
    let cap = (seq_sq as f64).sqrt().floor() as usize;
    cap.clamp(1, MAX_SEQ_LEN)
}

/// The per-chunk truncation length actually applied to a tokenized input:
/// the load-time `token_cap` clamped by the model's `MAX_SEQ_LEN` ceiling.
/// A `token_cap` of 0 (unit-test fixtures) means "no extra cap".
fn effective_token_cap(token_cap: usize) -> usize {
    if token_cap == 0 {
        MAX_SEQ_LEN
    } else {
        token_cap.min(MAX_SEQ_LEN)
    }
}

/// Total physical system RAM in bytes, best-effort and cross-platform.
///
/// macOS uses the `hw.memsize` sysctl; Linux reads `MemTotal` from
/// `/proc/meminfo`. On any other platform, or if detection fails, we return a
/// conservative `0` so the caller falls back to the small (2 GiB) budget.
fn total_system_ram() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let mut size: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        let name = c"hw.memsize";
        // SAFETY: `name` is a valid NUL-terminated C string; `size`/`len` point
        // to a u64 and its length. sysctlbyname writes at most `len` bytes.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut size as *mut u64 as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 { size } else { 0 }
    }
    #[cfg(target_os = "linux")]
    {
        // /proc/meminfo: "MemTotal:    16327476 kB"
        let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") else {
            return 0;
        };
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:")
                && let Some(kb) = rest.split_whitespace().next()
                && let Ok(kb) = kb.parse::<u64>()
            {
                return kb * 1024;
            }
        }
        0
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        0
    }
}

pub struct NativeEmbedder {
    inner: Arc<Mutex<EmbedderInner>>,
    /// Max tokens per single chunk before the attention scratch would exceed the
    /// memory budget. Oversized chunks are truncated to this length (see
    /// `derive_token_cap` / `single_chunk_budget`). 0 means "no extra cap"
    /// (only the `MAX_SEQ_LEN` ceiling applies) — used in unit tests.
    ///
    /// Deliberately a plain field and not part of [`EmbedderInner`]: `inner` is
    /// held for a request's entire batch, and `token_cap()` is read by
    /// `/v1/health` on every liveness probe from a synchronous lock inside an
    /// `async fn`, which blocks a tokio worker rather than yielding it. Putting
    /// this back behind that mutex makes a liveness probe wait out a full
    /// forward-pass batch and takes the whole server down with it. Written once
    /// at load and never mutated, so it needs no synchronisation at all.
    token_cap: usize,
}

struct EmbedderInner {
    weights: Qwen3EmbedWeights,
    tokenizer: Tokenizer,
}

// ── no-KV-cache Qwen3 embedder (Q8_0 quantized) ───────────────────────────────

/// Qwen3 transformer weights loaded once; `forward_one` produces embeddings
/// with no KV cache and no generation state.  Each text gets a fresh causal
/// mask so no state leaks between texts in a batch.
///
/// Projection weights are Q8_0-quantized `QMatMul`; activations run in F32 (the
/// dtype candle's quantized matmul kernels consume on both CPU and Metal).
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
    q: QMatMul,
    k: QMatMul,
    v: QMatMul,
    o: QMatMul,
    q_norm: RmsNorm,
    k_norm: RmsNorm,
    gate: QMatMul,
    up: QMatMul,
    down: QMatMul,
    post_norm: RmsNorm,
}

impl Qwen3EmbedWeights {
    /// Build the model from the cached Q8_0 GGUF, placing every tensor on
    /// `device`. Q8_0 projection weights become `QMatMul`; the embedding table
    /// is dequantized to F16 for the gather; RMSNorm weights are F32.
    fn from_gguf(path: &Path, cfg: &Config, device: &Device) -> Result<Self> {
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("opening quantized GGUF {}", path.display()))?;
        let content = gguf_file::Content::read(&mut file)
            .with_context(|| format!("reading GGUF header {}", path.display()))?;

        // Token-embedding table: stored Q8_0, dequantized to F16 for the gather.
        let embed_w = read_qtensor(&content, &mut file, "model.embed_tokens.weight", device)?
            .dequantize_f16(device)
            .context("dequantizing embed_tokens")?;
        let embed_tokens = Embedding::new(embed_w, cfg.hidden_size);

        // Pre-compute RoPE sin/cos tables in F32 (activations run in F32).
        let half_hd = cfg.head_dim / 2;
        let inv_freq: Vec<f32> = (0..half_hd)
            .map(|i| 1.0f32 / (cfg.rope_theta as f32).powf(2.0 * i as f32 / cfg.head_dim as f32))
            .collect();
        let inv_freq_t = Tensor::from_vec(inv_freq, (1, half_hd), device)?;
        let t = Tensor::arange(0u32, MAX_SEQ_LEN as u32, device)?
            .to_dtype(DType::F32)?
            .reshape((MAX_SEQ_LEN, 1))?;
        let freqs = t.matmul(&inv_freq_t)?; // (MAX_SEQ_LEN, head_dim/2)
        let rope_cos = freqs.cos()?;
        let rope_sin = freqs.sin()?;

        let eps = cfg.rms_norm_eps;
        let mut layers = Vec::with_capacity(cfg.num_hidden_layers);
        for i in 0..cfg.num_hidden_layers {
            let p = format!("model.layers.{i}");
            layers.push(EmbedLayer {
                attn_norm: read_norm(
                    &content,
                    &mut file,
                    &format!("{p}.input_layernorm.weight"),
                    eps,
                    device,
                )?,
                q: read_qmm(
                    &content,
                    &mut file,
                    &format!("{p}.self_attn.q_proj.weight"),
                    device,
                )?,
                k: read_qmm(
                    &content,
                    &mut file,
                    &format!("{p}.self_attn.k_proj.weight"),
                    device,
                )?,
                v: read_qmm(
                    &content,
                    &mut file,
                    &format!("{p}.self_attn.v_proj.weight"),
                    device,
                )?,
                o: read_qmm(
                    &content,
                    &mut file,
                    &format!("{p}.self_attn.o_proj.weight"),
                    device,
                )?,
                q_norm: read_norm(
                    &content,
                    &mut file,
                    &format!("{p}.self_attn.q_norm.weight"),
                    eps,
                    device,
                )?,
                k_norm: read_norm(
                    &content,
                    &mut file,
                    &format!("{p}.self_attn.k_norm.weight"),
                    eps,
                    device,
                )?,
                gate: read_qmm(
                    &content,
                    &mut file,
                    &format!("{p}.mlp.gate_proj.weight"),
                    device,
                )?,
                up: read_qmm(
                    &content,
                    &mut file,
                    &format!("{p}.mlp.up_proj.weight"),
                    device,
                )?,
                down: read_qmm(
                    &content,
                    &mut file,
                    &format!("{p}.mlp.down_proj.weight"),
                    device,
                )?,
                post_norm: read_norm(
                    &content,
                    &mut file,
                    &format!("{p}.post_attention_layernorm.weight"),
                    eps,
                    device,
                )?,
            });
        }

        let final_norm = read_norm(&content, &mut file, "model.norm.weight", eps, device)?;

        Ok(Self {
            embed_tokens,
            layers,
            final_norm,
            rope_cos,
            rope_sin,
            n_head: cfg.num_attention_heads,
            n_kv_head: cfg.num_key_value_heads,
            head_dim: cfg.head_dim,
            device: device.clone(),
        })
    }

    /// Forward pass for one text; returns `[1, seq_len, hidden_size]`.
    fn forward_one(&self, ids: &[u32]) -> Result<Tensor> {
        let seq = ids.len();
        let ids_t = Tensor::new(ids, &self.device)?.unsqueeze(0)?; // [1, seq]
        // Embedding table is F16; promote to F32 for the (quantized) transformer.
        let mut h = self.embed_tokens.forward(&ids_t)?.to_dtype(DType::F32)?; // [1, seq, hidden]

        let mask = causal_mask(seq, h.dtype(), &self.device)?; // [1, 1, seq, seq]
        for layer in &self.layers {
            h = self.layer_fwd(&h, layer, &mask)?;
        }
        Ok(self.final_norm.forward(&h)?)
    }

    /// Padded batch forward pass: sequences right-padded to the longest in the
    /// batch. Returns one L2-normalised embedding per input, in order.
    ///
    /// CPU forwards the batch as a single BLAS call (batch dim > 1). Metal/GPU
    /// uses the sequential path instead: its buffer pool grows unboundedly with
    /// batched inference because `(b × n_head × seq²)` attention tensors are
    /// never compacted between passes → OOM.
    ///
    /// `cancel`, when set, is checked before each chunk on the sequential path
    /// (the only path where a per-iteration check is cheap and meaningful  -  the
    /// CPU batched path below is one indivisible BLAS call, so mid-call
    /// cancellation isn't worth the complexity; its floor is one sub-batch,
    /// bounded by the caller). On observing cancellation this bails with an
    /// error before doing the next chunk's forward pass  -  see
    /// GH#631.
    fn embed_batch(
        &self,
        batch_ids: &[&[u32]],
        cancel: Option<&AtomicBool>,
    ) -> Result<Vec<Vec<f32>>> {
        let b = batch_ids.len();
        assert!(b > 0);

        let max_seq = batch_ids.iter().map(|ids| ids.len()).max().unwrap_or(0);

        // Sequential path: single sequences, GPU/Metal (buffer pool grows
        // unboundedly with batching), or sequences past BATCH_MAX_SEQ (batched
        // attention tensor would OOM).
        if b == 1 || !matches!(self.device, Device::Cpu) || max_seq > BATCH_MAX_SEQ {
            let mut out = Vec::with_capacity(b);
            for ids in batch_ids {
                if let Some(flag) = cancel {
                    anyhow::ensure!(
                        !flag.load(Ordering::Relaxed),
                        "embed cancelled mid sub-batch (sequential path)"
                    );
                }
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

        // Build flat right-padded buffer [B × max_seq] with pad id = 0.
        let mut flat: Vec<u32> = Vec::with_capacity(b * max_seq);
        for ids in batch_ids {
            flat.extend_from_slice(ids);
            flat.extend(std::iter::repeat_n(0u32, max_seq - ids.len()));
        }
        let ids_t = Tensor::from_slice(&flat, (b, max_seq), &self.device)?;

        // Forward pass: all layers work naturally with batch dim > 1.
        // Embedding table is F16; promote to F32 for the (quantized) transformer.
        let mut h = self.embed_tokens.forward(&ids_t)?.to_dtype(DType::F32)?; // [b, max_seq, hidden]
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
        let q = q
            .reshape((b, seq, self.n_head, self.head_dim))?
            .transpose(1, 2)?;
        let k = k
            .reshape((b, seq, self.n_kv_head, self.head_dim))?
            .transpose(1, 2)?;
        let v = v
            .reshape((b, seq, self.n_kv_head, self.head_dim))?
            .transpose(1, 2)?;

        // QK RMSNorm (Qwen3 adds a per-head norm before RoPE)
        let q = layer.q_norm.forward(&q)?;
        let k = layer.k_norm.forward(&k)?;

        // RoPE
        let cos = self.rope_cos.narrow(0, 0, seq)?;
        let sin = self.rope_sin.narrow(0, 0, seq)?;
        let q = rope(&q.contiguous()?, &cos, &sin)?;
        let k = rope(&k.contiguous()?, &cos, &sin)?;

        // Grouped query attention: expand K, V from n_kv_heads to n_heads.
        // MUST repeat-interleave (each kv head duplicated n_rep times
        // contiguously, [kv0,kv0,kv1,kv1,…]) so query head j attends through kv
        // head j/n_rep. `Tensor::repeat` instead *tiles* ([kv0,…,kvN,kv0,…,kvN]),
        // silently pairing most query heads with the wrong K/V. `repeat_kv`
        // returns the correct interleaved order, contiguous.
        let n_rep = self.n_head / self.n_kv_head;
        let k = repeat_kv(k, n_rep)?;
        let v = repeat_kv(v, n_rep)?;

        // Scaled dot-product attention + causal mask
        let scale = (self.head_dim as f64).powf(-0.5);
        let scores = (q.matmul(&k.transpose(2, 3)?)? * scale)?;
        let scores = scores.broadcast_add(mask)?;
        let probs = candle_nn::ops::softmax_last_dim(&scores)?;
        let out = probs.matmul(&v)?; // [b, n_head, seq, hd]

        // Merge heads: [b, seq, n_head*hd]
        let out =
            out.transpose(1, 2)?
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

/// Read one tensor from the open GGUF onto `device`.
fn read_qtensor(
    content: &gguf_file::Content,
    file: &mut std::fs::File,
    name: &str,
    device: &Device,
) -> Result<QTensor> {
    content
        .tensor(file, name, device)
        .with_context(|| format!("reading tensor {name} from GGUF"))
}

/// Read an F32 RMSNorm weight from the GGUF and wrap it in an `RmsNorm`.
fn read_norm(
    content: &gguf_file::Content,
    file: &mut std::fs::File,
    name: &str,
    eps: f64,
    device: &Device,
) -> Result<RmsNorm> {
    let w = read_qtensor(content, file, name, device)?
        .dequantize(device)
        .with_context(|| format!("dequantizing norm {name}"))?;
    Ok(RmsNorm::new(w, eps))
}

/// Read a Q8_0 projection weight from the GGUF as a quantized matmul.
fn read_qmm(
    content: &gguf_file::Content,
    file: &mut std::fs::File,
    name: &str,
    device: &Device,
) -> Result<QMatMul> {
    let qt = read_qtensor(content, file, name, device)?;
    QMatMul::from_qtensor(qt).with_context(|| format!("building QMatMul for {name}"))
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
    /// Load the F2LLM-v2-330M embedder from local files already on disk, with
    /// zero network access — this crate carries no download dependency
    /// (`spelunk-server` resolves the artifacts via Hugging Face Hub first).
    ///
    /// All three must be present locally and from the same model revision so the
    /// weight keys and tensor shapes line up: `gguf_path` (Q8_0 GGUF),
    /// `tokenizer_path` (`tokenizer.json`), `config_path` (Qwen3 `config.json`).
    /// Uses Metal/GPU on macOS with the `metal` feature, CPU otherwise.
    pub fn load_from_path(
        gguf_path: &Path,
        tokenizer_path: &Path,
        config_path: &Path,
    ) -> Result<Self> {
        anyhow::ensure!(
            gguf_path.exists(),
            "GGUF file not found: {}",
            gguf_path.display()
        );

        let device = select_device();
        let on_gpu = !matches!(device, Device::Cpu);
        tracing::info!(
            "loading F2LLM-v2-330M (Q8_0) from local path via candle on {} ({})",
            if on_gpu { "Metal/GPU" } else { "CPU" },
            gguf_path.display()
        );

        let tokenizer = Tokenizer::from_file(tokenizer_path).map_err(|e| {
            anyhow::anyhow!("loading tokenizer from {}: {e}", tokenizer_path.display())
        })?;

        let config: Config = serde_json::from_str(
            &std::fs::read_to_string(config_path)
                .with_context(|| format!("reading config.json {}", config_path.display()))?,
        )
        .with_context(|| format!("parsing config.json {}", config_path.display()))?;

        Self::from_files(gguf_path, tokenizer, config, device)
    }

    /// Shared final step for both load paths: build the quantized weights from an
    /// on-disk GGUF, derive the single-chunk token cap from system RAM, and wrap
    /// everything in the `NativeEmbedder`. Performs no network access.
    fn from_files(
        gguf_path: &Path,
        tokenizer: Tokenizer,
        config: Config,
        device: Device,
    ) -> Result<Self> {
        let on_gpu = !matches!(device, Device::Cpu);

        // The single-chunk token cap derivation assumes `N_HEAD` attention heads
        // (the attention scratch is `[1, n_head, seq, seq]`). Guard against a
        // future model whose config no longer matches the compiled-in constant.
        debug_assert_eq!(
            config.num_attention_heads, N_HEAD,
            "N_HEAD constant ({N_HEAD}) must match model config.json \
             num_attention_heads ({}) — token-cap derivation depends on it",
            config.num_attention_heads
        );
        let n_head = config.num_attention_heads;

        let weights = Qwen3EmbedWeights::from_gguf(gguf_path, &config, &device)
            .context("loading quantized F2LLM-v2-330M weights")?;

        // Pick the memory budget from total system RAM, then derive the
        // single-chunk token cap that keeps the attention scratch within it.
        let total_ram = total_system_ram();
        let budget = single_chunk_budget(total_ram);
        let token_cap = derive_token_cap(budget, n_head);

        tracing::info!(
            "F2LLM-v2-330M ready (dim={DIM}, Q8_0, device={}); \
             system RAM {:.1} GiB → single-chunk budget {} GiB, token cap {token_cap}",
            if on_gpu { "Metal/GPU" } else { "CPU" },
            total_ram as f64 / (1024.0 * 1024.0 * 1024.0),
            budget / (1024 * 1024 * 1024),
        );

        Ok(Self {
            inner: Arc::new(Mutex::new(EmbedderInner { weights, tokenizer })),
            token_cap,
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

fn l2_normalise(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

#[async_trait::async_trait]
impl crate::EmbeddingBackend for NativeEmbedder {
    /// Embed a batch of strings using F2LLM-v2-330M (Q8_0), with no way to
    /// cancel early. Delegates to [`Self::embed_with_cancel`] with a flag
    /// that's never set, so there is exactly one implementation of the
    /// tokenize/sub-batch logic.
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.embed_with_cancel(texts, Arc::new(AtomicBool::new(false)))
            .await
    }

    /// Embed a batch of strings using F2LLM-v2-330M (Q8_0), stopping early if
    /// `cancel` is observed set.
    ///
    /// Texts are tokenized, sorted by token length (to minimise padding waste),
    /// then forwarded through the Qwen3 decoder in padded sub-batches of
    /// `EMBED_BATCH_SIZE`.  Each sub-batch is one BLAS call with batch dim > 1,
    /// which amortises the per-call overhead.  The last token's hidden state is
    /// L2-normalised to produce a 896-dim embedding; results are returned in the
    /// original input order.
    ///
    /// `cancel` is checked in three places (see GH#631): once
    /// immediately after acquiring the embedder lock (a batch abandoned while
    /// queued behind another does zero forward passes  -  this is the cascade
    /// killer for the mutex-serialized embedder), once between each sub-batch,
    /// and once per chunk inside the sequential path of `embed_batch` (bounds
    /// waste to ~one chunk on Metal, where every chunk is sequential). The one
    /// place it is deliberately NOT checked is mid-BLAS-call in the CPU batched
    /// path  -  not worth the complexity; its floor is one `EMBED_BATCH_SIZE`
    /// sub-batch.
    async fn embed_with_cancel(
        &self,
        texts: &[&str],
        cancel: Arc<AtomicBool>,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        let owned: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        let inner = Arc::clone(&self.inner);
        let effective_cap = effective_token_cap(self.token_cap);

        tokio::task::spawn_blocking(move || {
            let guard = inner
                .lock()
                .map_err(|_| anyhow::anyhow!("native embedder lock poisoned"))?;

            let total = owned.len();
            if cancel.load(Ordering::Relaxed) {
                tracing::info!(
                    "embed batch abandoned before starting (0/{total} chunks completed)  -  \
                     client disconnected or server timed out while queued"
                );
                return Err(EmbedError::Cancelled {
                    completed: 0,
                    total,
                }
                .into());
            }

            // 1. Tokenize all texts upfront, capping each to `effective_cap`.
            // `token_cap` is the memory-budget-derived bound that keeps the
            // single-chunk attention scratch (`[1, n_head, seq, seq]` f32) within
            // RAM; a ~40 k-token chunk would otherwise allocate ~100 GB and OOM
            // the whole index, so truncate (preserving the leading signal).
            // (token_cap == 0 in unit-test fixtures means "no extra cap".)
            let mut id_vecs: Vec<Vec<u32>> = Vec::with_capacity(owned.len());
            for text in &owned {
                let encoding = guard
                    .tokenizer
                    .encode(text.as_str(), true) // add_special_tokens=true → appends EOS
                    .map_err(|e| EmbedError::Tokenization(e.to_string()))?;
                let full_len = encoding.get_ids().len();
                let ids: Vec<u32> = encoding
                    .get_ids()
                    .iter()
                    .take(effective_cap)
                    .copied()
                    .collect();
                if full_len > effective_cap {
                    tracing::warn!(
                        "chunk truncated for embedding: {full_len} tokens > cap {effective_cap} \
                         (memory-budget limit) — embedding leading {effective_cap} tokens only"
                    );
                }
                anyhow::ensure!(!ids.is_empty(), "empty token sequence after tokenization");
                id_vecs.push(ids);
            }

            // 2. Sort by token length so sequences in the same sub-batch have
            //    similar lengths, minimising padding waste.
            let mut indexed: Vec<(usize, Vec<u32>)> = id_vecs.into_iter().enumerate().collect();
            indexed.sort_unstable_by_key(|(_, ids)| ids.len());

            // 3. Process in sub-batches; reassemble into original order.
            let mut results: Vec<Vec<f32>> = vec![Vec::new(); owned.len()];
            let mut completed = 0usize;
            for sub_batch in indexed.chunks(EMBED_BATCH_SIZE) {
                let sub_batch_started = std::time::Instant::now();
                if cancel.load(Ordering::Relaxed) {
                    tracing::info!(
                        "embed batch abandoned between sub-batches \
                         ({completed}/{total} chunks completed)  -  client disconnected or \
                         server timed out"
                    );
                    return Err(EmbedError::Cancelled { completed, total }.into());
                }
                let batch_ids: Vec<&[u32]> =
                    sub_batch.iter().map(|(_, ids)| ids.as_slice()).collect();
                let vecs = guard
                    .weights
                    .embed_batch(&batch_ids, Some(&cancel))
                    .map_err(|e| {
                        if cancel.load(Ordering::Relaxed) {
                            tracing::info!(
                                "embed batch abandoned mid sub-batch \
                             ({completed}/{total} chunks completed, current sub-batch of \
                             {} interrupted)  -  client disconnected or server timed out",
                                sub_batch.len()
                            );
                            EmbedError::Cancelled { completed, total }
                        } else {
                            EmbedError::Inference(e.to_string())
                        }
                    })?;
                for ((orig_idx, _), vec) in sub_batch.iter().zip(vecs) {
                    results[*orig_idx] = vec;
                }
                completed += sub_batch.len();
                // Pure observability: a trail for diagnosing a wedged vs.
                // steadily-progressing embed from the server side, without
                // relying entirely on the client's post-hoc symptoms.
                tracing::debug!(
                    "embed sub-batch of {} chunk(s) done in {:?} ({completed}/{total} total)",
                    sub_batch.len(),
                    sub_batch_started.elapsed(),
                );
            }

            Ok(results)
        })
        .await
        .context("spawn_blocking panicked in native embedder")?
    }

    fn dimension(&self) -> usize {
        DIM
    }

    /// The host-derived per-chunk truncation cap (see `derive_token_cap`),
    /// surfaced so `/v1/health` can advertise it and a client can size a
    /// batch's total token budget realistically. `None` when running under
    /// the `token_cap == 0` test fixture ("no extra cap").
    fn token_cap(&self) -> Option<usize> {
        if self.token_cap == 0 {
            None
        } else {
            Some(self.token_cap)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `[1, n_kv, seq, head_dim]` tensor where every element of kv-head
    /// `k` equals `k` — so the head index is recoverable from any of its values.
    fn headed_kv(n_kv: usize, seq: usize, head_dim: usize) -> Tensor {
        let data: Vec<f32> = (0..n_kv)
            .flat_map(|k| vec![k as f32; seq * head_dim])
            .collect();
        Tensor::from_vec(data, (1, n_kv, seq, head_dim), &Device::Cpu).unwrap()
    }

    /// The source kv-head stamped on each output head, in order.
    fn head_sources(t: &Tensor) -> Vec<usize> {
        let (_, n_head, seq, head_dim) = t.dims4().unwrap();
        let flat: Vec<f32> = t.flatten_all().unwrap().to_vec1().unwrap();
        let per_head = seq * head_dim;
        (0..n_head).map(|h| flat[h * per_head] as usize).collect()
    }

    /// Grouped-query-attention K/V expansion must repeat-interleave the kv-head
    /// dim (`[kv0,kv0,kv1,kv1,…]`) so query head `j` attends through kv head
    /// `j / n_rep`. `Tensor::repeat` tiles instead (`[kv0..kvN, kv0..kvN]`),
    /// silently pairing 15 of 16 query heads with the wrong K/V. Guards the
    /// `repeat_kv` call in `Qwen3EmbedWeights::attn`.
    #[test]
    fn repeat_kv_interleaves_gqa_heads_not_tiles() {
        // F2LLM-v2-330M: 16 attention heads / 8 kv heads → n_rep = 2.
        let (n_kv, n_rep, seq, head_dim) = (8usize, 2usize, 3usize, 4usize);
        let kv = headed_kv(n_kv, seq, head_dim);

        // Production path.
        let expanded = repeat_kv(kv.clone(), n_rep).unwrap();
        assert_eq!(expanded.dims4().unwrap(), (1, n_kv * n_rep, seq, head_dim));

        // Correct GQA ordering: each kv head duplicated n_rep times, contiguously.
        let expected: Vec<usize> = (0..n_kv * n_rep).map(|h| h / n_rep).collect();
        assert_eq!(
            head_sources(&expanded),
            expected,
            "repeat_kv must repeat-interleave kv heads (spelunk-oss#19)"
        );

        // Tiling bug: `Tensor::repeat` → [kv0..kv7, kv0..kv7].
        let tiled = kv.repeat(&[1, n_rep, 1, 1]).unwrap();
        let tiled_order: Vec<usize> = (0..n_kv).chain(0..n_kv).collect();
        assert_eq!(head_sources(&tiled), tiled_order);
        assert_ne!(
            head_sources(&expanded),
            head_sources(&tiled),
            "interleave (repeat_kv) and tile (Tensor::repeat) must differ for n_rep > 1 — \
             reverting to Tensor::repeat reintroduces spelunk-oss#19"
        );
    }

    /// `repeat_kv` must return a **contiguous** tensor: candle's CPU matmul rejects
    /// a non-contiguous rhs (`MatMulUnexpectedStriding`), which previously crashed
    /// the embedder on the CPU backend. Guards against an expansion that leaves the
    /// K/V views strided going into the attention matmuls.
    #[test]
    fn repeat_kv_output_is_contiguous() {
        let kv = headed_kv(8, 3, 4);
        let expanded = repeat_kv(kv, 2).unwrap();
        assert!(
            expanded.is_contiguous(),
            "repeat_kv output must be contiguous for candle's CPU matmul"
        );
    }

    // ── single-chunk memory-budget cap ────────────────────────────────────────

    /// Budget selection keys off total system RAM: 2 GiB at/below the 16 GiB
    /// threshold, 4 GiB above it.
    #[test]
    fn budget_selected_by_system_ram() {
        let gib = 1024u64 * 1024 * 1024;
        // Small hosts (and the unknown/0 fallback) get the conservative 2 GiB.
        assert_eq!(single_chunk_budget(0), SINGLE_CHUNK_BUDGET_SMALL);
        assert_eq!(single_chunk_budget(8 * gib), SINGLE_CHUNK_BUDGET_SMALL);
        assert_eq!(single_chunk_budget(16 * gib), SINGLE_CHUNK_BUDGET_SMALL);
        // Strictly above 16 GiB unlocks the 4 GiB budget.
        assert_eq!(single_chunk_budget(16 * gib + 1), SINGLE_CHUNK_BUDGET_LARGE);
        assert_eq!(single_chunk_budget(32 * gib), SINGLE_CHUNK_BUDGET_LARGE);
    }

    /// The token cap is derived from `n_head × seq² × 4 ≤ budget`. Pin the two
    /// production budgets to their derived caps (~5 792 @ 2 GiB, ~8 192 @ 4 GiB)
    /// and assert the inverse: the resulting attention scratch stays within
    /// budget while one more token would exceed it.
    #[test]
    fn token_cap_derivation_matches_budget() {
        let cap_2g = derive_token_cap(SINGLE_CHUNK_BUDGET_SMALL, N_HEAD);
        let cap_4g = derive_token_cap(SINGLE_CHUNK_BUDGET_LARGE, N_HEAD);

        // Anchored ballpark from the decision (2 GiB ≈ 5.8 k, 4 GiB ≈ 8.2 k).
        assert_eq!(cap_2g, 5792, "2 GiB cap");
        assert_eq!(cap_4g, 8192, "4 GiB cap");

        // The cap is the largest seq whose scratch fits; cap+1 must not.
        let scratch = |seq: usize| (N_HEAD as u64) * (seq as u64) * (seq as u64) * 4;
        assert!(scratch(cap_2g) <= SINGLE_CHUNK_BUDGET_SMALL);
        assert!(scratch(cap_2g + 1) > SINGLE_CHUNK_BUDGET_SMALL);
        assert!(scratch(cap_4g) <= SINGLE_CHUNK_BUDGET_LARGE);
        assert!(scratch(cap_4g + 1) > SINGLE_CHUNK_BUDGET_LARGE);

        // Larger budget ⇒ larger (or equal) cap.
        assert!(cap_4g > cap_2g);
    }

    /// A 40 k-token chunk uncapped would allocate ~100 GiB of attention scratch;
    /// the cap brings it within budget.
    #[test]
    fn oversized_chunk_scratch_drops_below_budget() {
        let scratch = |seq: usize| (N_HEAD as u64) * (seq as u64) * (seq as u64) * 4;

        // Before: a full 40 960-token chunk (MAX_SEQ_LEN) → ~100 GiB scratch.
        let before = scratch(MAX_SEQ_LEN);
        assert!(
            before > 90 * 1024 * 1024 * 1024,
            "full-length chunk scratch should be ~100 GiB (was {before} bytes)"
        );

        // After: truncated to the 2 GiB cap → scratch within budget.
        let cap = derive_token_cap(SINGLE_CHUNK_BUDGET_SMALL, N_HEAD);
        assert!(scratch(cap) <= SINGLE_CHUNK_BUDGET_SMALL);
    }

    /// The cap is never larger than the model's position-embedding ceiling, and
    /// degenerate budgets still yield a usable (>= 1) cap.
    #[test]
    fn token_cap_is_clamped() {
        // A huge budget can't exceed MAX_SEQ_LEN.
        assert_eq!(derive_token_cap(u64::MAX, N_HEAD), MAX_SEQ_LEN);
        // A tiny budget still yields at least one token (never zero/panics).
        assert_eq!(derive_token_cap(0, N_HEAD), 1);
        assert!(derive_token_cap(1024, N_HEAD) >= 1);
    }

    /// Whatever RAM this host actually has, the detected budget must derive a
    /// cap strictly smaller than MAX_SEQ_LEN — i.e. the 40 k-token OOM path is
    /// closed on every supported budget.
    #[test]
    fn detected_budget_caps_below_max_seq_len() {
        for ram in [0u64, 8, 16, 17, 64].map(|g| g * 1024 * 1024 * 1024) {
            let cap = derive_token_cap(single_chunk_budget(ram), N_HEAD);
            assert!(
                cap < MAX_SEQ_LEN,
                "cap {cap} for {ram}-byte host must be below MAX_SEQ_LEN {MAX_SEQ_LEN}"
            );
        }
    }

    // ── L2 normalisation invariant ────────────────────────────────────────────
    //
    // Contract: "896-dim, L2-normalised". These pin the normalisation step
    // without needing the model on disk; the end-to-end proof runs through the
    // ignored network tests in spelunk-server's `embed_hub`.

    /// A non-zero vector must come out with unit L2 norm and preserved direction
    /// (each component scaled by the same factor).
    #[test]
    fn l2_normalise_yields_unit_norm_and_preserves_direction() {
        let original = [3.0f32, 0.0, 4.0]; // norm 5
        let mut v = original;
        l2_normalise(&mut v);

        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-6,
            "normalised vector must have unit L2 norm, got {norm}"
        );
        // Direction preserved: every component is the original divided by 5.
        for (out, orig) in v.iter().zip(original) {
            assert!((out - orig / 5.0).abs() < 1e-6);
        }
    }

    /// The zero vector must be left untouched, not divide-by-zero into NaNs that
    /// would poison the int8 index.
    #[test]
    fn l2_normalise_leaves_zero_vector_finite() {
        let mut v = [0.0f32; 4];
        l2_normalise(&mut v);
        assert!(
            v.iter().all(|x| x.is_finite() && *x == 0.0),
            "zero vector must stay all-zero and finite (no divide-by-zero NaNs)"
        );
    }

    /// Pin the public `DIM` constant (896, F2LLM-v2-330M hidden size) so an
    /// accidental change to the exported contract fails a cheap offline test.
    #[test]
    fn dim_is_f2llm_hidden_size() {
        assert_eq!(
            DIM, 896,
            "public embedding dimension must stay 896 (F2LLM-v2-330M)"
        );
    }

    /// `load_from_path` must do no network access: with a missing GGUF it fails
    /// fast on the local-file check rather than reaching out to the Hub.
    #[test]
    fn load_from_path_missing_gguf_errors_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let gguf = dir.path().join("absent.gguf");
        let tokenizer = dir.path().join("tokenizer.json");
        let config = dir.path().join("config.json");

        let msg = match NativeEmbedder::load_from_path(&gguf, &tokenizer, &config) {
            Ok(_) => panic!("missing GGUF must be a load error"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            msg.contains("GGUF file not found"),
            "load_from_path must fail on the local-file check (no network), got: {msg}"
        );
    }

    /// Stronger offline guarantee: even when the GGUF exists (existence check
    /// passes), a missing tokenizer must fail on the local read, never a Hub
    /// download — proving the code past the existence guard stays on disk.
    #[test]
    fn load_from_path_present_gguf_missing_tokenizer_errors_locally() {
        let dir = tempfile::tempdir().unwrap();
        // A present (empty) GGUF: the `exists()` guard passes, so control reaches
        // the tokenizer/config loading that a Hub-fallback bug would live in.
        let gguf = dir.path().join("present.gguf");
        std::fs::write(&gguf, b"").unwrap();
        let tokenizer = dir.path().join("tokenizer.json"); // absent
        let config = dir.path().join("config.json"); // absent

        let msg = match NativeEmbedder::load_from_path(&gguf, &tokenizer, &config) {
            Ok(_) => panic!("an empty GGUF with no tokenizer must be a load error"),
            Err(e) => format!("{e:#}"),
        };
        // The error must name the local tokenizer path, never a Hub URL/download.
        assert!(
            msg.contains("loading tokenizer from")
                && msg.contains(&tokenizer.display().to_string()),
            "load_from_path must fail on the local tokenizer read (no network), got: {msg}"
        );
        assert!(
            !msg.contains("http") && !msg.contains("huggingface") && !msg.contains("downloading"),
            "load_from_path must not reference any network fetch, got: {msg}"
        );
    }

    // End-to-end "load a real GGUF/tokenizer/config and embed" tests (including
    // the `token_cap()` proof) live in spelunk-server's `embed_hub`, which can
    // acquire the model artifacts this crate can't.

    /// The `token_cap()` derivation (`derive_token_cap`/`single_chunk_budget`)
    /// must be stable and non-degenerate for the host's budget. Pure-math check
    /// against the private helpers; the live proof against a loaded embedder is
    /// `embed_hub::tests::native_embedder_reports_its_token_cap`.
    #[test]
    fn token_cap_matches_derive_token_cap_for_host_budget() {
        let budget = single_chunk_budget(total_system_ram());
        let expected_cap = derive_token_cap(budget, N_HEAD);

        // `token_cap` is set from this derivation at load time (see
        // `from_files`); assert the derivation is stable rather than loading a model.
        assert!(expected_cap >= 1, "derived cap must be usable");
        assert!(
            expected_cap <= MAX_SEQ_LEN,
            "derived cap must not exceed the model's position-embedding ceiling"
        );
    }

    // ── token_cap must not depend on the forward-pass mutex ───────────────────
    //
    // `/v1/health` reads `token_cap()` on every liveness probe. The forward-pass
    // mutex is held for a request's entire batch (up to 32 sequential passes),
    // and it is taken synchronously from an `async fn`, so a probe that waits on
    // it blocks a tokio worker rather than yielding it. These tests build an
    // embedder with a dummy inner (no model, no forward pass ever run) purely so
    // the accessor can be exercised against a genuinely held lock.
    //
    // These are the only tests in the workspace that fail if the accessor goes
    // back behind that mutex. The server-side liveness suite runs against a
    // mock backend and stays green through such a change, so do not weaken or
    // delete these on the assumption that something downstream is watching.
    // `embedder_with_cap` constructs `NativeEmbedder` by struct literal on
    // purpose: moving `token_cap` back into `EmbedderInner` breaks this file at
    // compile time rather than silently.

    fn dummy_weights() -> Qwen3EmbedWeights {
        let device = Device::Cpu;
        Qwen3EmbedWeights {
            embed_tokens: Embedding::new(
                Tensor::zeros((2, 2), DType::F16, &device).expect("embed table"),
                2,
            ),
            layers: Vec::new(),
            final_norm: RmsNorm::new(Tensor::ones(2, DType::F32, &device).expect("norm"), 1e-6),
            rope_cos: Tensor::zeros((2, 1), DType::F32, &device).expect("rope cos"),
            rope_sin: Tensor::zeros((2, 1), DType::F32, &device).expect("rope sin"),
            n_head: N_HEAD,
            n_kv_head: 8,
            head_dim: 64,
            device,
        }
    }

    fn embedder_with_cap(token_cap: usize) -> NativeEmbedder {
        NativeEmbedder {
            inner: Arc::new(Mutex::new(EmbedderInner {
                weights: dummy_weights(),
                tokenizer: Tokenizer::new(tokenizers::models::bpe::BPE::default()),
            })),
            token_cap,
        }
    }

    // Hold the forward-pass mutex on a separate thread until the returned
    // sender is used, exactly as an in-flight embed batch does.
    fn hold_forward_pass_lock(
        embedder: &Arc<NativeEmbedder>,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let holder = Arc::clone(embedder);
        let (taken_tx, taken_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let _guard = holder.inner.lock().expect("forward-pass mutex");
            taken_tx.send(()).expect("signal that the lock is held");
            let _ = release_rx.recv();
        });
        taken_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("holder thread must acquire the forward-pass mutex");
        (release_tx, handle)
    }

    // Read the cap on its own thread so a re-coupled accessor fails the bound
    // instead of hanging the whole test suite.
    fn token_cap_within(
        embedder: &Arc<NativeEmbedder>,
        bound: std::time::Duration,
    ) -> Option<usize> {
        use crate::EmbeddingBackend;

        let reader = Arc::clone(embedder);
        let (cap_tx, cap_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = cap_tx.send(reader.token_cap());
        });
        cap_rx.recv_timeout(bound).unwrap_or_else(|_| {
            panic!(
                "token_cap() did not return within {bound:?} while the forward-pass mutex was \
                 held: the accessor is back behind the embed lock"
            )
        })
    }

    #[test]
    fn token_cap_returns_while_the_forward_pass_lock_is_held() {
        let embedder = Arc::new(embedder_with_cap(5792));
        let (release, holder) = hold_forward_pass_lock(&embedder);

        let cap = token_cap_within(&embedder, std::time::Duration::from_millis(250));
        assert_eq!(
            cap,
            Some(5792),
            "the advertised cap must be readable while a batch holds the forward-pass mutex"
        );

        release.send(()).expect("release the holder thread");
        holder.join().expect("holder thread");
    }

    #[test]
    fn token_cap_is_independent_of_embedder_busyness() {
        use crate::EmbeddingBackend;

        let embedder = Arc::new(embedder_with_cap(8192));
        let at_rest = embedder.token_cap();

        let (release, holder) = hold_forward_pass_lock(&embedder);
        let while_busy = token_cap_within(&embedder, std::time::Duration::from_millis(250));
        release.send(()).expect("release the holder thread");
        holder.join().expect("holder thread");

        assert_eq!(at_rest, Some(8192), "cap at rest");
        assert_eq!(
            while_busy, at_rest,
            "the reported cap must not change with embedder busyness"
        );
    }

    #[test]
    fn zero_cap_fixture_reports_none_and_a_real_cap_reports_some() {
        use crate::EmbeddingBackend;

        assert_eq!(
            embedder_with_cap(0).token_cap(),
            None,
            "the 0 fixture means 'no extra cap' and must stay `None`"
        );
        assert_eq!(
            embedder_with_cap(5792).token_cap(),
            Some(5792),
            "a real host-derived cap must be reported verbatim"
        );
    }

    #[test]
    fn effective_cap_is_the_smaller_of_the_token_cap_and_max_seq_len() {
        assert_eq!(
            effective_token_cap(0),
            MAX_SEQ_LEN,
            "0 means 'no extra cap': only the MAX_SEQ_LEN ceiling applies"
        );
        assert_eq!(
            effective_token_cap(1),
            1,
            "the smallest cap `derive_token_cap` can produce must survive verbatim, not be \
             rounded up or treated like the 0 sentinel"
        );
        assert_eq!(
            effective_token_cap(5792),
            5792,
            "a cap below the ceiling wins"
        );
        assert_eq!(
            effective_token_cap(MAX_SEQ_LEN),
            MAX_SEQ_LEN,
            "a cap exactly at the ceiling is not off-by-one clamped below it"
        );
        assert_eq!(
            effective_token_cap(MAX_SEQ_LEN - 1),
            MAX_SEQ_LEN - 1,
            "one below the ceiling is still the cap, not the ceiling"
        );
        assert_eq!(
            effective_token_cap(MAX_SEQ_LEN + 1),
            MAX_SEQ_LEN,
            "a cap above the ceiling must be clamped to MAX_SEQ_LEN"
        );
        assert_eq!(
            effective_token_cap(usize::MAX),
            MAX_SEQ_LEN,
            "no cap value can push the truncation length past the model's position-embedding \
             ceiling"
        );
    }

    #[test]
    fn host_derived_cap_survives_the_move_off_the_forward_pass_mutex() {
        use crate::EmbeddingBackend;

        let derived = derive_token_cap(single_chunk_budget(total_system_ram()), N_HEAD);
        let embedder = embedder_with_cap(derived);

        assert_eq!(
            embedder.token_cap(),
            Some(derived),
            "the accessor must surface exactly the cap derived at load time"
        );
        assert_eq!(
            effective_token_cap(derived),
            derived.min(MAX_SEQ_LEN),
            "truncation must still use min(token_cap, MAX_SEQ_LEN)"
        );
    }
}
