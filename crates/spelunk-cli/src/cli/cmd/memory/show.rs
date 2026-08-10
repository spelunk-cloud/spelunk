use anyhow::Result;

use super::super::color::{color_enabled, cprintln};
use super::super::status::format_age;
use super::MemoryShowArgs;
use crate::{
    config::Config,
    storage::{NoteId, open_memory_backend},
};

pub(super) async fn memory_show(
    args: MemoryShowArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    // Fold in any fetched teammate notes before the lookup, so an entry a
    // teammate just published is visible by id on the default path without a
    // re-init (ADR-077 D1).
    super::reconcile::refresh_read_path_from_git_notes(cfg, mem_path, backend_override).await;

    super::outbox::poll_and_apply(cfg, mem_path).await;

    let backend = open_memory_backend(cfg, mem_path, backend_override).await?;
    match backend.get(args.id.clone()).await? {
        None => anyhow::bail!("No memory entry with id {}.", args.id),
        Some(n) => match crate::utils::effective_format(&args.format) {
            "json" => println!("{}", serde_json::to_string_pretty(&n)?),
            _ => {
                cprintln!("\x1b[1m#{} [{}] {}\x1b[0m", n.id, n.kind, n.title);
                cprintln!("\x1b[2m{}\x1b[0m", format_age(n.created_at));
                let effective_valid_at = n.valid_at.unwrap_or(n.created_at);
                if n.valid_at.is_some() || effective_valid_at != n.created_at {
                    println!("valid_at:   {}", format_age(effective_valid_at));
                } else {
                    println!("valid_at:   (same as created_at)");
                }
                if let Some(inv) = n.invalid_at {
                    println!("invalid_at: {}", format_age(inv));
                }
                if !n.tags.is_empty() {
                    println!("tags: {}", n.tags.join(", "));
                }
                if !n.linked_files.is_empty() {
                    println!("files: {}", n.linked_files.join(", "));
                }
                if let Some(ref sha) = n.source_ref {
                    let short = &sha[..sha.len().min(8)];
                    // Two genuinely different strings (not just colored vs.
                    // plain), so this branches on the centralized color
                    // decision directly instead of going through `cprintln!`.
                    if color_enabled() {
                        println!(
                            "source:  \x1b[36mgit show {sha}\x1b[0m  \x1b[2m(SHA: {short})\x1b[0m"
                        );
                    } else {
                        println!("source:  {sha}");
                    }
                }
                println!();
                println!("{}", n.body);

                let (outgoing, incoming) = match n.id.as_i64() {
                    Some(rowid) => backend.get_edges(rowid).await?,
                    None => (vec![], vec![]),
                };
                if !outgoing.is_empty() || !incoming.is_empty() {
                    println!();
                    cprintln!("\x1b[2m── relationships ──\x1b[0m");
                    for e in &outgoing {
                        let label = match e.kind.as_str() {
                            "supersedes" => "\x1b[33m→ supersedes\x1b[0m",
                            "relates_to" => "\x1b[36m→ relates_to\x1b[0m",
                            "contradicts" => "\x1b[31m→ contradicts\x1b[0m",
                            _ => "→",
                        };
                        let target_title = backend
                            .get(NoteId::from_i64(e.to_id))
                            .await?
                            .map(|n| n.title)
                            .unwrap_or_else(|| "(deleted)".to_string());
                        cprintln!("  {label}  #{} {target_title}", e.to_id);
                    }
                    for e in &incoming {
                        let label = match e.kind.as_str() {
                            "supersedes" => "\x1b[33m← superseded by\x1b[0m",
                            "relates_to" => "\x1b[36m← related from\x1b[0m",
                            "contradicts" => "\x1b[31m← contradicted by\x1b[0m",
                            _ => "←",
                        };
                        let src_title = backend
                            .get(NoteId::from_i64(e.from_id))
                            .await?
                            .map(|n| n.title)
                            .unwrap_or_else(|| "(deleted)".to_string());
                        cprintln!("  {label}  #{} {src_title}", e.from_id);
                    }
                }
            }
        },
    }
    Ok(())
}
