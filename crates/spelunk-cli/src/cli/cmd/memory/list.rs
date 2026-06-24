use anyhow::Result;

use super::MemoryListArgs;
use super::{parse_as_of, print_note_summary};
use crate::{config::Config, storage::open_memory_backend};

pub(super) async fn memory_list(
    args: MemoryListArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    // Discovery nudge: warn once when unimported server.db notes exist.
    super::reconcile::maybe_emit_nudge(mem_path, cfg);

    let backend = open_memory_backend(cfg, mem_path, backend_override).await?;
    let as_of = parse_as_of(args.as_of.as_deref())?;
    let mut notes = if let Some(ref sha_prefix) = args.source_ref {
        backend
            .list_by_source_ref(sha_prefix, args.limit, args.archived, as_of)
            .await?
    } else {
        backend
            .list(args.kind.as_deref(), args.limit, args.archived, as_of)
            .await?
    };

    // Cross-project dep pass (ADR-003): append locked/cross-project decisions
    // and requirements from linked projects unless --local-only is set.
    // The dep pass is skipped when --source-ref is given (commit-specific queries
    // are inherently local) or when --archived is set (archived entries are
    // project-local housekeeping noise, not cross-cutting signals).
    if !args.local_only && args.source_ref.is_none() && !args.archived {
        let index_db_path = crate::config::resolve_db(None, &cfg.db_path);
        let mut seen: std::collections::HashSet<(String, i64)> = Default::default();
        // Seed seen set from local results so local entries don't collide with
        // same-id entries from a dep that happens to share a local path (unlikely
        // but defensive). Local notes have no root_path key, so we use "".
        for n in &notes {
            seen.insert((String::new(), n.id));
        }
        let dep_notes =
            super::cross_project::collect_dep_cross_cutting(&index_db_path, &mut seen).await;
        // Filter dep notes to the requested kind (if --kind was specified).
        let dep_notes: Vec<_> = if let Some(ref k) = args.kind {
            dep_notes.into_iter().filter(|n| &n.kind == k).collect()
        } else {
            dep_notes
        };
        notes.extend(dep_notes);
    }

    if notes.is_empty() {
        println!("No memory entries found.");
        return Ok(());
    }

    match crate::utils::effective_format(&args.format) {
        "json" => println!("{}", serde_json::to_string_pretty(&notes)?),
        "jsonl" => {
            for n in &notes {
                println!("{}", serde_json::to_string(n)?);
            }
        }
        _ => {
            for n in &notes {
                print_note_summary(n);
            }
        }
    }
    Ok(())
}
