use anyhow::Result;

use super::PlumbingReadMemoryArgs;
use crate::{config::Config, storage::open_memory_backend};

pub(super) async fn read_memory(
    args: PlumbingReadMemoryArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
) -> Result<()> {
    let backend = open_memory_backend(cfg, mem_path, None).await?;

    if let Some(id) = args.id {
        match backend.get(id).await? {
            None => std::process::exit(1),
            Some(note) => println!("{}", serde_json::to_string(&note)?),
        }
        return Ok(());
    }

    let notes = backend
        .list(args.kind.as_deref(), args.limit, false, None)
        .await?;

    if notes.is_empty() {
        std::process::exit(1);
    }

    for note in &notes {
        println!("{}", serde_json::to_string(note)?);
    }
    Ok(())
}
