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
    // ADR-067: chunks needs the index and has no live mode, so an un-init'd dir
    // refuses via open_project_db rather than reading the global store.
    let (_db_path, db) = open_project_db(args.db.as_deref(), &cfg.db_path)?;
    // Stored paths use forward slashes; normalize the query arg so a Windows
    // caller passing `src\lib.rs` matches the indexed `src/lib.rs`.
    let results = db.chunks_for_file(&spelunk_core::utils::normalize_index_path(&args.path))?;

    if results.is_empty() {
        // A file with no chunks may simply be un-indexed, but if the built-in
        // index filter excludes it the bare "No chunks found" is misleading:
        // explain the exclusion and how to re-include it.
        if let Some(explanation) = index_filter_explanation(&cfg, &args.path) {
            print!("{explanation}");
        } else {
            println!("No chunks found for '{}'.", args.path);
        }
        return Ok(());
    }

    match crate::utils::effective_format(&args.format) {
        "json" => println!("{}", serde_json::to_string_pretty(&results)?),
        _ => print_chunks_text(&results),
    }

    Ok(())
}

/// If `path` is excluded by the built-in index filter, return a message that
/// names the matched pattern and shows the `[index]` re-include recipe;
/// otherwise `None` (the caller falls back to the bare "No chunks found").
///
/// Detects two exclusion routes: a glob match (deterministic from the path) and
/// a generated-marker (best-effort, only when the file resolves on disk).
fn index_filter_explanation(cfg: &Config, path: &str) -> Option<String> {
    use spelunk_core::indexer::filter::{Decision, IndexFilter, generated_marker};

    let filter = IndexFilter::build(
        &cfg.index.exclude,
        cfg.index.use_default_excludes,
        cfg.index.detect_generated,
    )
    .ok()?;
    let norm = spelunk_core::utils::normalize_index_path(path);

    // Parent-aware: catches a file nested under an excluded dir (e.g.
    // `node_modules/...`) as well as a direct glob match.
    let (reason, recipe) = match filter.classify(std::path::Path::new(&norm), false) {
        Decision::Exclude(mi) => {
            let reason = format!("it matches the exclude pattern `{}`", mi.pattern);
            // A directory-prune match (pattern ends in `/`) cannot be re-included
            // by a `!file` line under the pruned dir (git parity); the recipe must
            // re-include the directory itself instead.
            let recipe = if mi.pattern.ends_with('/') {
                format!("[\"!{p}\", \"!{p}**\"]", p = mi.pattern)
            } else {
                format!("[\"!{norm}\"]")
            };
            (reason, recipe)
        }
        // Not a glob match: fall back to a marker sniff if the file is on disk.
        Decision::Keep if filter.detect_generated() => {
            let on_disk = std::env::current_dir().ok().map(|d| d.join(&norm));
            let marker = on_disk.as_deref().and_then(generated_marker)?;
            (
                format!("its header declares it generated (`{marker}`)"),
                format!("[\"!{norm}\"]"),
            )
        }
        _ => return None,
    };

    Some(format!(
        "No chunks found for '{path}': the built-in index filter skipped it because {reason}, \
         so it was never indexed.\n\
         To index it anyway, add a re-include to [index] in .spelunk/config.toml:\n\
         \n  [index]\n  exclude = {recipe}\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file excluded because its parent directory was pruned yields the
    /// **directory-form** re-include recipe: a `!file` line cannot re-include a
    /// path under an already-excluded dir (git parity), so the recipe must name
    /// the directory that was pruned, not the full file path.
    #[test]
    fn index_filter_explanation_pruned_dir_uses_directory_recipe() {
        let cfg = Config::default();
        let msg = index_filter_explanation(&cfg, "vendor/lib/pkg.js")
            .expect("excluded path must be explained");
        assert!(msg.contains("built-in index filter"), "{msg}");
        assert!(msg.contains("vendor/"), "names the matched pattern: {msg}");
        assert!(msg.contains("[index]"), "shows the config table: {msg}");
        assert!(
            msg.contains("exclude = [\"!vendor/\", \"!vendor/**\"]"),
            "shows the directory-form re-include recipe: {msg}"
        );
        // The non-functional full-path recipe must NOT be emitted for a
        // pruned-dir exclusion.
        assert!(
            !msg.contains("[\"!vendor/lib/pkg.js\"]"),
            "must not emit the non-functional full-path recipe: {msg}"
        );
    }

    /// A direct-glob match (e.g. `*.min.js`) is re-includable by its full path,
    /// so the recipe keeps the file-path form.
    #[test]
    fn index_filter_explanation_direct_glob_uses_file_recipe() {
        let cfg = Config::default();
        let msg =
            index_filter_explanation(&cfg, "app.min.js").expect("excluded path must be explained");
        assert!(msg.contains("built-in index filter"), "{msg}");
        assert!(msg.contains("*.min.js"), "names the matched pattern: {msg}");
        assert!(
            msg.contains("exclude = [\"!app.min.js\"]"),
            "shows the file-form re-include recipe: {msg}"
        );
    }

    /// A normal source path is not filter-excluded, so there is no explanation
    /// (the caller falls back to the bare "No chunks found").
    #[test]
    fn index_filter_explanation_none_for_normal_path() {
        let cfg = Config::default();
        assert!(index_filter_explanation(&cfg, "src/lib.rs").is_none());
    }
}
