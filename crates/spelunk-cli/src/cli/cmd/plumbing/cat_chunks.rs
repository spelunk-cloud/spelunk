use anyhow::Result;

use super::PlumbingCatChunksArgs;
use crate::{config::Config, storage::Database};

pub(super) fn cat_chunks(args: PlumbingCatChunksArgs, db: &Database, _cfg: &Config) -> Result<()> {
    // Stored paths are normalized to forward slashes; normalize the query arg so
    // a Windows caller passing `src\lib.rs` matches the indexed `src/lib.rs`.
    let file = spelunk_core::utils::normalize_index_path(&args.file);
    let chunks = db.chunks_for_file(&file)?;
    if chunks.is_empty() {
        eprintln!("No indexed chunks for '{}'", args.file);
        std::process::exit(1);
    }
    for c in chunks {
        println!("{}", serde_json::to_string(&c)?);
    }
    Ok(())
}
