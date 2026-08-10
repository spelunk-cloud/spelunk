use anyhow::{Context, Result};
use clap::Args;
use indicatif::MultiProgress;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct IndexArgs {
    /// Path to the codebase root to index
    pub path: PathBuf,

    /// Path to the SQLite database (overrides config)
    #[arg(short, long)]
    pub db: Option<PathBuf>,

    /// Cap on the embedding batch size: number of chunks sent per server
    /// request. The embed phase calibrates the actual per-request size from
    /// measured throughput (small batches on slow hardware, larger ones on
    /// fast hardware); this flag only sets the ceiling it may grow to. 0 (the
    /// default) leaves the ceiling at the server's own limit (256 chunks).
    #[arg(long, default_value = "0")]
    pub batch_size: usize,

    /// Force full re-index (ignore change detection)
    #[arg(long)]
    pub force: bool,

    /// Backfill token_count for all existing chunks and exit (useful for upgrading old indexes)
    #[arg(long)]
    pub recount: bool,

    /// Skip LLM summary generation even when server_url is configured
    #[arg(long)]
    pub no_summaries: bool,

    /// Number of chunks to send to the LLM per summary request (default: 10)
    #[arg(long, default_value = "10")]
    pub summary_batch_size: usize,

    /// Internal: run only phases 3-5 (graph rank, summaries).
    /// Used by the background process spawned after a large foreground index.
    #[arg(long = "_background-phases", hide = true, default_value_t = false)]
    pub background_phases: bool,

    /// Internal: skip parsing and run only the embed phase (plus phases 3-5)
    /// against the chunks already stored in the index. Used by the subprocess
    /// spawned for `--detach-embed`, which rebuilds the embed queue from the DB.
    #[arg(long = "_embed-phases", hide = true, default_value_t = false)]
    pub embed_phases: bool,

    /// Detach immediately: re-exec spelunk in the background and return.
    /// Useful in git hooks so the hook does not block the git process.
    #[arg(long, default_value_t = false)]
    pub detach: bool,

    /// Parse in the foreground, then hand the (usually long) embedding phase to
    /// a detached background process and return the prompt. Confirm completion
    /// later with `spelunk status` (it reports "embedding in progress" while the
    /// detached run has chunks left to embed).
    #[arg(long, default_value_t = false)]
    pub detach_embed: bool,

    /// The `--config` override this process itself resolved, if any. Not part
    /// of the `index` subcommand's own argv: `--config` is a global `Cli`-level
    /// flag, so `main` fills this in after parsing. Threaded through so the
    /// detached-child spawns below can forward the same override rather than
    /// have the child re-resolve the default config.
    #[arg(skip)]
    pub config_path: Option<PathBuf>,
}

use crate::{capability, config::Config, registry::Registry, storage::Database};

mod continuation;
mod crash_test_hook;
mod embed_phase;
mod mentions;
mod parse_phase;
mod phases;
mod run_lock;
mod summaries;
mod worktree;

pub async fn index(args: IndexArgs, cfg: Config) -> Result<()> {
    if args.detach {
        super::helpers::spawn_detached()?;
        return Ok(());
    }

    // Validate config: server_url requires project_id.
    cfg.validate()?;

    // Compile secret-scanning regexes once before the hot loop.
    crate::indexer::secrets::init();

    // If running inside a git linked worktree, resolve to the main worktree root
    // so all worktrees share one index without creating any symlink.
    let project_root = worktree::resolve_main_worktree_root(&args.path);

    // Default DB lives inside the project root, scoping the index to the project.
    let db_path = args
        .db
        .clone()
        .unwrap_or_else(|| project_root.join(".spelunk").join("index.db"));

    // Serialize whole `spelunk index` runs against this project: two
    // concurrent writers reproducibly corrupt index.db (see run_lock.rs doc
    // comment), so only one process may hold this at a time. `mut` + `Option`
    // because the two background-spawn sites below explicitly release it
    // before handing off to a continuation child (see their comments).
    let spelunk_dir = db_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| project_root.join(".spelunk"));
    let mut run_lock = match run_lock::try_acquire(&spelunk_dir)? {
        run_lock::LockOutcome::Acquired(lock) => Some(lock),
        run_lock::LockOutcome::HeldByOther { holder_pid } => {
            let who = holder_pid
                .map(|p| format!("pid {p}"))
                .unwrap_or_else(|| "another process".to_string());
            anyhow::bail!(
                "index already running ({who}) on this project, try again once it finishes"
            );
        }
    };

    let db = match Database::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            if args.force && db_path.exists() {
                tracing::warn!("corrupt index detected, deleting and rebuilding: {e}");
                std::fs::remove_file(&db_path)
                    .with_context(|| format!("removing corrupt index at {}", db_path.display()))?;
                Database::open(&db_path)?
            } else {
                return Err(e).with_context(|| {
                    format!(
                        "failed to open index at {}\n\
                         The database may be corrupt. Run with --force to delete it and rebuild from scratch:\n\
                         \n  spelunk index {} --force\n",
                        db_path.display(),
                        args.path.display(),
                    )
                });
            }
        }
    };

    // Keep the global registry in sync with the current location.
    {
        let root_now = spelunk_core::utils::canonicalize(args.path.as_ref());
        let db_now = spelunk_core::utils::canonicalize(db_path.as_ref());
        if let Ok(reg) = Registry::open() {
            let _ = reg.register(&root_now, &db_now);
        }
    }

    // --recount: backfill token_count for existing chunks, then exit.
    if args.recount {
        let updated = db.backfill_token_counts()?;
        println!("Backfilled token counts for {updated} chunk(s).");
        return Ok(());
    }

    // Canonicalise the root so symlinks don't create duplicate entries.
    let root_canonical = spelunk_core::utils::canonicalize(args.path.as_ref());

    // ── Background-phases mode ────────────────────────────────────────────────
    // When spawned as a background process (--_background-phases), skip phases
    // 1 & 2 (walk, parse, embed) which are already done, and run only phases 3–5.
    if args.background_phases {
        phases::run_background_phases(&args, &cfg, &db, &root_canonical, &db_path).await?;
        return Ok(());
    }

    // ── Embed-phases mode (detached embed) ────────────────────────────────────
    // Spawned by `--detach-embed` after the foreground process finished
    // parsing: skip phase 1 (parse) and rebuild the embed queue from the chunks
    // already stored in the DB, then run the embed phase and phases 3–5.
    if args.embed_phases {
        phases::run_embed_phases(&args, &cfg, &db, &project_root, &root_canonical, &db_path)
            .await?;
        return Ok(());
    }

    let mp = MultiProgress::new();

    // ── Phase 1: parse + store chunks ────────────────────────────────────────
    let result = parse_phase::run_parse_phase(&root_canonical, &db, &args, &mp, &cfg)?;
    if result.removed > 0 {
        eprintln!("Removed {} stale file(s) from index.", result.removed);
    }

    // ── Phase 2: embed chunks (Tier 1 only) ─────────────────────────────────
    //
    // `get_inference_tier` (not `get_tier`): local_first always prefers the
    // local loopback embedder for inference, even with an explicit
    // server_url set (2026-07-23 founder decision). `get_tier` alone would
    // probe the explicit server_url and hand its (possibly wrong) tier
    // straight to the batch-calibrated embed request loop below.
    let tier = capability::get_inference_tier(&cfg).await;

    if result.chunk_ids_and_texts.is_empty() {
        let stats = db.stats()?;
        println!(
            "Index: {} files, {} chunks, {} embeddings (nothing new to process)",
            stats.file_count, stats.chunk_count, stats.embedding_count
        );
        return Ok(());
    }

    // Embed only when the server's embedder is actually ready to serve
    // (`caps.index_embed` is advertised only in the `ready` state). When the
    // server is reachable but the model is still `loading` or has failed
    // (`unavailable`), skip embedding and print a visible, differentiated
    // notice rather than letting the embed request 503 out mid-index or
    // silently producing an unembedded index.
    let embed_ready = matches!(tier.caps(), Some(c) if c.index_embed);

    // ── Detached embed ────────────────────────────────────────────────────────
    // Parsing is done and the chunks are persisted; hand the (usually long)
    // embedding phase to a background process so the user regains the prompt
    // now. The subprocess (`--_embed-phases`) rebuilds the embed queue from the
    // DB, so nothing from `result` needs to cross the process boundary. Confirm
    // completion later with `spelunk status`.
    //
    // The spawn is gated on "worth waiting for" (ready OR still loading), not
    // on ready alone: the worker owns the readiness wait, and a fresh install
    // arrives here with the embedder still `loading`. Gating the spawn on
    // `embed_ready` is exactly the no-op that ships a permanently unembedded
    // index on a cold machine.
    if args.detach_embed && tier.is_server() && continuation::detach_embed_eligible(&tier) {
        let embed_log = continuation::background_log_path(&db_path);
        // Dropping the lock before spawning closes the corruption race (the
        // child never interleaves writes with us), but a third `spelunk
        // index` can still win the reacquire in the gap; `wait_for_holder_pid`
        // below confirms the spawned pid, specifically, becomes the holder
        // before we report success.
        drop(run_lock.take());
        crash_test_hook::pause_at("after_run_lock_drop", "embed");
        if let continuation::EmbedSpawn::Detached {
            log_in_use,
            child_pid,
        } = continuation::spawn_embed_subprocess(&args, embed_log.as_deref())?
        {
            let stats = db.stats()?;
            let pending = stats.chunk_count - stats.embedding_count;
            if run_lock::wait_for_holder_pid(
                &spelunk_dir,
                child_pid,
                continuation::HANDOFF_CONFIRM_TIMEOUT,
                continuation::HANDOFF_POLL_INTERVAL,
            ) {
                println!(
                    "Index: {} files, {} chunks. Embedding {} chunk(s) in the background\u{2026}",
                    stats.file_count, stats.chunk_count, pending,
                );
                if !embed_ready {
                    println!("The embedder is still loading; the background worker waits for it.");
                }
                println!("Run `spelunk status` to check progress.");
                if let Some(p) = log_in_use {
                    println!("  Log: {}", p.display());
                }
            } else {
                println!(
                    "Index: {} files, {} chunks. Started a background process to embed {} \
                     chunk(s), but another `spelunk index` run claimed this project's lock \
                     before it could take over.",
                    stats.file_count, stats.chunk_count, pending,
                );
                println!(
                    "Those chunks may be left unembedded. Run `spelunk index` again once the \
                     other run finishes to pick them up."
                );
            }
            return Ok(());
        }
        // Spawn failed: fall through to the inline path (embeds now if ready,
        // else prints the skip notice), unprotected by the run lock already
        // dropped above. Accepted: `Command::spawn` only fails on resource
        // exhaustion, and re-acquiring here would just move the same
        // race-vs-a-real-child problem rather than remove it.
    }

    if tier.is_server() && embed_ready {
        // Liveness marker so `spelunk status` from another terminal reports a
        // foreground embed as running rather than telling the user to resume.
        let worker_guard = super::embed_worker::EmbedWorkerGuard::acquire(&db, &db_path);
        embed_phase::run_embed_phase(
            result.chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            &project_root,
            args.batch_size,
            &mp,
        )
        .await?;
        drop(worker_guard);
    } else {
        phases::eprint_embed_skipped_notice(&tier, &cfg);
    }

    let stats = db.stats()?;
    println!(
        "\nIndex: {} files, {} chunks, {} embeddings",
        stats.file_count, stats.chunk_count, stats.embedding_count
    );

    // ── Background spawn for phases 3–5 ──────────────────────────────────────
    // When more than 100 files were newly indexed, detach phases 3-5 into a
    // background process so the user regains the prompt immediately.
    if result.indexed > 100 {
        eprintln!("Spawning background job for graph rank, spec discovery, and summaries\u{2026}");
        let log = continuation::background_log_path(&db_path);
        let mut cmd = continuation::build_detached_child_command(
            &std::env::current_exe()?,
            "--_background-phases",
            &args,
        );
        let in_use = continuation::redirect_to_background_log(&mut cmd, log.as_deref());
        if let Some(p) = in_use {
            eprintln!("  Log: {}", p.display());
        }
        // Release before spawning: closes the corruption race (see the
        // detach-embed site above for the full reasoning, including why this
        // alone does not guarantee the child specifically wins the reacquire).
        drop(run_lock.take());
        crash_test_hook::pause_at("after_run_lock_drop", "background_phases");
        match cmd.spawn() {
            Ok(child) => {
                if run_lock::wait_for_holder_pid(
                    &spelunk_dir,
                    child.id(),
                    continuation::HANDOFF_CONFIRM_TIMEOUT,
                    continuation::HANDOFF_POLL_INTERVAL,
                ) {
                    return Ok(());
                }
                eprintln!(
                    "Warning: another `spelunk index` run claimed this project's lock before \
                     the background job could take over; graph rank, spec discovery, and \
                     summaries were not completed. Run `spelunk index` again once the other run \
                     finishes."
                );
                return Ok(());
            }
            Err(e) => {
                // Fall through and run phases 3-5 inline as fallback,
                // unprotected by the run lock already dropped above (see the
                // detach-embed site's comment on this same tradeoff).
                tracing::warn!("failed to spawn background indexer; running inline: {e}");
            }
        }
    }

    phases::run_phases_3_to_5(&args, &cfg, &db, &root_canonical, &db_path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Minimal parser wrapper so we can exercise `IndexArgs` clap parsing in
    /// isolation without pulling in the whole top-level `Cli`.
    #[derive(clap::Parser, Debug)]
    struct TestCli {
        #[command(flatten)]
        index: IndexArgs,
    }

    #[test]
    fn batch_size_flag_is_captured() {
        // The user-supplied `--batch-size` must land in `IndexArgs.batch_size`,
        // which `index()` then threads into `run_embed_phase`. Before this fix
        // the value was parsed but never passed through (silent no-op).
        let cli =
            TestCli::try_parse_from(["spelunk", "some/path", "--batch-size", "16"]).expect("parse");
        assert_eq!(cli.index.batch_size, 16);
    }

    #[test]
    fn batch_size_defaults_to_zero_meaning_calibrated_with_no_user_cap() {
        // 0 means "no user-supplied cap" — the embed phase calibrates the
        // batch size from measured throughput up to the server's own 256-chunk
        // ceiling, rather than being pinned to a fixed default (see
        // `resolve_batch_ceiling` in embed_phase.rs).
        let cli = TestCli::try_parse_from(["spelunk", "some/path"]).expect("parse");
        assert_eq!(cli.index.batch_size, 0);
    }
}
