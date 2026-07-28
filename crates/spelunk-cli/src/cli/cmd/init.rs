use super::color::cprintln;
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

    /// Explicit project slug, written to `.spelunk/config.toml`. Overrides the
    /// git-derived default; use it for projects without a git remote. Ignored
    /// when a `project_id` is already set in config.
    #[arg(long)]
    pub name: Option<String>,
}

use crate::{
    capability,
    config::Config,
    registry::Registry,
    storage::{Database, RewriteRefStatus, ensure_notes_rewrite_ref},
};

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

    let spelunk_dir = project_root.join(".spelunk");
    let db_path = spelunk_dir.join("index.db");
    let config_path = spelunk_dir.join("config.toml");

    // Ignore machine-specific SQLite (index.db/memory.db + their -wal/-shm
    // sidecars); config.toml is committed, so must not be listed here.
    // Idempotent: never clobbers a pre-existing file.
    write_spelunk_gitignore(&spelunk_dir);

    // Project slug: explicit --name, else derived (`host/owner/repo` when a git
    // remote exists, else `local/<blake3-hex>`). Written to config.toml, never
    // overwriting an existing project_id (no retroactive rename).
    let desired_slug = args
        .name
        .clone()
        .unwrap_or_else(|| spelunk_core::config::derive_project_id(&project_root));
    let (project_slug, wrote_slug) =
        match spelunk_core::config::write_project_slug(&config_path, &desired_slug) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Warning: could not write project slug to config: {e}");
                (desired_slug.clone(), false)
            }
        };

    // ── 2. Check if already initialised ──────────────────────────────────────
    let already_exists = db_path.exists();
    if already_exists {
        println!(
            "Note: spelunk is already initialised for '{}' (DB exists at {}).",
            project_slug,
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

    // ── 5. Auto-spawn server (TTY only) or probe for a running server ─────────
    //
    // Interactive (stdin is a TTY): attempt to start the server so semantic
    // search works immediately. Non-interactive (CI / hook): probe only,
    // never auto-spawn; print a skip notice if offline.
    //
    // This runs BEFORE the index step, and the index step below hands the
    // embed pass to the detached worker. The two are one change (ADR-070 D1):
    // starting the server first is what makes the detached embed reachable on
    // a fresh machine (otherwise the embed probes for a server this very
    // command has not started yet and silently ships a zero-embedding index),
    // and detaching is what keeps the reorder from holding the terminal
    // through the entire embed pass.
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
                    Some("[server not running - semantic search skipped]".to_string())
                }
            }
        }
    };

    // ── 6. Run initial index (unless --no-index) ──────────────────────────────
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
        // Delegate to the real index command logic. `detach_embed: true` hands
        // the (usually long) embed pass to the detached background worker, so
        // init returns the prompt after parsing instead of holding the
        // terminal through the whole embed (ADR-070 D1; on the profiled repo
        // that wait is ~103 minutes). The worker waits out a still-loading
        // embedder, so this holds on a cold machine whose server step 5 only
        // just started.
        let index_args = super::index::IndexArgs {
            path: project_root.clone(),
            db: None,
            batch_size: 32,
            force: false,
            recount: false,
            no_summaries: true,
            summary_batch_size: 10,
            background_phases: false,
            embed_phases: false,
            detach: false,
            detach_embed: true,
            // `init` has no global `--config` override of its own to forward
            // (it isn't threaded through `InitArgs`); a detached embed child
            // spawned from here falls back to the default config, same as
            // before this field existed.
            config_path: None,
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

    // ── 6b. Import git-notes memory into the project memory.db ────────────────
    // Entries recorded on `refs/notes/spelunk` before init (git-notes fallback
    // or write-through) are invisible to the SQLite-backed `memory list` until
    // imported. No enclosing git repo → nothing to import. Non-fatal: a failure
    // here must not sink init.
    let memory_line: Option<String> = if let Some(git_root) = git_root.as_ref() {
        let mem_path = spelunk_dir.join("memory.db");
        // Fold in anything a previous `git fetch` left on the tracking ref
        // before hydrating, so teammates' entries import too (ADR-069 D5).
        crate::storage::merge_tracking_notes(Some(git_root)).await;
        match super::memory::reconcile::import_git_notes_into_memory(git_root, &mem_path).await {
            Ok(0) => None,
            Ok(n) => Some(format!("imported {n} entries from git notes")),
            Err(e) => {
                tracing::warn!("git-notes memory import skipped (non-fatal): {e}");
                None
            }
        }
    } else {
        None
    };

    // ── 7. Configure git-notes refspec on `origin` (only inside a git repo) ───
    let notes_lines = if git_root.is_some() {
        configure_notes_refspec(&project_root).await
    } else {
        Vec::new()
    };

    // ── 8. Print success summary ──────────────────────────────────────────────
    println!();
    println!("spelunk initialised for {}", project_slug);
    println!();
    println!("  Index:   {} files, {} chunks", file_count, chunk_count);
    println!("  DB:      {}", db_path.display());
    if wrote_slug {
        println!(
            "  Project: {}  (written to {})",
            project_slug,
            config_path.display()
        );
    } else {
        println!(
            "  Project: {}  (from {})",
            project_slug,
            config_path.display()
        );
    }
    println!("  Hook:    {}", hook_status);
    if let Some(line) = memory_line {
        println!("  Memory:  {line}");
    }
    if let Some(line) = server_line {
        cprintln!("  Server:  {line}");
    }
    for line in &notes_lines {
        println!("  {line}");
    }
    println!();
    println!("Next steps:");
    println!("  spelunk search \"your query\"");
    println!("  spelunk context");

    Ok(())
}

/// Write `.spelunk/.gitignore` covering the machine-specific SQLite and log
/// files. The `*` glob covers the `-wal`/`-shm` sidecars. Created only when
/// absent so re-init never clobbers user edits; failures are non-fatal.
fn write_spelunk_gitignore(spelunk_dir: &std::path::Path) {
    let gitignore_path = spelunk_dir.join(".gitignore");
    if gitignore_path.exists() {
        return;
    }
    // Only machine-specific regenerated files are listed. config.toml is
    // committed, so it must stay out of this file.
    const GITIGNORE: &str = "# Machine-specific SQLite, regenerated by `spelunk index`.\n\
                             index.db*\n\
                             memory.db*\n\
                             # Diagnostics from the detached background index phases.\n\
                             *.log\n";
    if let Err(e) = std::fs::create_dir_all(spelunk_dir) {
        eprintln!("Warning: could not create {}: {e}", spelunk_dir.display());
        return;
    }
    if let Err(e) = std::fs::write(&gitignore_path, GITIGNORE) {
        eprintln!("Warning: could not write {}: {e}", gitignore_path.display());
    }
}

/// Configure the `origin` fetch refspec so teammates' `refs/notes/spelunk`
/// (spelunk's memory) travels on clone/fetch. Returns announce lines for the
/// init summary.
///
/// The destination is a **tracking** ref (`refs/notes/origin/spelunk`), never
/// the working ref. Fetching straight onto `refs/notes/spelunk` force-updates
/// it, silently replacing a local unpushed note with the remote's; and the
/// non-glob form makes plain `git fetch` exit 128 until someone pushes notes.
/// The glob tolerates the missing remote ref; only the tracking destination
/// stops the clobber. spelunk merges the tracking ref on its read paths
/// (ADR-069 D4/D5), so fetched notes stay visible without user action.
///
/// Push refspec is deliberately NOT set: any `remote.origin.push` value
/// overrides git's default branch push, so a normal `git push` would stop
/// pushing the current branch. Publishing rides the opt-in pre-push hook
/// instead, which this announces (ADR-069 D1/D3) because opt-in only works if it
/// is discoverable without reading the docs.
/// Also points `notes.rewriteRef` at spelunk's ref so memory survives history
/// rewrites. That half is independent of `origin`: rewrites are purely local,
/// so it runs even in a remote-less repo.
async fn configure_notes_refspec(project_root: &std::path::Path) -> Vec<String> {
    const FETCH_REFSPEC: &str = "+refs/notes/spelunk*:refs/notes/origin/spelunk*";

    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(project_root)
            .args(args)
            .output()
    };

    let mut lines = {
        // No `origin` remote → skip gracefully with the exact manual commands.
        let has_origin = git(&["remote", "get-url", "origin"])
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has_origin {
            vec![
                "Memory:  no 'origin' remote, so the notes refspec is not configured".to_string(),
                format!(
                    "         run later: git config --add remote.origin.fetch '{FETCH_REFSPEC}'"
                ),
            ]
        } else {
            // Idempotent: only `--add` when the identical refspec is not already present.
            let already = git(&["config", "--get-all", "remote.origin.fetch"])
                .ok()
                .filter(|o| o.status.success())
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .any(|l| l.trim() == FETCH_REFSPEC)
                })
                .unwrap_or(false);
            if already {
                vec!["Memory:  notes fetch refspec already configured on 'origin'".to_string()]
            } else {
                match git(&["config", "--add", "remote.origin.fetch", FETCH_REFSPEC]) {
                    Ok(o) if o.status.success() => vec![
                        "Memory:  configured notes fetch refspec on 'origin' (teammates' memory arrives on fetch)"
                            .to_string(),
                    ],
                    Ok(o) => vec![format!(
                        "Memory:  could not configure notes refspec: {}",
                        String::from_utf8_lossy(&o.stderr).trim()
                    )],
                    Err(e) => vec![format!("Memory:  could not configure notes refspec: {e}")],
                }
            }
        }
    };

    // Continuation lines: every branch above already opened a `Memory:` block.

    // Reading a teammate's memory is automatic (the refspec above plus the
    // read-path merge); publishing yours is opt-in, so say so unprompted.
    if super::hooks::pre_push_installed(project_root) {
        lines.push(
            "         pre-push hook installed: your memory publishes on `git push`".to_string(),
        );
    } else {
        lines.push(format!(
            "         your memory stays local until you install the pre-push hook: {}",
            super::hooks::PRE_PUSH_INSTALL_CMD
        ));
    }

    match ensure_notes_rewrite_ref(Some(project_root)).await {
        RewriteRefStatus::Configured => lines.push(
            "         configured notes.rewriteRef (memory survives `git commit --amend` and `git rebase`)"
                .to_string(),
        ),
        RewriteRefStatus::AlreadyCovered => {}
        RewriteRefStatus::Failed => {
            lines.push(
                "         could not set notes.rewriteRef; memory will not survive `git commit --amend` or `git rebase`"
                    .to_string(),
            );
            lines.push(
                "         run later: git config --add notes.rewriteRef refs/notes/spelunk"
                    .to_string(),
            );
        }
    }
    lines
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
///
/// Shares `hooks.rs`'s resolution logic rather than re-implementing it: a
/// second hardcoded `$GIT_DIR/hooks` here previously disagreed with
/// `core.hooksPath` in exactly the same way `spelunk hooks install` did.
fn install_hook_for_init() -> Result<String> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    match super::hooks::install_post_commit_hook(&cwd)? {
        super::hooks::Installed::Wrote(p) => Ok(format!("installed at {}", p.display())),
        super::hooks::Installed::Updated(p) => Ok(format!("updated at {}", p.display())),
        super::hooks::Installed::AlreadyPresent(p) => {
            Ok(format!("already installed at {}", p.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitignore_ignores_dbs_but_not_committed_files() {
        let tmp = tempfile::tempdir().unwrap();
        let spelunk_dir = tmp.path().join(".spelunk");

        write_spelunk_gitignore(&spelunk_dir);

        let body = std::fs::read_to_string(spelunk_dir.join(".gitignore")).unwrap();
        assert!(body.contains("index.db*"), "must ignore index.db*: {body}");
        assert!(
            body.contains("memory.db*"),
            "must ignore memory.db*: {body}"
        );
        assert!(
            !body.contains("config.toml"),
            "config.toml is committed, must not be ignored: {body}"
        );
    }

    #[test]
    fn gitignore_is_idempotent_and_preserves_user_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let spelunk_dir = tmp.path().join(".spelunk");
        std::fs::create_dir_all(&spelunk_dir).unwrap();
        let gitignore_path = spelunk_dir.join(".gitignore");
        std::fs::write(&gitignore_path, "custom-user-line\n").unwrap();

        write_spelunk_gitignore(&spelunk_dir);

        // A pre-existing file is never clobbered.
        let body = std::fs::read_to_string(&gitignore_path).unwrap();
        assert_eq!(body, "custom-user-line\n");
    }
}
