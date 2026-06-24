use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct CheckArgs {
    /// Output format: text, json, or porcelain
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Path to the SQLite database (overrides auto-detect)
    #[arg(short, long)]
    pub db: Option<PathBuf>,

    /// List the stale file paths (one per line) in addition to the summary
    #[arg(long)]
    pub files: bool,

    /// Machine-readable output (deprecated — use --format porcelain)
    #[arg(long, hide = true)]
    pub porcelain: bool,
}

use crate::{
    capability,
    config::{Config, resolve_db},
    storage::{Database, open_memory_backend},
    utils::{format_age, worktree_modified_files},
};

pub async fn check(args: CheckArgs, cfg: Config) -> Result<()> {
    let db_path = resolve_db(args.db.as_deref(), &cfg.db_path);
    if !db_path.exists() {
        anyhow::bail!(
            "No index found (checked current directory and parents).\n\
             Run `spelunk index <path>` first."
        );
    }

    let db = Database::open(&db_path)?;
    let stored = db.all_file_hashes()?;

    let mut stale: Vec<String> = Vec::new();

    // Check every indexed file against its current on-disk hash.
    for (path, stored_hash) in &stored {
        match std::fs::read(path) {
            Ok(bytes) => {
                let current = format!("{}", blake3::hash(&bytes));
                if current != *stored_hash {
                    stale.push(path.clone());
                }
            }
            Err(_) => {
                // File deleted since last index.
                stale.push(path.clone());
            }
        }
    }

    let effective = if args.porcelain {
        "porcelain"
    } else {
        crate::utils::effective_format(&args.format)
    };
    let fresh = stale.is_empty();
    let last_indexed: Option<i64> = db.stats().ok().and_then(|s| s.last_indexed);

    if effective == "porcelain" {
        let last_ts = last_indexed.unwrap_or(0);
        println!(
            "stale={} total={} last_indexed={}",
            stale.len(),
            stored.len(),
            last_ts
        );
        if args.files {
            for p in &stale {
                println!("{p}");
            }
        }
    } else if effective == "json" {
        let tier = capability::get_tier(&cfg).await;
        let (server_reachable, server_url_val) = match tier {
            capability::Tier::Server { url, .. } => (true, serde_json::Value::String(url.clone())),
            capability::Tier::Offline => (
                false,
                cfg.server_url
                    .as_deref()
                    .map(|u| serde_json::Value::String(u.to_string()))
                    .unwrap_or(serde_json::Value::Null),
            ),
        };
        let mem_path = resolve_db(args.db.as_deref(), &cfg.db_path).with_file_name("memory.db");
        let memory_backend_kind = open_memory_backend(&cfg, &mem_path, None)
            .await
            .ok()
            .map(|b| b.backend_kind())
            .unwrap_or("sqlite");
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "fresh": fresh,
                "indexed_files": stored.len(),
                "stale_files": stale.len(),
                "stale": stale,
                "last_indexed_at": last_indexed,
                "server_reachable": server_reachable,
                "server_url": server_url_val,
                "memory_backend": memory_backend_kind,
            }))?
        );
    } else if fresh {
        println!("Index is up to date. ({} files indexed)", stored.len());
    } else {
        println!("{} file(s) changed since last index:", stale.len());
        for p in &stale {
            println!("  {p}");
        }
        println!("\nRun `spelunk index .` to update.");
    }

    // Show server status line (text mode only).
    //
    // We probe the tier and key off `tier.is_server()` rather than
    // `cfg.server_url.is_some()`: with loopback auto-discovery (spelunk#316) a
    // server can be reachable even when no `server_url` is configured, and the
    // old guard silently omitted that auto-discovered server from the output.
    // When offline we still want a status line iff a URL was explicitly set
    // (so the user sees the "unreachable" hint); we don't nag when nothing was
    // configured and no local server was found.
    if effective == "text" || effective == "porcelain" {
        let tier = capability::get_tier(&cfg).await;
        if tier.is_server() || cfg.server_url.is_some() {
            match tier {
                capability::Tier::Server { url, caps, .. } => {
                    let features: Vec<&str> = [
                        caps.search_semantic.then_some("semantic search"),
                        caps.explore.then_some("explore"),
                        caps.plan.then_some("plan"),
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    let feature_str = if features.is_empty() {
                        "memory sync".to_string()
                    } else {
                        features.join(", ")
                    };
                    println!("Server:  {url}  \x1b[32m✓\x1b[0m  ({feature_str} available)");
                }
                capability::Tier::Offline => {
                    let url = cfg.server_url.as_deref().unwrap_or("?");
                    println!("Server:  {url}  \x1b[31m✗\x1b[0m  unreachable — offline mode");
                }
            }
        }
    }

    // Show active intent entries (text mode only; silently skip if memory unavailable).
    if effective == "text" || effective == "porcelain" {
        let mem_path = resolve_db(args.db.as_deref(), &cfg.db_path).with_file_name("memory.db");
        if let Ok(backend) = open_memory_backend(&cfg, &mem_path, None).await
            && let Ok(intents) = backend.list(Some("intent"), 20, false, None).await
            && !intents.is_empty()
        {
            println!("Active agent sessions:");
            for n in &intents {
                let age = format_age(n.created_at);
                if n.linked_files.is_empty() {
                    println!("  · \"{}\"  ({})", n.title, age);
                } else {
                    println!(
                        "  · \"{}\"  linked: {}  ({})",
                        n.title,
                        n.linked_files.join(", "),
                        age
                    );
                }
            }

            // File overlap warning: compare intent linked_files with worktree changes.
            let modified = worktree_modified_files();
            if !modified.is_empty() {
                let intent_files: std::collections::HashSet<String> = intents
                    .iter()
                    .flat_map(|n| n.linked_files.iter().cloned())
                    .collect();

                for file in &modified {
                    if intent_files.contains(file) {
                        println!("⚠  Overlap: {file} is listed in an active intent");
                    }
                }
            }
        }
    }

    if !fresh {
        std::process::exit(1);
    }
    Ok(())
}
