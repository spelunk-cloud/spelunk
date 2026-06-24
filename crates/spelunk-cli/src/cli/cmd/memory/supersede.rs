use anyhow::Result;

use super::{MemorySupersededArgs, backend_err};
use crate::{config::Config, storage::open_memory_backend};

pub(super) async fn memory_supersede(
    args: MemorySupersededArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    let backend = open_memory_backend(cfg, mem_path, backend_override).await?;
    if backend.get(args.new_id).await?.is_none() {
        anyhow::bail!("No memory entry with id {} (new).", args.new_id);
    }
    if backend
        .supersede(args.old_id, args.new_id)
        .await
        .map_err(backend_err)?
    {
        println!(
            "Archived #{old} → superseded by #{new}.",
            old = args.old_id,
            new = args.new_id
        );
    } else {
        anyhow::bail!("No active memory entry with id {} (old).", args.old_id);
    }
    Ok(())
}
