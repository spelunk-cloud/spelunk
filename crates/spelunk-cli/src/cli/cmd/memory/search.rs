use anyhow::Result;

use super::super::helpers::{embed_query, require_server_client};
use super::MemorySearchArgs;
use super::{backend_err, parse_as_of, print_note_summary};
use crate::{
    capability,
    config::Config,
    storage::{NoteId, open_memory_backend},
};

pub(super) async fn memory_search(
    args: MemorySearchArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    let index_db_path = crate::config::resolve_db(None, &cfg.db_path);
    crate::storage::record_usage_at(&index_db_path, "memory search");

    // Fold in any fetched teammate notes before searching, so a teammate's
    // newly-published entry is searchable on the default path without a re-init
    // (ADR-077 D1).
    super::reconcile::refresh_read_path_from_git_notes(cfg, mem_path, backend_override).await;

    // Discovery nudge: warn once when unimported server.db notes exist.
    super::reconcile::maybe_emit_nudge(mem_path, cfg);
    super::outbox::poll_and_apply(cfg, mem_path).await;

    // Honor the auto-discovered server tier: loopback auto-discovery sets the
    // capability tier without populating `cfg.server_url`, so build an
    // effective config that fills in `server_url`/`project_id` from the tier
    // (mirrors `explore` — IMP-3 / spelunk#316). Falls back to `cfg` unchanged
    // when the tier isn't `Server` or `server_url` is already configured.
    let project_root = mem_path.parent().unwrap_or(mem_path);
    // `get_inference_tier` (not `get_tier`): local_first always prefers the
    // local loopback embedder for query-embedding, even with an explicit
    // server_url set (2026-07-23 founder decision).
    let tier = capability::get_inference_tier(cfg).await;
    let eff_cfg = tier.effective_config(cfg, project_root);
    let cfg = &eff_cfg;

    let mode = args.mode.as_str();
    let backend = open_memory_backend(cfg, mem_path, backend_override).await?;
    let as_of = parse_as_of(args.as_of.as_deref())?;

    let notes = if mode == "text" {
        let sp = super::super::ui::spinner("Searching (text)…");
        let result = backend
            .search_text(&args.query, args.limit, as_of)
            .await
            .map_err(backend_err)?;
        sp.finish_and_clear();
        result
    } else {
        let sp = super::super::ui::spinner("Embedding query…");
        let client = require_server_client(cfg, "memory search")?;
        let blob = embed_query(
            &client,
            "Given a question, retrieve passages that answer the question",
            &args.query,
        )
        .await?;
        sp.finish_and_clear();

        if mode == "semantic" {
            backend
                .search(&blob, &args.query, args.limit, as_of)
                .await
                .map_err(backend_err)?
        } else {
            backend
                .search_hybrid(&blob, &args.query, args.limit, as_of)
                .await
                .map_err(backend_err)?
        }
    };

    let mut notes = if args.expand_graph {
        let mut seen: std::collections::HashSet<i64> =
            notes.iter().filter_map(|n| n.id.as_i64()).collect();
        let mut expanded = notes;
        let mut neighbours = vec![];
        for n in &expanded {
            let Some(rowid) = n.id.as_i64() else {
                continue;
            };
            let (outgoing, incoming) = backend.get_edges(rowid).await.map_err(backend_err)?;
            for e in outgoing.iter().chain(incoming.iter()) {
                if e.kind != "relates_to" {
                    continue;
                }
                let neighbour_id = if e.from_id == rowid {
                    e.to_id
                } else {
                    e.from_id
                };
                if seen.insert(neighbour_id)
                    && let Some(nb) = backend.get(NoteId::from_i64(neighbour_id)).await?
                {
                    neighbours.push(nb);
                }
            }
        }
        expanded.extend(neighbours);
        expanded
    } else {
        notes
    };

    // Cross-project dep pass (ADR-003): append locked/cross-project decisions
    // and requirements from linked projects unless --local-only is set.
    // Dep stores are queried via text search (they have no embedder available
    // in the CLI path), filtered post-query to the `locked`/`cross-project`
    // tag set. Results are appended after local results per ADR-003 §6.
    if !args.local_only {
        let mut seen: std::collections::HashSet<(String, NoteId)> = Default::default();
        for n in &notes {
            seen.insert((String::new(), n.id.clone()));
        }
        let dep_notes =
            super::cross_project::collect_dep_cross_cutting(&index_db_path, &mut seen).await;
        notes.extend(dep_notes);
    }

    if notes.is_empty() {
        println!("No memory entries found.");
        return Ok(());
    }

    match crate::utils::effective_format(&args.format) {
        "json" => println!("{}", serde_json::to_string_pretty(&notes)?),
        _ => {
            for n in &notes {
                print_note_summary(n);
            }
        }
    }
    Ok(())
}
