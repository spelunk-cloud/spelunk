use anyhow::Result;

use super::MemoryArchiveArgs;
use crate::{config::Config, storage::open_memory_backend};

pub(super) async fn memory_archive(
    args: MemoryArchiveArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    let backend = open_memory_backend(cfg, mem_path, backend_override).await?;
    if backend.archive(args.id).await? {
        println!("Archived memory entry #{}.", args.id);
    } else {
        anyhow::bail!("No active memory entry with id {}.", args.id);
    }
    Ok(())
}
