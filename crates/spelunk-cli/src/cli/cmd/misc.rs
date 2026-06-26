use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct ChunksArgs {
    /// File path (exact or suffix match against indexed paths)
    pub path: String,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Path to the SQLite database (overrides config)
    #[arg(short, long)]
    pub db: Option<PathBuf>,
}

use super::helpers::open_project_db;
use super::ui::print_chunks_text;
use crate::config::Config;

pub fn languages() -> Result<()> {
    let langs = crate::indexer::parser::SUPPORTED_LANGUAGES;
    println!("Supported languages:");
    for lang in langs {
        println!("  {lang}");
    }
    Ok(())
}

pub fn chunks(args: ChunksArgs, cfg: Config) -> Result<()> {
    let (_db_path, db) = open_project_db(args.db.as_deref(), &cfg.db_path)?;
    // Stored paths use forward slashes; normalize the query arg so a Windows
    // caller passing `src\lib.rs` matches the indexed `src/lib.rs`.
    let results = db.chunks_for_file(&spelunk_core::utils::normalize_index_path(&args.path))?;

    if results.is_empty() {
        println!("No chunks found for '{}'.", args.path);
        return Ok(());
    }

    match crate::utils::effective_format(&args.format) {
        "json" => println!("{}", serde_json::to_string_pretty(&results)?),
        _ => print_chunks_text(&results),
    }

    Ok(())
}
