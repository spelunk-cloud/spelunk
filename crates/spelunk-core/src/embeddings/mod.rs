use anyhow::Result;

/// The embedding vector dimension produced by the default native model (F2LLM-v2-330M, 896-dim).
pub const EMBEDDING_DIM: usize = 896;

/// Trait every embedding backend must implement.
///
/// Implementations live in submodules gated by feature flags.
/// Nothing outside `src/embeddings/` or `src/backends.rs` should
/// import concrete backend types.
#[async_trait::async_trait]
pub trait EmbeddingBackend: Send + Sync {
    /// Embed a batch of text strings. Returns one vector per input.
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// Dimensionality of the output vectors.
    #[allow(dead_code)]
    fn dimension(&self) -> usize;
}

/// Serialise a float vector to raw little-endian bytes for sqlite-vec storage.
pub fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Quantise an L2-normalised float vector to `int8` bytes for a sqlite-vec
/// `int8[N]` column. Embeddings produced by F2LLM are unit vectors (components
/// in `[-1, 1]`), so each component maps to `round(x * 127)` clamped to
/// `[-127, 127]`. This is 4× smaller than f32 storage; because the scaling is
/// uniform, L2 distance ranking is preserved (a sqlite-vec int8 L2 distance is
/// ~127× the corresponding f32 distance — callers rescale by `INT8_SCALE`).
///
/// Used only for the chunk/snapshot vector tables (`embeddings`,
/// `snapshot_embeddings`); memory note vectors keep full-precision f32 storage.
pub fn vec_to_int8_blob(v: &[f32]) -> Vec<u8> {
    v.iter()
        .map(|&x| ((x * 127.0).round().clamp(-127.0, 127.0) as i8) as u8)
        .collect()
}

/// Factor by which a sqlite-vec `int8` L2 distance exceeds the equivalent f32
/// distance, given the `* 127` quantisation in [`vec_to_int8_blob`]. Divide raw
/// int8 distances by this to keep them on the same scale as the old f32 index.
pub const INT8_SCALE: f32 = 127.0;

/// Deserialise raw little-endian bytes back to a float vector.
#[allow(dead_code)]
pub fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}
