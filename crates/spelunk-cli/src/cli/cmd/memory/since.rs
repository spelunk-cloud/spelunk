use anyhow::{Context, Result};
use serde::Deserialize;

use super::MemorySinceArgs;
use crate::{capability, config::Config};

/// Wire type that matches the server's `ServerNote` JSON schema.
#[derive(Debug, serde::Serialize, Deserialize)]
struct NoteResponse {
    id: i64,
    kind: String,
    title: String,
    body: String,
    tags: Vec<String>,
    linked_files: Vec<String>,
    created_at: i64,
    status: String,
    superseded_by: Option<i64>,
    #[serde(default)]
    distance: Option<f64>,
}

pub(super) async fn memory_since(
    args: MemorySinceArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    // ── Remote path ───────────────────────────────────────────────────────────
    let tier = capability::get_tier(cfg).await;
    if let (capability::Tier::Server { url: base_url, .. }, Some(project_id)) =
        (tier, cfg.project_id.as_deref())
    {
        let base_url = base_url.as_str();
        let limit = args.limit.min(500);
        let url = format!(
            "{}/v1/projects/{}/memory/since",
            base_url.trim_end_matches('/'),
            project_id,
        );
        let client = reqwest::Client::new();
        let mut req = client
            .get(&url)
            .query(&[("t", args.since.to_string()), ("limit", limit.to_string())]);
        if let Some(key) = cfg.server_key.as_deref() {
            req = req.header("Authorization", format!("Bearer {key}"));
        }
        let notes: Vec<NoteResponse> = req
            .send()
            .await
            .context("GET /memory/since")?
            .error_for_status()
            .context("server returned error for GET /memory/since")?
            .json()
            .await
            .context("parsing /memory/since response")?;

        print_notes(&notes, &args.format);
        return Ok(());
    }

    // ── Local path ────────────────────────────────────────────────────────────
    let backend = crate::storage::open_memory_backend(cfg, mem_path, backend_override).await?;
    let all = backend
        .list(None, args.limit, false, None)
        .await
        .context("listing local memory entries")?;

    // Filter client-side by created_at > since.
    let filtered: Vec<_> = all
        .into_iter()
        .filter(|n| n.created_at > args.since)
        .collect();

    match crate::utils::effective_format(&args.format) {
        "json" => println!("{}", serde_json::to_string_pretty(&filtered)?),
        "jsonl" => {
            for n in &filtered {
                println!("{}", serde_json::to_string(n)?);
            }
        }
        _ => {
            if filtered.is_empty() {
                println!("No memory entries found after timestamp {}.", args.since);
            } else {
                for n in &filtered {
                    super::print_note_summary(n);
                }
            }
        }
    }
    Ok(())
}

fn print_notes(notes: &[NoteResponse], format: &str) {
    match crate::utils::effective_format(format) {
        "json" => println!(
            "{}",
            serde_json::to_string_pretty(notes).unwrap_or_default()
        ),
        "jsonl" => {
            for n in notes {
                println!("{}", serde_json::to_string(n).unwrap_or_default());
            }
        }
        _ => {
            if notes.is_empty() {
                println!("No memory entries found.");
                return;
            }
            for n in notes {
                let dist = n
                    .distance
                    .map(|d| format!("  \x1b[2mdist: {d:.4}\x1b[0m"))
                    .unwrap_or_default();
                let archived_badge = if n.status == "archived" {
                    " \x1b[31m[archived]\x1b[0m"
                } else {
                    ""
                };
                println!(
                    "\x1b[1m#{id}\x1b[0m  \x1b[33m[{kind}]\x1b[0m  {title}{archived}{dist}",
                    id = n.id,
                    kind = n.kind,
                    title = n.title,
                    archived = archived_badge,
                );
                println!(
                    "     \x1b[2m{}\x1b[0m",
                    super::super::status::format_age(n.created_at)
                );
                if let Some(sup) = n.superseded_by {
                    println!("     \x1b[2msuperseded by #{sup}\x1b[0m");
                }
                if !n.tags.is_empty() {
                    println!("     tags: {}", n.tags.join(", "));
                }
                if !n.linked_files.is_empty() {
                    println!("     files: {}", n.linked_files.join(", "));
                }
                let preview: Vec<&str> = n.body.lines().take(2).collect();
                for line in &preview {
                    println!("     \x1b[2m{line}\x1b[0m");
                }
                if n.body.lines().count() > 2 {
                    println!("     \x1b[2m…\x1b[0m");
                }
                println!();
            }
        }
    }
}
