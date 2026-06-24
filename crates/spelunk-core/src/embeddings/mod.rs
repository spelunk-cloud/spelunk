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

/// Deserialise raw little-endian bytes back to a float vector.
#[allow(dead_code)]
pub fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}
