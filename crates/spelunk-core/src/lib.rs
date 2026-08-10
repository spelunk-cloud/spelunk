pub mod config;
pub mod conventions;
pub mod embeddings;
pub mod error;
pub mod indexer;
pub mod llm;
pub mod registry;
pub mod search;
pub mod storage;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod utils;
