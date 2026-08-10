use anyhow::Result;

use super::super::color::cprintln;
use super::super::status::format_age;
use super::{MemoryTimelineArgs, backend_err};
use crate::{capability, config::Config, storage::open_memory_backend};

pub(super) async fn memory_timeline(
    args: MemoryTimelineArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    // Honor the auto-discovered server tier (IMP-3 / spelunk#316): see
    // `memory_search` for rationale — loopback auto-discovery sets the
    // capability tier without populating `cfg.server_url`.
    let project_root = mem_path.parent().unwrap_or(mem_path);
    // `get_inference_tier` (not `get_tier`): local_first always prefers the
    // local loopback embedder, even with an explicit server_url set
    // (2026-07-23 founder decision).
    let tier = capability::get_inference_tier(cfg).await;
    let eff_cfg = tier.effective_config(cfg, project_root);
    let cfg = &eff_cfg;

    super::outbox::poll_and_apply(cfg, mem_path).await;

    // Timeline is a local, always-available capability: it filters the topic
    // through the same no-server full-text path as `memory search --mode text`
    // (the local backend matches on `query` and ignores `query_blob`), so no
    // query embedding — and thus no running inference server — is required.
    // The empty blob is only consulted by the remote backend, which embeds the
    // `query` text server-side anyway.
    let backend = open_memory_backend(cfg, mem_path, backend_override).await?;
    let notes = backend
        .search_timeline(&[], &args.query, args.limit)
        .await
        .map_err(backend_err)?;

    if notes.is_empty() {
        println!("No memory entries found for topic: {}", args.query);
        return Ok(());
    }

    match crate::utils::effective_format(&args.format) {
        "json" => println!("{}", serde_json::to_string_pretty(&notes)?),
        _ => {
            cprintln!("\x1b[1mTimeline: {}\x1b[0m\n", args.query);
            let (active, superseded): (Vec<_>, Vec<_>) =
                notes.iter().partition(|n| n.status == "active");

            if !active.is_empty() {
                cprintln!("\x1b[32mActive\x1b[0m");
                for n in &active {
                    print_timeline_entry(n);
                }
            }
            if !superseded.is_empty() {
                if !active.is_empty() {
                    println!();
                }
                cprintln!("\x1b[2mSuperseded / Archived\x1b[0m");
                for n in &superseded {
                    print_timeline_entry(n);
                }
            }
        }
    }
    Ok(())
}

fn print_timeline_entry(n: &crate::storage::memory::Note) {
    let ts = n.valid_at.unwrap_or(n.created_at);
    let marker = if n.status == "active" { "●" } else { "○" };
    let sup = if let Some(id) = &n.superseded_by {
        format!(" → #{id}")
    } else {
        String::new()
    };
    let short_ref = n
        .source_ref
        .as_deref()
        .map(|s| format!(" \x1b[2m({})\x1b[0m", &s[..s.len().min(7)]))
        .unwrap_or_default();
    let inv = n
        .invalid_at
        .map(|t| format!(" \x1b[2m– {}\x1b[0m", format_age(t)))
        .unwrap_or_default();
    cprintln!(
        " {marker} \x1b[36m{}\x1b[0m  \x1b[1m[{}] #{} {}\x1b[0m{sup}{short_ref}{inv}",
        format_age(ts),
        n.kind,
        n.id,
        n.title
    );
    let excerpt: String = n.body.chars().take(80).collect();
    let ellipsis = if n.body.len() > 80 { "…" } else { "" };
    cprintln!("     \x1b[2m{excerpt}{ellipsis}\x1b[0m");
}
