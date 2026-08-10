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
    config::{Config, require_project_db},
    storage::{Database, open_memory_backend},
    utils::{format_age, worktree_modified_files},
};

/// Emit one human-readable diagnostic line for `check` (server reachability,
/// active intents, overlap warnings).
///
/// In `porcelain` mode stdout is reserved for the stable `key=value` contract,
/// so these lines go to **stderr** instead — still visible to a human watching
/// the terminal, but never mixed into the stdout a script parses with
/// `while read -r line`. ANSI color is stripped there, since stderr consumers
/// don't opt into the `--color` policy (the Unicode glyphs themselves are kept,
/// as they carry meaning, not color). In text mode the line goes to stdout,
/// honoring the color policy exactly as before.
fn emit_check_diagnostic(line: &str, porcelain: bool) {
    if porcelain {
        eprintln!("{}", spelunk_core::utils::strip_ansi(line));
    } else if crate::cli::cmd::color::color_enabled() {
        println!("{line}");
    } else {
        println!("{}", spelunk_core::utils::strip_ansi(line));
    }
}

pub async fn check(args: CheckArgs, cfg: Config) -> Result<()> {
    // ADR-067: fail closed in an un-init'd dir rather than checking the
    // machine-global index.db. Explicit `--db` bypasses the project gate.
    let db_path = match args.db.as_deref() {
        Some(p) => p.to_path_buf(),
        None => require_project_db(&cfg.db_path, false)?,
    };
    if !db_path.exists() {
        anyhow::bail!(
            "No index found (checked current directory and parents).\n\
             Run `spelunk index <path>` first."
        );
    }

    let db = Database::open(&db_path)?;

    // Indexed paths are stored relative to the project root, which for an
    // in-project command is the cwd.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let report = db.staleness_report(&cwd, None)?;
    let total = report.sampled;
    let stale = report.stale_paths;

    let effective = if args.porcelain {
        "porcelain"
    } else {
        crate::utils::effective_format(&args.format)
    };
    let fresh = stale.is_empty();
    let last_indexed: Option<i64> = report.last_indexed_at;

    if effective == "porcelain" {
        let last_ts = last_indexed.unwrap_or(0);
        println!(
            "stale={} total={} last_indexed={}",
            stale.len(),
            total,
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
        let mem_path = db_path.with_file_name("memory.db");
        let memory_backend_kind = open_memory_backend(&cfg, &mem_path, None)
            .await
            .ok()
            .map(|b| b.backend_kind())
            .unwrap_or("sqlite");
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "fresh": fresh,
                "indexed_files": total,
                "stale_files": stale.len(),
                "stale": stale,
                "last_indexed_at": last_indexed,
                "server_reachable": server_reachable,
                "server_url": server_url_val,
                "memory_backend": memory_backend_kind,
            }))?
        );
    } else if fresh {
        println!("Index is up to date. ({total} files indexed)");
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
        let porcelain = effective == "porcelain";
        let tier = capability::get_tier(&cfg).await;
        if tier.is_server() || cfg.server_url.is_some() {
            let line = match tier {
                capability::Tier::Server { url, caps, .. } => {
                    let features: Vec<&str> = [
                        caps.search_semantic.then_some("semantic search"),
                        caps.explore.then_some("explore"),
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    let feature_str = if features.is_empty() {
                        "memory sync".to_string()
                    } else {
                        features.join(", ")
                    };
                    format!("Server:  {url}  \x1b[32m✓\x1b[0m  ({feature_str} available)")
                }
                capability::Tier::Offline => {
                    let url = cfg.server_url.as_deref().unwrap_or("?");
                    let label = match capability::explicit_probe_failure() {
                        Some(capability::ConnFailure::Tls(cause)) => {
                            format!("reachable, but TLS trust failed: {cause}")
                        }
                        _ => "unreachable, offline mode".to_string(),
                    };
                    format!("Server:  {url}  \x1b[31m✗\x1b[0m  {label}")
                }
            };
            emit_check_diagnostic(&line, porcelain);
        }
    }

    // Show active intent entries (text mode only; silently skip if memory unavailable).
    if effective == "text" || effective == "porcelain" {
        let porcelain = effective == "porcelain";
        let mem_path = db_path.with_file_name("memory.db");
        if let Ok(backend) = open_memory_backend(&cfg, &mem_path, None).await
            && let Ok(intents) = backend.list(Some("intent"), 20, false, None).await
            && !intents.is_empty()
        {
            emit_check_diagnostic("Active agent sessions:", porcelain);
            for n in &intents {
                let age = format_age(n.created_at);
                let line = if n.linked_files.is_empty() {
                    format!("  · \"{}\"  ({})", n.title, age)
                } else {
                    format!(
                        "  · \"{}\"  linked: {}  ({})",
                        n.title,
                        n.linked_files.join(", "),
                        age
                    )
                };
                emit_check_diagnostic(&line, porcelain);
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
                        emit_check_diagnostic(
                            &format!("⚠  Overlap: {file} is listed in an active intent"),
                            porcelain,
                        );
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
