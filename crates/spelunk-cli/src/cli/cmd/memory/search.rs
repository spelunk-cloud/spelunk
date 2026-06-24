use anyhow::Result;

use super::super::helpers::{embed_query, require_server_client};
use super::MemorySearchArgs;
use super::{backend_err, parse_as_of, print_note_summary};
use crate::{capability, config::Config, storage::open_memory_backend};

pub(super) async fn memory_search(
    args: MemorySearchArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    let index_db_path = crate::config::resolve_db(None, &cfg.db_path);
    crate::storage::record_usage_at(&index_db_path, "memory search");

    // Discovery nudge: warn once when unimported server.db notes exist.
    super::reconcile::maybe_emit_nudge(mem_path, cfg);

    // Honor the auto-discovered server tier: loopback auto-discovery sets the
    // capability tier without populating `cfg.server_url`, so build an
    // effective config that fills in `server_url`/`project_id` from the tier
    // (mirrors `explore` — IMP-3 / spelunk#316). Falls back to `cfg` unchanged
    // when the tier isn't `Server` or `server_url` is already configured.
    let project_root = mem_path.parent().unwrap_or(mem_path);
    let tier = capability::get_tier(cfg).await;
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
        let mut seen: std::collections::HashSet<i64> = notes.iter().map(|n| n.id).collect();
        let mut expanded = notes;
        let mut neighbours = vec![];
        for n in &expanded {
            let (outgoing, incoming) = backend.get_edges(n.id).await.map_err(backend_err)?;
            for e in outgoing.iter().chain(incoming.iter()) {
                if e.kind != "relates_to" {
                    continue;
                }
                let neighbour_id = if e.from_id == n.id {
                    e.to_id
                } else {
                    e.from_id
                };
                if seen.insert(neighbour_id)
                    && let Some(nb) = backend.get(neighbour_id).await?
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
        let mut seen: std::collections::HashSet<(String, i64)> = Default::default();
        for n in &notes {
            seen.insert((String::new(), n.id));
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
