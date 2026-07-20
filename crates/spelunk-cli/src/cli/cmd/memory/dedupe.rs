//! `spelunk memory dedupe`: collapse duplicate-`entity_id` groups already
//! resident in the local `memory.db`.
//!
//! Existing duplicate rows under the ADR-068 canonical `entity_id` (rows with
//! byte-identical `{kind, title, body}` but differing `created_at`, `tags`,
//! `linked_files`, or `status`) are never collapsed automatically: opening
//! the store only backfills `entity_id` and, once zero duplicate groups
//! remain, promotes `idx_notes_entity_id` to UNIQUE (see
//! `spelunk_core::storage::memory::entity_id_migration`). This command is the
//! first place in the codebase that deletes existing local memory rows, so
//! it is explicit and dry-run-able rather than riding along on that
//! migration. See ADR-068's third amendment for the merge rule.

use anyhow::{Context, Result};

use super::MemoryDedupeArgs;
use crate::storage::{DedupeSummary, MemoryStore};

pub(super) async fn memory_dedupe(
    args: MemoryDedupeArgs,
    mem_path: &std::path::Path,
) -> Result<()> {
    let json = crate::utils::effective_format(&args.format) == "json";

    let store = MemoryStore::open(mem_path)
        .with_context(|| format!("opening memory.db at {}", mem_path.display()))?;

    let summary = store
        .dedupe_entity_ids(args.dry_run)
        .context("collapsing duplicate entity_id groups")?;

    emit_summary(&summary, json, args.dry_run);
    Ok(())
}

fn emit_summary(summary: &DedupeSummary, json: bool, dry_run: bool) {
    if json {
        println!("{}", serde_json::to_string(summary).unwrap_or_default());
        return;
    }
    if dry_run {
        eprintln!(
            "[spelunk] dedupe (dry-run): total_notes={} duplicate_groups={} rows_would_collapse={}",
            summary.total_notes, summary.duplicate_groups, summary.rows_collapsed
        );
        return;
    }
    eprintln!(
        "[spelunk] dedupe: total_notes={} duplicate_groups={} rows_collapsed={} \
         tags_merged={} linked_files_merged={} supersede_edges_repointed={} \
         supersede_self_edges_dropped={}",
        summary.total_notes,
        summary.duplicate_groups,
        summary.rows_collapsed,
        summary.tags_merged,
        summary.linked_files_merged,
        summary.supersede_edges_repointed,
        summary.supersede_self_edges_dropped,
    );
}
