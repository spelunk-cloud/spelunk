use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::qwen3::{Config as Qwen3Config, Model as Qwen3Model};
use hf_hub::{Repo, RepoType, api::sync::ApiBuilder};
use tokenizers::Tokenizer;

/// Embedding dimension produced by F2LLM-v2-330M (hidden_size = 896).
pub const DIM: usize = 896;

const MODEL_ID: &str = "codefuse-ai/F2LLM-v2-330M";
/// Hard ceiling for token sequences (max_position_embeddings).
const MAX_SEQ_LEN: usize = 40960;

pub struct NativeEmbedder {
    inner: Arc<Mutex<EmbedderInner>>,
}

struct EmbedderInner {
    // Weights stored as a cloneable VarBuilder; a fresh Qwen3Model is created per embed
    // text so that the KV cache inside each model is always empty. Model::clear_kv_cache
    // is pub(crate) in candle 0.10.2 and not callable here; this avoids the issue.
    vb: VarBuilder<'static>,
    config: Qwen3Config,
    tokenizer: Tokenizer,
    device: Device,
}

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
        // "model.embed_tokens.weight"). candle's Qwen3Model::new expects the prefixed form,
        // so we add it here while loading.
        let vb = load_weights_prefixed(&weight_paths, dtype, &device)
            .context("loading F2LLM-v2-330M weights")?;

        // Smoke-test: instantiate the model once to validate config/weights.
        Qwen3Model::new(&config, vb.clone()).context("building F2LLM-v2-330M model")?;

        tracing::info!(
            "F2LLM-v2-330M ready (dim={DIM}, device={})",
            if on_gpu { "Metal/GPU" } else { "CPU" }
        );

        Ok(Self {
            inner: Arc::new(Mutex::new(EmbedderInner {
                vb,
                config,
                tokenizer,
                device,
            })),
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
/// candle's Qwen3Model::new (which calls vb.pp("model.embed_tokens") etc.) can
/// find them. F2LLM stores keys without this prefix because it is saved as a
/// plain Qwen3Model rather than a Qwen3ForCausalLM wrapper.
fn load_weights_prefixed(
    paths: &[PathBuf],
    dtype: DType,
    device: &Device,
) -> Result<VarBuilder<'static>> {
    use std::collections::HashMap;
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
    /// Each string is tokenized with `add_special_tokens=true` (appends EOS),
    /// forwarded through the Qwen3 decoder, and the last token's hidden state
    /// is L2-normalised to produce a 896-dim embedding vector.
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        let owned: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        let inner = Arc::clone(&self.inner);

        tokio::task::spawn_blocking(move || {
            let guard = inner
                .lock()
                .map_err(|_| anyhow::anyhow!("native embedder lock poisoned"))?;

            let mut results = Vec::with_capacity(owned.len());

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
                let seq_len = ids.len();
                anyhow::ensure!(seq_len > 0, "empty token sequence after tokenization");

                // [1, seq_len]
                let input = Tensor::new(ids.as_slice(), &guard.device)?.unsqueeze(0)?;

                // A fresh model is created per text so the internal KV cache starts empty.
                // Model::clear_kv_cache is pub(crate) in candle 0.10.2; recreating from the
                // cloned VarBuilder is the safe alternative. Weight Tensors are Arc'd so the
                // clone is cheap — only the KV cache allocations and RoPE sin/cos table are
                // new per call.
                let mut model = Qwen3Model::new(&guard.config, guard.vb.clone())
                    .map_err(|e| anyhow::anyhow!("model instantiation failed: {e}"))?;

                // [1, seq_len, 896]
                let hidden = model.forward(&input, 0)?;

                // Last token (EOS) hidden state → [896]
                let last = hidden.i((0, seq_len - 1))?;
                let mut vec: Vec<f32> = last.to_dtype(DType::F32)?.to_vec1()?;
                l2_normalise(&mut vec);
                results.push(vec);
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
