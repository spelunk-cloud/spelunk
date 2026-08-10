use anyhow::Result;

use super::super::color::cprintln;
use super::{MemoryGraphArgs, backend_err};
use crate::{
    config::Config,
    storage::{NoteId, open_memory_backend},
};

pub(super) async fn memory_graph(
    args: MemoryGraphArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    let backend = open_memory_backend(cfg, mem_path, backend_override).await?;
    let root = backend
        .get(NoteId::from_i64(args.id))
        .await?
        .ok_or_else(|| anyhow::anyhow!("No memory entry with id {}.", args.id))?;

    let (outgoing, incoming) = backend.get_edges(args.id).await.map_err(backend_err)?;

    if crate::utils::effective_format(&args.format) == "json" {
        #[derive(serde::Serialize)]
        struct EdgeJson {
            from_id: i64,
            to_id: i64,
            kind: String,
        }
        #[derive(serde::Serialize)]
        struct GraphJson {
            id: NoteId,
            title: String,
            outgoing: Vec<EdgeJson>,
            incoming: Vec<EdgeJson>,
        }
        let graph = GraphJson {
            id: root.id.clone(),
            title: root.title.clone(),
            outgoing: outgoing
                .iter()
                .map(|e| EdgeJson {
                    from_id: e.from_id,
                    to_id: e.to_id,
                    kind: e.kind.clone(),
                })
                .collect(),
            incoming: incoming
                .iter()
                .map(|e| EdgeJson {
                    from_id: e.from_id,
                    to_id: e.to_id,
                    kind: e.kind.clone(),
                })
                .collect(),
        };
        println!("{}", serde_json::to_string_pretty(&graph)?);
        return Ok(());
    }

    cprintln!("\x1b[1m#{} [{}] {}\x1b[0m", root.id, root.kind, root.title);

    if outgoing.is_empty() && incoming.is_empty() {
        println!("  (no relationships)");
        return Ok(());
    }

    for e in &outgoing {
        let target_title = backend
            .get(NoteId::from_i64(e.to_id))
            .await?
            .map(|n| n.title)
            .unwrap_or_else(|| "(deleted)".to_string());
        let arrow = match e.kind.as_str() {
            "supersedes" => "\x1b[33m─[supersedes]→\x1b[0m",
            "relates_to" => "\x1b[36m─[relates_to]→\x1b[0m",
            "contradicts" => "\x1b[31m─[contradicts]→\x1b[0m",
            k => &format!("─[{k}]→"),
        };
        cprintln!("  {arrow}  #{} {target_title}", e.to_id);
    }
    for e in &incoming {
        let src_title = backend
            .get(NoteId::from_i64(e.from_id))
            .await?
            .map(|n| n.title)
            .unwrap_or_else(|| "(deleted)".to_string());
        let arrow = match e.kind.as_str() {
            "supersedes" => "\x1b[33m←[superseded by]\x1b[0m",
            "relates_to" => "\x1b[36m←[related from]\x1b[0m",
            "contradicts" => "\x1b[31m←[contradicted by]\x1b[0m",
            k => &format!("←[{k}]"),
        };
        cprintln!("  {arrow}  #{} {src_title}", e.from_id);
    }
    Ok(())
}
