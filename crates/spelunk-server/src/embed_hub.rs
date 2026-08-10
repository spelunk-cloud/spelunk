//! Hugging Face Hub acquisition path for the bundled F2LLM-v2-330M embedder.
//!
//! `spelunk-embed` only knows how to load the embedder from files already on
//! disk ([`spelunk_embed::NativeEmbedder::load_from_path`]) — it carries no
//! network-fetch dependency. This module owns the `hf-hub` download step: it
//! fetches the pre-quantized GGUF and tokenizer from our own first-party
//! Hugging Face repo into the local hf-hub cache (writing the embedded
//! `config.json` alongside them), then hands the resulting file paths to
//! `load_from_path`. This is the only place in `spelunk-server` — or the
//! workspace — that depends on `hf-hub`.
//!
//! [`load_from_model_dir`] is the air-gapped counterpart: it resolves the
//! same artifacts from an operator-provisioned directory instead of the Hub,
//! with no `hf_hub` involvement at all (see "Air-gapped / no-egress install"
//! in `docs/server-setup.md`).
//!
//! Everything here comes from `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF`, a repo
//! we own — there is no runtime dependency on the third-party upstream
//! `codefuse-ai/F2LLM-v2-330M` repo. See `docs/third-party-models.md` for the
//! Apache-2.0 attribution and the pinned upstream revision these artifacts
//! were derived from.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hf_hub::{Repo, RepoType, api::sync::ApiBuilder};
use spelunk_embed::NativeEmbedder;

/// `config.json` for F2LLM-v2-330M (Qwen3 architecture config; ~1 KB).
/// Embedded directly in the binary — it's tiny and never changes independent
/// of the pinned model revision recorded in `docs/third-party-models.md`, so
/// there's no reason to fetch it over the network. Vendored at
/// `crates/spelunk-server/assets/f2llm-v2-330m-config.json`.
const CONFIG_JSON: &str = include_str!("../assets/f2llm-v2-330m-config.json");

/// Override env var naming the Hugging Face repo id that holds a **pre-quantized
/// Q8_0 GGUF** (and, alongside it, the tokenizer) for the embedder. Read from
/// `SPELUNK_EMBEDDER_GGUF_REPO` at load time; see [`prequantized_gguf_repo`]
/// for the accepted values.
///
/// By default (unset) the loader fetches `QUANT_GGUF` and `tokenizer.json`
/// from [`DEFAULT_GGUF_REPO`] via the existing hf-hub cache — first-run
/// download is ~339 MB. Set this to a different `org/repo` to fetch both from
/// there instead (it must host both files, e.g. a mirror of our repo).
const GGUF_REPO_ENV: &str = "SPELUNK_EMBEDDER_GGUF_REPO";

/// Default Hugging Face repo id holding our **own pre-quantized Q8_0 GGUF**
/// (`f2llm-v2-330m-q8_0.gguf`) and tokenizer (`tokenizer.json`). Used when
/// `SPELUNK_EMBEDDER_GGUF_REPO` is unset, so a stock install fetches the
/// ~339 MB pre-quant GGUF plus tokenizer from here — no third-party repo
/// involved. Override with the env var (see [`GGUF_REPO_ENV`]).
const DEFAULT_GGUF_REPO: &str = "spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF";

/// Filename of the Q8_0-quantized GGUF cached next to the HF download.
/// Projection matmuls and the token-embedding table are stored Q8_0; the small
/// RMSNorm weights stay F32. Produced upstream by the pre-quantize pipeline
/// that publishes `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF` (see
/// `docs/third-party-models.md`), not built on device.
const QUANT_GGUF: &str = "f2llm-v2-330m-q8_0.gguf";

/// Load the F2LLM-v2-330M model, quantized to Q8_0, via the Hugging Face Hub.
///
/// Downloads our own pre-quantized GGUF (`f2llm-v2-330m-q8_0.gguf`) and
/// tokenizer (`tokenizer.json`) straight from
/// `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF` through the hf-hub cache
/// (checksum/resume reused) — first-run download is ~339 MB, cached in
/// `~/.local/share/spelunk/models/`. Set `SPELUNK_EMBEDDER_GGUF_REPO` to a
/// different `org/repo` to fetch both from there instead. `config.json` is
/// embedded in the binary (see [`CONFIG_JSON`]) and written to the same cache
/// directory so it lands next to the other artifacts as a real file.
///
/// Subsequent calls read everything from the local cache with no network
/// access. There is no runtime dependency on any third-party Hugging Face
/// repo. Once the GGUF/tokenizer/config are resolved on disk this hands off to
/// [`spelunk_embed::NativeEmbedder::load_from_path`], which does the actual
/// (network-free) model load.
pub fn load_from_hub() -> Result<NativeEmbedder> {
    let cache_dir = model_cache_dir()?;
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating model cache dir {}", cache_dir.display()))?;
    let gguf_path = cache_dir.join(QUANT_GGUF);

    tracing::info!(
        "resolving F2LLM-v2-330M (Q8_0) via Hugging Face Hub (cache: {})",
        cache_dir.display()
    );

    // config.json is embedded in the binary; write it out so it's a real file
    // next to the other artifacts (`load_from_path` reads it from disk).
    let config_path = cache_dir.join("config.json");
    std::fs::write(&config_path, CONFIG_JSON)
        .with_context(|| format!("writing embedded config.json to {}", config_path.display()))?;

    let api = ApiBuilder::new()
        .with_cache_dir(cache_dir)
        .build()
        .context("building HuggingFace Hub API client")?;

    let gguf_repo = prequantized_gguf_repo()?;
    let repo = api.repo(Repo::new(gguf_repo.clone(), RepoType::Model));

    let tokenizer_path = repo
        .get("tokenizer.json")
        .with_context(|| format!("downloading tokenizer.json from {gguf_repo}"))?;

    // Acquire the Q8_0 GGUF if it isn't already cached.
    if !gguf_path.exists() {
        tracing::info!(
            "fetching pre-quantized F2LLM-v2-330M Q8_0 GGUF from {gguf_repo} (first run)…"
        );
        let downloaded = repo
            .get(QUANT_GGUF)
            .with_context(|| format!("downloading {QUANT_GGUF} from {gguf_repo}"))?;
        // hf-hub returns a path inside its own blob/snapshot layout;
        // copy it to the stable cache path the loader reads from.
        if downloaded != gguf_path {
            std::fs::copy(&downloaded, &gguf_path).with_context(|| {
                format!(
                    "caching {} -> {}",
                    downloaded.display(),
                    gguf_path.display()
                )
            })?;
        }
        tracing::info!("fetched pre-quantized model to {}", gguf_path.display());
    }

    NativeEmbedder::load_from_path(&gguf_path, &tokenizer_path, &config_path)
}

/// Load the F2LLM-v2-330M embedder from a directory an operator provisioned
/// out-of-band (`spelunk-server --model-dir <path>` /
/// `SPELUNK_MODEL_DIR`), with zero network access. Unlike [`load_from_hub`],
/// this function never references `hf_hub`: the offline path is a pure
/// filesystem read, so there is no code path here for a corp firewall to
/// block. See "Air-gapped / no-egress install" in `docs/server-setup.md` for
/// the fetch-and-transfer procedure that produces this directory on a
/// connected machine.
///
/// Expects `dir` to contain the two artifacts that vary per pinned model
/// revision: the Q8_0 GGUF (see [`QUANT_GGUF`]) and `tokenizer.json`, exactly
/// as fetched by [`load_from_hub`]. `config.json` never changes independent
/// of the pinned revision (see [`CONFIG_JSON`]), so it's optional here: if
/// present it's used as-is (an explicit override), otherwise the embedded
/// default is written into `dir` so a second load from the same directory is
/// fully self-contained from just those two transferred files.
pub fn load_from_model_dir(dir: &Path) -> Result<NativeEmbedder> {
    anyhow::ensure!(
        dir.is_dir(),
        "--model-dir {} is not a directory. See \"Air-gapped / no-egress install\" in \
         docs/server-setup.md for the offline provisioning procedure.",
        dir.display()
    );

    let gguf_path = dir.join(QUANT_GGUF);
    let tokenizer_path = dir.join("tokenizer.json");
    let config_path = dir.join("config.json");

    anyhow::ensure!(
        gguf_path.exists(),
        "offline model artifact missing: {} not found in --model-dir {}. See \
         \"Air-gapped / no-egress install\" in docs/server-setup.md for the fetch-and-transfer \
         procedure.",
        QUANT_GGUF,
        dir.display()
    );
    anyhow::ensure!(
        tokenizer_path.exists(),
        "offline model artifact missing: tokenizer.json not found in --model-dir {}. See \
         \"Air-gapped / no-egress install\" in docs/server-setup.md for the fetch-and-transfer \
         procedure.",
        dir.display()
    );

    if !config_path.exists() {
        std::fs::write(&config_path, CONFIG_JSON).with_context(|| {
            format!("writing embedded config.json to {}", config_path.display())
        })?;
    }

    tracing::info!(
        "loading F2LLM-v2-330M (Q8_0) from offline --model-dir {} (zero network access)",
        dir.display()
    );

    NativeEmbedder::load_from_path(&gguf_path, &tokenizer_path, &config_path)
}

fn model_cache_dir() -> Result<PathBuf> {
    dirs::data_local_dir()
        .map(|d| d.join("spelunk").join("models"))
        .ok_or_else(|| anyhow::anyhow!("could not determine local data directory"))
}

/// Resolve the HF repo id of the pre-quantized Q8_0 GGUF (and tokenizer) to
/// fetch, from `SPELUNK_EMBEDDER_GGUF_REPO`.
///
/// The env var (after trimming surrounding whitespace) is interpreted as:
///
/// * **unset** → `DEFAULT_GGUF_REPO` — the default; a stock install fetches the
///   ~339 MB pre-quant GGUF plus tokenizer from
///   `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF`.
/// * **`off`** (any case) → hard error. This was previously an escape hatch
///   that downloaded the upstream BF16 safetensors and quantized them on
///   device; that path has been removed (v1: the pre-quantized first-party
///   GGUF is the only delivery mechanism), so a leftover `off` in the
///   environment now fails loudly instead of silently changing behavior.
/// * **any other value** → that `org/repo` id (trimmed) — override: fetch the
///   pre-quant GGUF and tokenizer from there instead (it must host both
///   files).
fn prequantized_gguf_repo() -> Result<String> {
    match std::env::var(GGUF_REPO_ENV) {
        Ok(v) => {
            let v = v.trim();
            anyhow::ensure!(
                !v.eq_ignore_ascii_case("off"),
                "{GGUF_REPO_ENV}=off is no longer supported: on-device quantization from \
                 upstream BF16 weights was removed (v1 always fetches the pre-quantized \
                 first-party GGUF). Unset {GGUF_REPO_ENV} to use the default repo, or set it \
                 to an `org/repo` that hosts a pre-quantized GGUF."
            );
            if v.is_empty() {
                Ok(DEFAULT_GGUF_REPO.to_string())
            } else {
                Ok(v.to_string())
            }
        }
        Err(_) => Ok(DEFAULT_GGUF_REPO.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `prequantized_gguf_repo()` resolves the GGUF source from
    /// `SPELUNK_EMBEDDER_GGUF_REPO`: unset/blank → the bundled default repo;
    /// `off` (any case, any surrounding whitespace) → hard error, since the
    /// on-device-quantize escape hatch it used to select has been removed;
    /// any other value → that `org/repo` (trimmed). Uses `serial` because it
    /// mutates a process-global env var.
    #[test]
    #[serial_test::serial(gguf_repo_env)]
    fn prequantized_gguf_repo_defaults_to_bundled_repo() {
        // SAFETY: guarded by #[serial] so no other test reads/writes this var
        // concurrently; we restore it before returning.
        let prev = std::env::var(GGUF_REPO_ENV).ok();

        unsafe { std::env::remove_var(GGUF_REPO_ENV) };
        assert_eq!(
            prequantized_gguf_repo().ok().as_deref(),
            Some("spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF"),
            "unset env var must default to fetching the bundled pre-quant GGUF"
        );

        unsafe { std::env::set_var(GGUF_REPO_ENV, "   ") };
        assert_eq!(
            prequantized_gguf_repo().ok().as_deref(),
            Some("spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF"),
            "blank/whitespace env var must fall back to the default repo, not fetch \"\""
        );

        // The removed escape hatch (`off`) must now error clearly rather than
        // silently changing behavior, in any case or with surrounding whitespace.
        for off in ["off", "OFF", "  off  "] {
            unsafe { std::env::set_var(GGUF_REPO_ENV, off) };
            assert!(
                prequantized_gguf_repo().is_err(),
                "`{off}` must be a hard error now that on-device quantize is removed"
            );
        }

        // Override: an explicit repo id is used verbatim, with whitespace trimmed.
        unsafe { std::env::set_var(GGUF_REPO_ENV, "  org/repo  ") };
        assert_eq!(prequantized_gguf_repo().ok().as_deref(), Some("org/repo"));

        match prev {
            Some(v) => unsafe { std::env::set_var(GGUF_REPO_ENV, v) },
            None => unsafe { std::env::remove_var(GGUF_REPO_ENV) },
        }
    }

    /// `model_cache_dir()` honours `XDG_DATA_HOME` when set (the Docker image
    /// points this at the persistent `/data` volume so the ~339 MB model
    /// survives `docker rm`/recreate, instead of landing in the container
    /// layer or a home directory that doesn't exist for the `-r` service
    /// user). Linux-only: `dirs::data_local_dir()` follows the XDG spec on
    /// Linux/BSD, but macOS ignores `XDG_DATA_HOME` entirely in favor of
    /// `~/Library/Application Support` (the Docker image is Linux, so that's
    /// the platform this fix targets). Uses `serial` because it mutates a
    /// process-global env var.
    #[test]
    #[cfg(target_os = "linux")]
    #[serial_test::serial(xdg_data_home_env)]
    fn model_cache_dir_honours_xdg_data_home() {
        // SAFETY: guarded by #[serial] so no other test reads/writes this var
        // concurrently; we restore it before returning.
        let prev = std::env::var("XDG_DATA_HOME").ok();

        let tmp = std::env::temp_dir().join("spelunk-model-cache-dir-test");
        unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };

        assert_eq!(
            model_cache_dir().expect("resolve cache dir"),
            tmp.join("spelunk").join("models")
        );

        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
    }

    /// End-to-end semantic-discrimination check over the real model. Ignored by
    /// default: it downloads the ~339 MB pre-quantized GGUF and runs inference.
    /// Run with `cargo test -p spelunk-server -- --ignored embeddings_discriminate`.
    ///
    /// With the #19 GQA bug present, related and unrelated pairs collapse to the
    /// same cosine (~0.1–0.25); with the fix, related pairs sit well above
    /// unrelated. This is the only test that exercises attention end-to-end via
    /// the Hub acquisition path (the pure-local path has its own coverage in
    /// `spelunk-embed`).
    #[test]
    #[ignore = "downloads the F2LLM model and runs inference"]
    fn embeddings_discriminate_related_from_unrelated() {
        use spelunk_core::embeddings::EmbeddingBackend;

        let embedder = load_from_hub().expect("load F2LLM-v2-330M");
        let rt = tokio::runtime::Runtime::new().unwrap();

        let texts: [&str; 3] = [
            "read the contents of a file from disk",
            "open a file and return its bytes",
            "the fall of the roman empire",
        ];
        let vecs = rt.block_on(embedder.embed(&texts)).expect("embed");

        // Embeddings are L2-normalised, so dot product == cosine similarity.
        let cos = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let related = cos(&vecs[0], &vecs[1]);
        let unrelated = cos(&vecs[0], &vecs[2]);

        assert!(
            related > unrelated + 0.2,
            "GQA-fixed embeddings must discriminate related from unrelated: \
             related={related:.3} vs unrelated={unrelated:.3} (spelunk-oss#19)"
        );
    }

    /// End-to-end proof that an oversized single chunk no longer OOMs/aborts
    /// (spelunk-oss#17), exercised via the Hub acquisition path. Ignored by
    /// default: downloads the model and runs inference.
    ///
    /// Run with:
    ///   SPELUNK_SECRET_STORE=file cargo test -p spelunk-server \
    ///     -- --ignored oversized_chunk_embeds_without_oom
    #[test]
    #[ignore = "downloads the F2LLM model and runs inference"]
    fn oversized_chunk_embeds_without_oom() {
        use spelunk_core::embeddings::EmbeddingBackend;

        let embedder = load_from_hub().expect("load F2LLM-v2-330M");
        let rt = tokio::runtime::Runtime::new().unwrap();

        // ~60 k whitespace-separated tokens — comfortably past MAX_SEQ_LEN
        // (40 960) and ~10x the 2 GiB cap (~5 792). Pre-fix this aborts the
        // process; post-fix it is truncated to the cap and embeds cleanly.
        let huge = "fn pagerank ( edges ) { compute } ".repeat(12_000);
        let normal = "read the contents of a file from disk";

        let vecs = rt
            .block_on(embedder.embed(&[huge.as_str(), normal]))
            .expect("embed must complete (truncated), not OOM/abort");

        assert_eq!(vecs.len(), 2);
        assert!(
            vecs[0].iter().all(|x| x.is_finite()),
            "truncated oversized-chunk embedding must be finite"
        );
        let norm: f32 = vecs[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "embedding must be L2-normalised");
    }

    /// Normal-sized chunks must embed identically whether or not the
    /// memory-budget cap is in effect (no regression for the common case).
    /// Ignored by default: downloads the model and runs inference.
    #[test]
    #[ignore = "downloads the F2LLM model and runs inference"]
    fn normal_chunk_unaffected_by_cap() {
        use spelunk_core::embeddings::EmbeddingBackend;

        let embedder = load_from_hub().expect("load F2LLM-v2-330M");
        let rt = tokio::runtime::Runtime::new().unwrap();

        let text = "pub fn compute_pagerank(edges: &[(String, String)]) -> Vec<f32> { todo!() }";
        let a = rt.block_on(embedder.embed(&[text])).expect("embed a");
        let b = rt.block_on(embedder.embed(&[text])).expect("embed b");
        assert_eq!(a[0], b[0], "normal-chunk embedding must be deterministic");
        // Sanity: this chunk is well under any budget-derived cap, so it was
        // never truncated — the produced vector is the full-precision result.
        assert!(text.split_whitespace().count() < 5792);
    }

    /// End-to-end: load the embedder via the Hub, priming the local cache, then
    /// load again from the resolved local paths with no network and assert an
    /// 896-dim L2-normalised vector. Ignored by default; downloads the model on
    /// first run.
    ///
    /// Run with:
    ///   SPELUNK_SECRET_STORE=file cargo test -p spelunk-server \
    ///     -- --ignored load_from_path_embeds
    #[test]
    #[ignore = "requires model artifacts already present in the local cache"]
    fn load_from_path_embeds_896_dim() {
        use spelunk_core::embeddings::EmbeddingBackend;
        use spelunk_embed::DIM;

        // Warm the local cache via the Hub loader (no-op if already cached).
        load_from_hub().expect("prime local cache");

        let cache_dir = model_cache_dir().expect("cache dir");
        let gguf = cache_dir.join(QUANT_GGUF);

        // config.json is embedded and written directly to the cache dir root
        // (see `load_from_hub`). The tokenizer comes from our own
        // `DEFAULT_GGUF_REPO`, cached under the hf-hub snapshot layout
        // `<cache>/models--spelunk-cloud--F2LLM-v2-330M-Q8_0-GGUF/snapshots/<rev>/tokenizer.json`.
        let config = cache_dir.join("config.json");
        let tokenizer = std::fs::read_dir(
            cache_dir
                .join("models--spelunk-cloud--F2LLM-v2-330M-Q8_0-GGUF")
                .join("snapshots"),
        )
        .expect("hf-hub snapshots dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("tokenizer.json"))
        .find(|p| p.exists())
        .expect("cached tokenizer.json");

        let embedder = NativeEmbedder::load_from_path(&gguf, &tokenizer, &config)
            .expect("offline load from local path");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let vecs = rt
            .block_on(embedder.embed(&["read the contents of a file from disk"]))
            .expect("embed");

        assert_eq!(vecs.len(), 1);
        assert_eq!(vecs[0].len(), DIM, "must be 896-dim");
        let norm: f32 = vecs[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "embedding must be L2-normalised");
    }

    /// `token_cap()` (the `EmbeddingBackend` trait method `/v1/health`'s
    /// `limits.embedder_token_cap` reads) must report a real, usable,
    /// host-derived cap for a fully loaded embedder — not `None` and not a
    /// degenerate value. This is the live end-to-end proof; the pure-math
    /// derivation itself (`derive_token_cap`/`single_chunk_budget`) has its own
    /// unconditional unit coverage in `spelunk_embed::embedder_native::tests`.
    /// Ignored by default: downloads the model. Run with:
    ///   SPELUNK_SECRET_STORE=file cargo test -p spelunk-server \
    ///     -- --ignored native_embedder_reports_its_token_cap
    #[test]
    #[ignore = "downloads the F2LLM model"]
    fn native_embedder_reports_its_token_cap() {
        use spelunk_core::embeddings::EmbeddingBackend;

        let embedder = load_from_hub().expect("load F2LLM-v2-330M");

        let cap = embedder
            .token_cap()
            .expect("a loaded NativeEmbedder must report a host-derived token cap");
        // Sanity bounds matching the documented derivation (~5 792 @ 2 GiB,
        // ~8 192 @ 4 GiB budget; see `derive_token_cap`'s doc comment) without
        // reaching into spelunk-embed's private constants from this crate.
        assert!(cap >= 1000, "token cap implausibly small: {cap}");
        assert!(
            cap <= 40_960,
            "token cap must not exceed MAX_SEQ_LEN: {cap}"
        );
    }

    // ── Offline / air-gapped model-dir load ───────────────────────────────────

    /// A `--model-dir` pointing at a plain file (not a directory) is a clear
    /// misconfiguration error, not a panic or a silent Hub fallback.
    #[test]
    fn load_from_model_dir_rejects_non_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"").unwrap();

        let msg = match load_from_model_dir(&file) {
            Ok(_) => panic!("a file path must not be accepted as --model-dir"),
            Err(e) => format!("{e:#}"),
        };
        assert!(msg.contains(&file.display().to_string()));
        assert!(
            msg.contains("server-setup.md"),
            "error must point at the offline provisioning docs, got: {msg}"
        );
    }

    /// An empty `--model-dir` (no artifacts provisioned yet) must fail with a
    /// clear error naming the missing GGUF and pointing at the offline docs
    /// section, never a bare Hugging Face Hub connection error, since this
    /// path never touches `hf_hub` at all.
    #[test]
    fn load_from_model_dir_missing_gguf_names_file_and_docs() {
        let dir = tempfile::tempdir().unwrap();

        let msg = match load_from_model_dir(dir.path()) {
            Ok(_) => panic!("an empty --model-dir must be a load error"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            msg.contains(QUANT_GGUF),
            "error must name the missing file: {msg}"
        );
        assert!(
            msg.contains("server-setup.md"),
            "error must point at the offline docs: {msg}"
        );
        assert!(
            !msg.contains("http") && !msg.contains("huggingface") && !msg.contains("downloading"),
            "must not reference any network fetch, got: {msg}"
        );
    }

    /// With the GGUF present but the tokenizer absent, the error names the
    /// tokenizer specifically, not a generic "artifacts missing".
    #[test]
    fn load_from_model_dir_missing_tokenizer_names_file_and_docs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(QUANT_GGUF), b"not a real gguf").unwrap();

        let msg = match load_from_model_dir(dir.path()) {
            Ok(_) => panic!("a missing tokenizer must be a load error"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            msg.contains("tokenizer.json"),
            "error must name the missing file: {msg}"
        );
        assert!(
            msg.contains("server-setup.md"),
            "error must point at the offline docs: {msg}"
        );
    }

    /// Both artifacts present but corrupt: the error must come from the local
    /// parse (naming the specific bad file), matching `load_from_path`'s
    /// existing per-file error behaviour: never a network error, never a
    /// panic (proving "no crash loop" starts from a `Result`, not a `unwrap`).
    #[test]
    fn load_from_model_dir_corrupt_tokenizer_errors_locally() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(QUANT_GGUF), b"not a real gguf").unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), b"not valid json").unwrap();

        let msg = match load_from_model_dir(dir.path()) {
            Ok(_) => panic!("a corrupt tokenizer must be a load error"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            msg.contains("tokenizer"),
            "error must name the tokenizer as the failing artifact, got: {msg}"
        );
        assert!(
            !msg.contains("http") && !msg.contains("huggingface") && !msg.contains("downloading"),
            "corrupt-artifact error must not reference any network fetch, got: {msg}"
        );
    }

    /// A minimal-but-valid `tokenizer.json`, built through the `tokenizers`
    /// crate's own serializer rather than hand-typed JSON, so a corrupt-GGUF
    /// test can get past tokenizer parsing and reach the GGUF parse itself
    /// (`Qwen3EmbedWeights::from_gguf`), a different failure mode with a
    /// different error path than the corrupt-tokenizer case above.
    fn write_valid_tokenizer(path: &std::path::Path) {
        let vocab: std::collections::HashMap<String, u32> =
            [("<unk>".to_string(), 0u32)].into_iter().collect();
        let model = tokenizers::models::wordlevel::WordLevel::builder()
            .vocab(vocab.into_iter().collect())
            .unk_token("<unk>".to_string())
            .build()
            .expect("valid WordLevel fixture model");
        tokenizers::Tokenizer::new(model)
            .save(path, false)
            .expect("saving fixture tokenizer.json");
    }

    /// Corrupt GGUF with a *valid* tokenizer must fail inside GGUF parsing
    /// (`Qwen3EmbedWeights::from_gguf`), not tokenizer parsing - proving the
    /// two artifact-corruption cases take genuinely distinct error paths
    /// rather than both happening to fail on whichever the code checks first.
    #[test]
    fn load_from_model_dir_corrupt_gguf_errors_locally() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(QUANT_GGUF), b"not a real gguf").unwrap();
        write_valid_tokenizer(&dir.path().join("tokenizer.json"));
        // No config.json: the real embedded config is auto-written, so the
        // failure is attributable to the GGUF alone.

        let msg = match load_from_model_dir(dir.path()) {
            Ok(_) => panic!("a corrupt GGUF must be a load error"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            !msg.contains("tokenizer") && !msg.contains("config.json"),
            "error must not misattribute a GGUF failure to the tokenizer or config, got: {msg}"
        );
        assert!(
            !msg.contains("http") && !msg.contains("huggingface") && !msg.contains("downloading"),
            "corrupt-GGUF error must not reference any network fetch, got: {msg}"
        );
    }

    /// A `--model-dir` containing only `tokenizer.json` (no GGUF at all) must
    /// still name the GGUF as missing, the same as a fully empty directory -
    /// proving the existence check order doesn't let a present tokenizer mask
    /// the missing GGUF with a different (e.g. tokenizer-shaped) error.
    #[test]
    fn load_from_model_dir_tokenizer_only_still_names_missing_gguf() {
        let dir = tempfile::tempdir().unwrap();
        write_valid_tokenizer(&dir.path().join("tokenizer.json"));

        let msg = match load_from_model_dir(dir.path()) {
            Ok(_) => panic!("a tokenizer-only --model-dir must be a load error"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            msg.contains(QUANT_GGUF),
            "error must name the missing GGUF even with tokenizer.json present: {msg}"
        );
        assert!(
            msg.contains("server-setup.md"),
            "error must point at the offline docs: {msg}"
        );
    }

    /// A `--model-dir` pointing at a path that doesn't exist at all (as
    /// opposed to an existing non-directory file) must fail with the same
    /// clear "not a directory" error naming the path, not a confusing
    /// downstream OS error from inside file-open calls.
    #[test]
    fn load_from_model_dir_rejects_nonexistent_path() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("does-not-exist");

        let msg = match load_from_model_dir(&missing) {
            Ok(_) => panic!("a nonexistent path must not be accepted as --model-dir"),
            Err(e) => format!("{e:#}"),
        };
        assert!(msg.contains(&missing.display().to_string()));
        assert!(
            msg.contains("is not a directory"),
            "error must clearly say the directory itself is missing, got: {msg}"
        );
        assert!(
            msg.contains("server-setup.md"),
            "error must point at the offline provisioning docs, got: {msg}"
        );
    }

    /// `load_from_model_dir` writes the embedded `config.json` into the
    /// directory when missing, mirroring `load_from_hub`'s cache layout, so
    /// an operator only ever needs to transfer the two revision-specific
    /// files (GGUF + tokenizer) and a second load from the same directory is
    /// fully self-contained.
    #[test]
    fn load_from_model_dir_writes_embedded_config_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(QUANT_GGUF), b"not a real gguf").unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), b"not valid json").unwrap();

        // The load itself still fails (corrupt fixtures), but config.json must
        // have been written before the failing tokenizer parse.
        let _ = load_from_model_dir(dir.path());
        let config_path = dir.path().join("config.json");
        assert!(
            config_path.exists(),
            "embedded config.json must be written to --model-dir"
        );
        assert_eq!(std::fs::read_to_string(config_path).unwrap(), CONFIG_JSON);
    }

    /// A second server start against the same `--model-dir` (config.json now
    /// present from the first run's auto-write) must behave identically to
    /// the first: the existing file is used as-is, not re-written or treated
    /// as a conflict, so the resulting error (from the still-corrupt GGUF /
    /// tokenizer fixtures) is unchanged between runs.
    #[test]
    fn load_from_model_dir_second_start_reuses_written_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(QUANT_GGUF), b"not a real gguf").unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), b"not valid json").unwrap();

        let first_msg = match load_from_model_dir(dir.path()) {
            Ok(_) => panic!("corrupt fixtures must be a load error"),
            Err(e) => format!("{e:#}"),
        };
        let config_path = dir.path().join("config.json");
        assert!(config_path.exists(), "first run must write config.json");

        // Simulate an operator restart: model-dir now has all three paths
        // present, exactly like a second `spelunk-server --model-dir` start.
        let second_msg = match load_from_model_dir(dir.path()) {
            Ok(_) => panic!("corrupt fixtures must still be a load error on a second start"),
            Err(e) => format!("{e:#}"),
        };

        assert_eq!(
            first_msg, second_msg,
            "a pre-existing config.json must not change the load outcome"
        );
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            CONFIG_JSON,
            "the pre-existing config.json must be left as the same embedded default, not corrupted by a second write"
        );
    }

    /// Zero-egress guarantee under a hostile network: even with every standard
    /// proxy env var pointed at an address nothing listens on,
    /// `load_from_model_dir` must behave identically to a clean environment:
    /// same error, and fast (no hang waiting on a dead proxy). The only way
    /// that holds is if the code path never attempts a network request at
    /// all. Guards against a future edit reintroducing an `hf_hub`/`reqwest`
    /// call into this function.
    #[test]
    #[serial_test::serial(network_proxy_env)]
    fn load_from_model_dir_ignores_hostile_network_env() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(QUANT_GGUF), b"not a real gguf").unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), b"not valid json").unwrap();

        let err_msg = |dir: &std::path::Path| match load_from_model_dir(dir) {
            Ok(_) => panic!("corrupt fixtures must be a load error"),
            Err(e) => format!("{e:#}"),
        };
        let clean_msg = err_msg(dir.path());

        // Point every standard proxy env var at a closed local port: any
        // accidental network call in this path would fail differently (or
        // hang) via the proxy, changing the message or the timing.
        let proxy_vars = [
            "http_proxy",
            "https_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
        ];
        // SAFETY: guarded by #[serial] so no other test reads/writes these
        // vars concurrently; restored before returning.
        let prev: Vec<Option<String>> = proxy_vars.iter().map(|v| std::env::var(v).ok()).collect();
        for v in proxy_vars {
            unsafe { std::env::set_var(v, "http://127.0.0.1:1") };
        }

        let started = std::time::Instant::now();
        let hostile_msg = err_msg(dir.path());
        let elapsed = started.elapsed();

        for (v, val) in proxy_vars.iter().zip(prev) {
            match val {
                Some(v2) => unsafe { std::env::set_var(v, v2) },
                None => unsafe { std::env::remove_var(v) },
            }
        }

        assert_eq!(
            clean_msg, hostile_msg,
            "load_from_model_dir must behave identically regardless of network reachability"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "must fail on the local parse alone, never wait on a network call: {elapsed:?}"
        );
    }

    /// End-to-end round-trip: the artifacts `load_from_hub` fetches onto a
    /// connected machine must be exactly what `load_from_model_dir` accepts
    /// once copied into a flat directory, and both load paths must produce
    /// agreeing embeddings for the same input. This is the proof that the
    /// documented fetch-and-transfer procedure (AC5) produces a directory this
    /// offline path actually loads. Ignored by default: downloads the model.
    ///
    /// Run with:
    ///   SPELUNK_SECRET_STORE=file cargo test -p spelunk-server \
    ///     -- --ignored offline_model_dir_round_trips_with_hub_artifacts
    #[test]
    #[ignore = "downloads the F2LLM model"]
    fn offline_model_dir_round_trips_with_hub_artifacts() {
        use spelunk_core::embeddings::EmbeddingBackend;

        // Prime the Hub cache, then locate the resolved files exactly as
        // `load_from_path_embeds_896_dim` does above.
        load_from_hub().expect("prime local cache via Hub");
        let cache_dir = model_cache_dir().expect("cache dir");
        let hub_gguf = cache_dir.join(QUANT_GGUF);
        let hub_config = cache_dir.join("config.json");
        let hub_tokenizer = std::fs::read_dir(
            cache_dir
                .join("models--spelunk-cloud--F2LLM-v2-330M-Q8_0-GGUF")
                .join("snapshots"),
        )
        .expect("hf-hub snapshots dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("tokenizer.json"))
        .find(|p| p.exists())
        .expect("cached tokenizer.json");

        // Simulate the operator's transfer: copy just the two
        // revision-specific files into a fresh flat directory.
        let offline_dir = tempfile::tempdir().unwrap();
        std::fs::copy(&hub_gguf, offline_dir.path().join(QUANT_GGUF)).unwrap();
        std::fs::copy(&hub_tokenizer, offline_dir.path().join("tokenizer.json")).unwrap();
        let _ = &hub_config; // config.json is embedded; the offline loader writes its own copy.

        let hub_embedder = NativeEmbedder::load_from_path(&hub_gguf, &hub_tokenizer, &hub_config)
            .expect("load via the Hub-resolved paths");
        let offline_embedder =
            load_from_model_dir(offline_dir.path()).expect("load via the offline model-dir path");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let text = "read the contents of a file from disk";
        let hub_vec = rt.block_on(hub_embedder.embed(&[text])).expect("hub embed");
        let offline_vec = rt
            .block_on(offline_embedder.embed(&[text]))
            .expect("offline embed");

        assert_eq!(
            hub_vec, offline_vec,
            "the same artifacts loaded via either path must produce identical embeddings"
        );
    }
}
