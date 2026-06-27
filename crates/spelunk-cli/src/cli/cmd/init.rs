use anyhow::{Context, Result};
use clap::Args;

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Also install the post-commit git hook
    #[arg(long)]
    pub hook: bool,

    /// Skip the initial index run
    #[arg(long)]
    pub no_index: bool,
}

use crate::{capability, config::Config, registry::Registry, storage::Database};

pub async fn init(args: InitArgs, cfg: Config) -> Result<()> {
    // ── 1. Detect project root ────────────────────────────────────────────────
    let cwd = std::env::current_dir()?;
    let git_root = find_git_root(&cwd);

    let project_root = match &git_root {
        Some(root) => root.clone(),
        None => {
            eprintln!(
                "Warning: not inside a git repository. Using current directory as project root."
            );
            cwd.clone()
        }
    };

    let project_name = project_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| project_root.to_string_lossy().into_owned());

    let db_path = project_root.join(".spelunk").join("index.db");

    // ── 2. Check if already initialised ──────────────────────────────────────
    let already_exists = db_path.exists();
    if already_exists {
        println!(
            "Note: spelunk is already initialised for '{}' (DB exists at {}).",
            project_name,
            db_path.display()
        );
        println!("Re-running init is safe — it will update the registry and optionally re-index.");
    }

    // ── 3. Register in global registry ───────────────────────────────────────
    let root_canonical = spelunk_core::utils::canonicalize(project_root.as_ref());

    if let Ok(reg) = Registry::open() {
        // We register with the expected db_path even if it doesn't exist yet —
        // the index step below will create it.
        let db_canonical = if db_path.exists() {
            spelunk_core::utils::canonicalize(db_path.as_ref())
        } else {
            db_path.clone()
        };
        if let Err(e) = reg.register(&root_canonical, &db_canonical) {
            eprintln!("Warning: registry update failed: {e}");
        }
    }

    // ── 4. Install hook (if requested) ───────────────────────────────────────
    let hook_status = if args.hook {
        match install_hook_for_init() {
            Ok(msg) => msg,
            Err(e) => format!("failed: {e}"),
        }
    } else {
        "not installed  (run `spelunk hooks install` to add)".to_string()
    };

    // ── 5. Run initial index (unless --no-index) ──────────────────────────────
    let (file_count, chunk_count) = if args.no_index {
        println!("Skipping index (--no-index). Run `spelunk index .` when ready.");
        // If the DB exists already, read its stats; otherwise report zeros.
        if db_path.exists() {
            match Database::open(&db_path) {
                Ok(db) => match db.stats() {
                    Ok(stats) => (stats.file_count, stats.chunk_count),
                    Err(_) => (0, 0),
                },
                Err(_) => (0, 0),
            }
        } else {
            (0, 0)
        }
    } else {
        // Delegate to the real index command logic.
        let index_args = super::index::IndexArgs {
            path: project_root.clone(),
            db: None,
            batch_size: 32,
            force: false,
            recount: false,
            no_summaries: true,
            summary_batch_size: 10,
            background_phases: false,
            detach: false,
        };
        super::index::index(index_args, cfg.clone()).await?;

        // Read fresh stats from the just-created DB.
        match Database::open(&db_path) {
            Ok(db) => match db.stats() {
                Ok(stats) => (stats.file_count, stats.chunk_count),
                Err(_) => (0, 0),
            },
            Err(_) => (0, 0),
        }
    };

    // ── 6. Write CLAUDE.md if missing ─────────────────────────────────────────
    let claude_md_path = project_root.join("CLAUDE.md");
    if !claude_md_path.exists() {
        let claude_md = format!(
            "# CLAUDE.md — {name}\n\
             \n\
             Developer guide for AI agents working on this codebase.\n\
             \n\
             ---\n\
             \n\
             ## Agent workflow\n\
             \n\
             This project is indexed with spelunk. Use it — don't just use Read/Grep/Glob.\n\
             \n\
             **At the start of every session:**\n\
             ```bash\n\
             spelunk check                 # verify index is fresh\n\
             spelunk context               # review handoffs, open questions, decisions, requirements\n\
             ```\n\
             \n\
             **Before reading any file, search first:**\n\
             ```bash\n\
             spelunk search \"<topic>\"      # find relevant chunks by meaning\n\
             spelunk graph <symbol>        # trace callers/callees when needed\n\
             ```\n\
             \n\
             **Store decisions as you make them:**\n\
             ```bash\n\
             spelunk memory add --kind decision --title \"...\" --body \"why, alternatives, tradeoffs\"\n\
             spelunk memory add --kind requirement --title \"...\"\n\
             spelunk memory add --kind note --title \"...\"      # surprising/non-obvious facts\n\
             ```\n\
             \n\
             **At the end of every session:**\n\
             ```bash\n\
             spelunk memory add --kind handoff --title \"Handoff: <summary>\" --body \"done, next, open\"\n\
             spelunk index .               # re-index after any commits\n\
             ```\n",
            name = project_name
        );
        if let Err(e) = std::fs::write(&claude_md_path, claude_md) {
            eprintln!("Warning: could not write CLAUDE.md: {e}");
        } else {
            println!("  CLAUDE.md written to {}", claude_md_path.display());
        }
    }

    // ── 7. Auto-spawn server (TTY only) or probe for a running server ─────────
    //
    // Interactive (stdin is a TTY): attempt to start the server so semantic
    // search works immediately. Non-interactive (CI / hook): probe only —
    // never auto-spawn; print a skip notice if offline.
    let server_line: Option<String> = {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            match super::server::ensure_server_running(7777).await {
                Ok((port, true)) => Some(format!(
                    "http://127.0.0.1:{port}  \x1b[32m✓\x1b[0m  (auto-started)"
                )),
                Ok((port, false)) => Some(format!("http://127.0.0.1:{port}  \x1b[32m✓\x1b[0m")),
                Err(e) => {
                    tracing::debug!("server auto-start skipped: {e}");
                    None
                }
            }
        } else {
            let tier = capability::get_tier(&cfg).await;
            match tier {
                capability::Tier::Server { url, .. } => Some(format!("{url}  \x1b[32m✓\x1b[0m")),
                capability::Tier::Offline => {
                    Some("[server not running — semantic search skipped]".to_string())
                }
            }
        }
    };

    // ── 8. Print success summary ──────────────────────────────────────────────
    println!();
    println!("spelunk initialised for {}", project_name);
    println!();
    println!("  Index:   {} files, {} chunks", file_count, chunk_count);
    println!("  DB:      {}", db_path.display());
    println!("  Hook:    {}", hook_status);
    if let Some(line) = server_line {
        println!("  Server:  {line}");
    }
    println!();
    println!("Next steps:");
    println!("  spelunk search \"your query\"");
    println!("  spelunk context");

    Ok(())
}

/// Walk up from `start` to find the nearest `.git` directory.
/// Returns the directory containing `.git`, not the `.git` directory itself.
fn find_git_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Install the git post-commit hook, returning a short status string.
fn install_hook_for_init() -> Result<String> {
    // Re-use the hook installation logic from hooks.rs by calling the same
    // underlying helper used there: replicate it inline to avoid making private
    // functions pub, keeping the hook module self-contained.
    let cwd = std::env::current_dir().context("getting current directory")?;
    let git_dir = gix::discover(&cwd)
        .context("not inside a git repository")?
        .git_dir()
        .to_path_buf();
    let hooks_dir = git_dir.join("hooks");
    std::fs::create_dir_all(&hooks_dir)?;
    let hook_path = hooks_dir.join("post-commit");

    if hook_path.exists() {
        let existing = std::fs::read_to_string(&hook_path)?;
        if existing.contains("spelunk post-commit hook") {
            return Ok(format!("already installed at {}", hook_path.display()));
        }
        anyhow::bail!(
            "a post-commit hook already exists at {} and was not installed by spelunk; \
             merge manually or remove it first",
            hook_path.display()
        );
    }

    const POST_COMMIT_HOOK: &str = r#"#!/bin/sh
# spelunk post-commit hook — installed by `spelunk hooks install`
# Keeps the spelunk index in sync and harvests memory from new commits.
# Silently skips if `spelunk` is not in PATH, so teammates without spelunk are unaffected.

if ! command -v spelunk >/dev/null 2>&1; then
  exit 0
fi

PROJECT_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0

spelunk index "$PROJECT_ROOT"
spelunk memory harvest --git-range HEAD~1..HEAD
"#;

    std::fs::write(&hook_path, POST_COMMIT_HOOK)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms)?;
    }

    Ok(format!("installed at {}", hook_path.display()))
}
