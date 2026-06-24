use crate::get_cache_dir;
use ort::execution_providers::ExecutionProviderDispatch;
use std::path::PathBuf;

pub trait HasMaxLength {
    const MAX_LENGTH: usize;
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct InitOptionsWithLength<M> {
    pub model_name: M,
    pub execution_providers: Vec<ExecutionProviderDispatch>,
    pub cache_dir: PathBuf,
    pub show_download_progress: bool,
    pub max_length: usize,
    /// Number of intra-op threads for ONNX Runtime. `None` (the default) uses
    /// every available CPU core via `std::thread::available_parallelism`.
    /// Set this to cap CPU usage (e.g. on laptops) at the cost of throughput.
    pub intra_threads: Option<usize>,
    /// Disable ONNX Runtime memory patterns. When `false` (default) ORT
    /// pre-allocates every intermediate tensor after the first inference and
    /// retains those arenas for the session lifetime (~6 GB for 12-layer models
    /// at seq_len=512). Set to `true` for long-running server processes where
    /// post-run memory reclamation matters more than allocation overhead.
    pub disable_memory_pattern: bool,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct InitOptions<M> {
    pub model_name: M,
    pub execution_providers: Vec<ExecutionProviderDispatch>,
    pub cache_dir: PathBuf,
    pub show_download_progress: bool,
    /// Number of intra-op threads for ONNX Runtime. `None` (the default) uses
    /// every available CPU core via `std::thread::available_parallelism`.
    /// Set this to cap CPU usage (e.g. on laptops) at the cost of throughput.
    pub intra_threads: Option<usize>,
    /// Disable ONNX Runtime memory patterns. When `false` (default) ORT
    /// pre-allocates every intermediate tensor after the first inference and
    /// retains those arenas for the session lifetime (~6 GB for 12-layer models
    /// at seq_len=512). Set to `true` for long-running server processes where
    /// post-run memory reclamation matters more than allocation overhead.
    pub disable_memory_pattern: bool,
}

impl<M: Default + HasMaxLength> Default for InitOptionsWithLength<M> {
    fn default() -> Self {
        Self {
            model_name: M::default(),
            execution_providers: Default::default(),
            cache_dir: get_cache_dir().into(),
            show_download_progress: true,
            max_length: M::MAX_LENGTH,
            intra_threads: None,
            disable_memory_pattern: false,
        }
    }
}

impl<M: Default> Default for InitOptions<M> {
    fn default() -> Self {
        Self {
            model_name: M::default(),
            execution_providers: Default::default(),
            cache_dir: get_cache_dir().into(),
            show_download_progress: true,
            intra_threads: None,
            disable_memory_pattern: false,
        }
    }
}

impl<M: Default + HasMaxLength> InitOptionsWithLength<M> {
    /// Create a new InitOptionsWithLength with the given model name
    pub fn new(model_name: M) -> Self {
        Self {
            model_name,
            ..Default::default()
        }
    }

    /// Set the maximum length
    pub fn with_max_length(mut self, max_length: usize) -> Self {
        self.max_length = max_length;
        self
    }

    /// Set the cache directory for the model file
    pub fn with_cache_dir(mut self, cache_dir: PathBuf) -> Self {
        self.cache_dir = cache_dir;
        self
    }

    /// Set the execution providers for the model
    pub fn with_execution_providers(
        mut self,
        execution_providers: Vec<ExecutionProviderDispatch>,
    ) -> Self {
        self.execution_providers = execution_providers;
        self
    }

    /// Set the number of intra-op threads ONNX Runtime uses. By default
    /// (`None`) all available CPU cores are used; capping this limits CPU
    /// usage at the cost of per-inference throughput.
    pub fn with_intra_threads(mut self, intra_threads: usize) -> Self {
        self.intra_threads = Some(intra_threads);
        self
    }

    /// Set whether to show download progress
    pub fn with_show_download_progress(mut self, show_download_progress: bool) -> Self {
        self.show_download_progress = show_download_progress;
        self
    }

    /// Disable ONNX Runtime memory patterns. When enabled (the default) ORT
    /// pre-allocates all intermediate tensor buffers after the first inference
    /// and holds them for the session lifetime, which can retain several GB of
    /// RAM. Disable for server processes where post-run memory matters.
    pub fn with_disable_memory_pattern(mut self, disable: bool) -> Self {
        self.disable_memory_pattern = disable;
        self
    }
}

impl<M: Default> InitOptions<M> {
    /// Create a new InitOptions with the given model name
    pub fn new(model_name: M) -> Self {
        Self {
            model_name,
            ..Default::default()
        }
    }

    /// Set the cache directory for the model file
    pub fn with_cache_dir(mut self, cache_dir: PathBuf) -> Self {
        self.cache_dir = cache_dir;
        self
    }

    /// Set the execution providers for the model
    pub fn with_execution_providers(
        mut self,
        execution_providers: Vec<ExecutionProviderDispatch>,
    ) -> Self {
        self.execution_providers = execution_providers;
        self
    }

    /// Set the number of intra-op threads ONNX Runtime uses. By default
    /// (`None`) all available CPU cores are used; capping this limits CPU
    /// usage at the cost of per-inference throughput.
    pub fn with_intra_threads(mut self, intra_threads: usize) -> Self {
        self.intra_threads = Some(intra_threads);
        self
    }

    /// Set whether to show download progress
    pub fn with_show_download_progress(mut self, show_download_progress: bool) -> Self {
        self.show_download_progress = show_download_progress;
        self
    }

    /// Disable ONNX Runtime memory patterns. When enabled (the default) ORT
    /// pre-allocates all intermediate tensor buffers after the first inference
    /// and holds them for the session lifetime, which can retain several GB of
    /// RAM. Disable for server processes where post-run memory matters.
    pub fn with_disable_memory_pattern(mut self, disable: bool) -> Self {
        self.disable_memory_pattern = disable;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intra_threads_defaults_none_and_builder_sets() {
        let o = InitOptions::<crate::ImageEmbeddingModel>::default();
        assert_eq!(o.intra_threads, None);
        let o = o.with_intra_threads(4);
        assert_eq!(o.intra_threads, Some(4));
    }
}
