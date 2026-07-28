//! Native F2LLM-v2-330M embedder for spelunk.
//!
//! This crate owns the candle-based embedding engine (Qwen3 decoder,
//! 896-dim, Q8_0-quantized GGUF, Metal/GPU on macOS). It is a library so both
//! the bundled `spelunk-server` binary and downstream consumers that need a
//! local embedder can depend on it directly.
//!
//! The sole load entry point is [`NativeEmbedder::load_from_path`], which loads
//! the model from local files already on disk with **zero network access** —
//! callers fetch the GGUF, tokenizer, and config themselves. This crate
//! deliberately carries no download/fetch dependency of its own (no `hf-hub`),
//! so anything that depends on `spelunk-embed` — e.g. a minimal embedding
//! engine bundled elsewhere — inherits the smallest possible dependency
//! surface. `spelunk-server` resolves the artifacts via its own Hugging Face
//! Hub acquisition path (`embed_hub` module) and then calls `load_from_path`.
//!
//! The result is a [`NativeEmbedder`], which implements the crate's own
//! [`EmbeddingBackend`] trait (re-exported by spelunk-core at
//! `spelunk_core::embeddings::EmbeddingBackend`). candle is an unconditional
//! dependency: depending on this crate means you want its native embedder, so
//! there is no feature gate to opt out of candle. Add the `metal` feature for
//! Metal GPU acceleration on macOS.

mod backend;
pub use backend::EmbeddingBackend;

/// Stable provenance id for the native embedding model, `<repo-shortname>@<dim>`.
/// Exact-match token: never parse it. Requantization or a hardware-portability
/// rebuild of the same model must NOT change this — only a genuine model swap
/// (different weights / vector space) does, which forces a re-index.
///
/// Before changing this value, ship memory.db embedding-provenance stamping
/// first: unstamped `note_embeddings` vectors are assumed to be this model
/// (backfill rule "unstamped ⇒ F2LLM-v2-330M@896"), an invariant that only
/// holds while this is the sole model ever shipped. See the 2026-07-26
/// `requirement` entry in spelunk memory and task spelunk-oss^286 for the
/// acceptance criteria that work must meet.
pub const MODEL_ID: &str = "F2LLM-v2-330M@896";

mod embedder_native;
pub use embedder_native::{DIM, NativeEmbedder};

mod error;
pub use error::EmbedError;
