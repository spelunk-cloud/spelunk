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

    /// Max concurrent embedding requests (default: 32)
    #[arg(long, default_value = "32")]
    pub batch_size: usize,

    /// Force full re-index (ignore change detection)
    #[arg(long)]
    pub force: bool,

    /// Backfill token_count for all existing chunks and exit (useful for upgrading old indexes)
    #[arg(long)]
    pub recount: bool,

    /// Skip LLM summary generation even when llm_model is configured
    #[arg(long)]
    pub no_summaries: bool,

    /// Number of chunks to send to the LLM per summary request (default: 10)
    #[arg(long, default_value = "10")]
    pub summary_batch_size: usize,

    /// Internal: run only phases 3-5 (graph rank, summaries).
    /// Used by the background process spawned after a large foreground index.
    #[arg(long = "_background-phases", hide = true, default_value_t = false)]
    pub background_phases: bool,

    /// Detach immediately: re-exec spelunk in the background and return.
    /// Useful in git hooks so the hook does not block the git process.
    #[arg(long, default_value_t = false)]
    pub detach: bool,
}

use crate::{capability, config::Config, registry::Registry, storage::Database};

mod embed_phase;
mod mentions;
mod parse_phase;
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
        run_background_phases(&args, &cfg, &db, &root_canonical, &db_path).await?;
        return Ok(());
    }

    let mp = MultiProgress::new();

    // ── Phase 1: parse + store chunks ────────────────────────────────────────
    let result = parse_phase::run_parse_phase(&root_canonical, &db, &args, &mp)?;
    if result.removed > 0 {
        eprintln!("Removed {} stale file(s) from index.", result.removed);
    }

    // ── Phase 2: embed chunks (Tier 1 only) ─────────────────────────────────
    let tier = capability::get_tier(&cfg).await;

    if result.chunk_ids_and_texts.is_empty() {
        let stats = db.stats()?;
        println!(
            "Index: {} files, {} chunks, {} embeddings (nothing new to process)",
            stats.file_count, stats.chunk_count, stats.embedding_count
        );
        return Ok(());
    }

    if tier.is_server() {
        embed_phase::run_embed_phase(
            result.chunk_ids_and_texts,
            &db,
            &cfg,
            tier,
            &project_root,
            &mp,
        )
        .await?;
    } else if cfg.server_url.is_some() {
        eprintln!(
            "Warning: spelunk-server at {} is unreachable — skipping embedding phase.",
            cfg.server_url.as_deref().unwrap_or("?")
        );
        eprintln!(
            "Chunks are indexed for text/ast-grep search. Re-run `spelunk index` when the server is back to add embeddings."
        );
    } else {
        eprintln!(
            "Note: configure server_url in ~/.config/spelunk/config.toml to enable semantic search."
        );
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
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.arg("index");
        cmd.arg(&args.path);
        cmd.arg("--_background-phases");
        if let Some(db_arg) = &args.db {
            cmd.args(["--db", &db_arg.to_string_lossy()]);
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if cmd.spawn().is_ok() {
            return Ok(());
        }
        // Fall through and run phases 3-5 inline as fallback.
        tracing::warn!("failed to spawn background indexer; running inline");
    }

    run_phases_3_to_5(&args, &cfg, &db, &root_canonical, &db_path).await
}

// ── Phases 3–5 (shared between inline and background-phases mode) ─────────────

async fn run_phases_3_to_5(
    args: &IndexArgs,
    cfg: &Config,
    db: &Database,
    root_canonical: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<()> {
    // Phase 3: PageRank
    eprintln!("Computing graph rank…");
    let edges = db.graph_edges_all()?;
    if !edges.is_empty() {
        let pr_scores = crate::indexer::pagerank::compute_pagerank(&edges, 20, 0.85);
        let named_chunks = db.chunks_with_names()?;
        let updates: Vec<(i64, f32)> = named_chunks
            .into_iter()
            .filter_map(|(id, name)| name.and_then(|n| pr_scores.get(&n).copied().map(|s| (id, s))))
            .collect();
        if !updates.is_empty() {
            db.update_graph_ranks(&updates)?;
        }
    }

    // Phase 4: LLM summaries — spawn a background thread so the caller
    // returns immediately. The thread opens its own DB connection because
    // `Database` (rusqlite::Connection) is not Send.
    let no_summaries = args.no_summaries;
    let summary_batch_size = args.summary_batch_size;
    let summary_cfg = cfg.clone();
    let summary_db_path = db_path.to_path_buf();
    eprintln!("Generating summaries in background\u{2026}");
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        match rt {
            Ok(rt) => rt.block_on(async move {
                match crate::storage::Database::open(&summary_db_path) {
                    Ok(bg_db) => {
                        if let Err(e) = summaries::generate_summaries(
                            no_summaries,
                            summary_batch_size,
                            &summary_cfg,
                            &bg_db,
                        )
                        .await
                        {
                            eprintln!("summary error: {e}");
                        }
                    }
                    Err(e) => eprintln!("summary error: {e}"),
                }
            }),
            Err(e) => eprintln!("summary error: could not build runtime: {e}"),
        }
    });

    // Phase 5: convention extraction (heuristic, no LLM).
    eprintln!("Extracting conventions\u{2026}");
    match crate::conventions::run_extraction(db) {
        Ok(records) => {
            if !records.is_empty() {
                eprintln!("Conventions: {} record(s) detected.", records.len());
            }
        }
        Err(e) => tracing::warn!("convention extraction failed (non-fatal): {e}"),
    }

    // Register / update this project in the global registry.
    if let Ok(reg) = Registry::open() {
        let db_canonical = spelunk_core::utils::canonicalize(db_path);
        if let Err(e) = reg.register(root_canonical, &db_canonical) {
            tracing::warn!("registry update failed: {e}");
        }
    }
    Ok(())
}

async fn run_background_phases(
    args: &IndexArgs,
    cfg: &Config,
    db: &Database,
    root_canonical: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<()> {
    run_phases_3_to_5(args, cfg, db, root_canonical, db_path).await
}
