use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct SearchArgs {
    /// Natural language search query
    pub query: String,

    /// Number of results to return (max 100)
    #[arg(short, long, default_value = "10", conflicts_with = "budget")]
    pub limit: usize,

    /// Return best chunks fitting within this token budget (mutually exclusive with --limit)
    #[arg(long, conflicts_with = "limit")]
    pub budget: Option<usize>,

    /// Output format: text, json, or jsonl
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Enrich results with 1-hop call-graph neighbours (callers + callees)
    #[arg(short, long)]
    pub graph: bool,

    /// Maximum number of graph-expanded results to add (when --graph is set)
    #[arg(long, default_value = "10")]
    pub graph_limit: usize,

    /// Search mode: auto (default), text (FTS only), semantic/hybrid (LinearRAG), or ast-grep
    #[arg(long, default_value = "auto")]
    pub mode: String,

    /// Path to the SQLite database (overrides config)
    #[arg(short, long)]
    pub db: Option<PathBuf>,

    /// Skip the lightweight staleness probe (suppress stale-index warning)
    #[arg(long)]
    pub no_stale_check: bool,

    /// Search only the primary project index, skipping all linked project DBs
    #[arg(long)]
    pub local_only: bool,

    /// Search against this snapshot instead of the live index (full or short commit SHA)
    #[arg(long, value_name = "SHA")]
    pub as_of: Option<String>,
}

use super::helpers::{project_display_name, require_server_client};
use super::ui::{print_results_text, spinner};
use crate::{
    capability,
    config::Config,
    embeddings::vec_to_blob,
    registry::{Project, resolve_project_context},
    search::{SearchResult, rag},
    storage::Database,
};

pub async fn search(args: SearchArgs, cfg: Config) -> Result<()> {
    let mode = args.mode.as_str();
    let auto_mode = mode == "auto";

    // ── No-index / no-embedder fast path ─────────────────────────────────────
    // When the user hasn't asked for a specific mode (auto) and either the
    // index is missing or the embedder is unavailable, silently fall back to
    // ast-grep — mirroring the `graph --live` pattern.
    if auto_mode {
        let db_result = resolve_project_and_deps(args.db.as_ref(), &cfg);
        if db_result.is_err() {
            // No index found — fall back to ast-grep silently.
            return search_live(
                &args.query,
                &args.format,
                std::path::Path::new("."),
                args.limit,
            );
        }
        // Index exists but embedder may not be available. We'll check below
        // after attempting to load it; for now, fall through to normal path.
    }

    let (db_path, dep_projects) = resolve_project_and_deps(args.db.as_ref(), &cfg)?;
    crate::storage::record_usage_at(&db_path, "search");

    // Honor the capability tier: when the server was auto-discovered via the
    // loopback probe, `cfg.server_url` is unset; fill it in from the tier so
    // the inference client can be built (mirrors explore.rs / memory/search.rs,
    // see spelunk#316).
    let project_root = db_path.parent().unwrap_or(&db_path);
    let tier = capability::get_tier(&cfg).await;
    let cfg = tier.effective_config(&cfg, project_root);

    // Apply --local-only: discard linked deps.
    let dep_projects = if args.local_only || args.as_of.is_some() {
        vec![]
    } else {
        dep_projects
    };

    if !args.no_stale_check && args.as_of.is_none() {
        maybe_warn_stale(&db_path);
    }

    // --as-of: resolve commit SHA to snapshot id.
    let snapshot_id: Option<i64> = if let Some(ref sha_prefix) = args.as_of {
        let db = Database::open(&db_path)?;
        let snap = db
            .list_snapshots()?
            .into_iter()
            .find(|s| s.commit_sha.starts_with(sha_prefix.as_str()))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No snapshot found for '{}'. Provide a full or partial commit SHA from your indexed history.",
                    sha_prefix
                )
            })?;
        Some(snap.id)
    } else {
        None
    };

    // ── Tier-0 fall-through for explicit semantic/hybrid modes (#303-F2 / #323) ──
    //
    // When the user explicitly requests `--mode semantic` or `--mode hybrid`
    // but no server is reachable (Tier 0), automatically switch to FTS text
    // search. Under ADR-004 inference-only routing (no explicit `server_url`),
    // the fallback is silent — the user never configured a server, so there is
    // nothing to warn about. The notice is only printed when `server_url` was
    // explicitly set (the user expected a server and it is unreachable).
    //
    // The `auto` mode already degrades gracefully via the embed_query_vec error
    // path below — this guard handles the explicit-mode case only.
    // Snapshot searches are skipped: they require embeddings by definition.
    if (mode == "semantic" || mode == "hybrid") && snapshot_id.is_none() && !tier.is_server() {
        if cfg.server_url.is_some() {
            eprintln!("[server unreachable — using text search]");
        }
        let sp = spinner("Searching (text)…");
        let db = Database::open(&db_path)?;
        let results = db
            .search_text(&args.query, args.limit.min(100))
            .unwrap_or_default();
        sp.finish_and_clear();
        if results.is_empty() {
            println!("No results found.");
            return Ok(());
        }
        match crate::utils::effective_format(&args.format) {
            "json" => println!("{}", serde_json::to_string_pretty(&results)?),
            "jsonl" => {
                for item in &results {
                    println!("{}", serde_json::to_string(item)?);
                }
            }
            _ => print_results_text(&results),
        }
        return Ok(());
    }

    let mut results = if mode == "text" && snapshot_id.is_none() {
        // Text mode: FTS5 only, no embedding model required.
        let sp = spinner("Searching (text)…");
        let db = Database::open(&db_path)?;
        let res = db
            .search_text(&args.query, args.limit.min(100))
            .unwrap_or_default();
        sp.finish_and_clear();
        res
    } else if mode == "ast-grep" && snapshot_id.is_none() {
        // Explicit ast-grep mode: skip index entirely.
        return search_live(
            &args.query,
            &args.format,
            std::path::Path::new("."),
            args.limit,
        );
    } else {
        // semantic, hybrid, auto, or snapshot search: need an embedding via server.
        //
        // Use the dedicated POST /v1/projects/{id}/search endpoint (#322) when a
        // server is reachable — it applies the code-retrieval prefix server-side
        // and returns the query vector for CLI-side KNN.  This eliminates the
        // need for a local api_base_url / embedder in Tier-1 mode.
        let client_result = require_server_client(&cfg, "search");

        // Map auto/hybrid/semantic → server-side mode string.
        let server_mode = if auto_mode { "hybrid" } else { mode };

        let sp = spinner("Embedding query…");
        let query_vec_result = match client_result {
            Ok(client) => client
                .search_query(&args.query, server_mode, args.limit.min(100))
                .await
                .and_then(|opt_vec| {
                    opt_vec.ok_or_else(|| {
                        anyhow::anyhow!(
                            "server returned text mode for a semantic/hybrid request; \
                             use --mode text explicitly"
                        )
                    })
                }),
            Err(e) => Err(e),
        };

        // In auto mode, if the embedding call fails (e.g. embedder unreachable),
        // fall back to ast-grep silently.
        if auto_mode && query_vec_result.is_err() && snapshot_id.is_none() {
            sp.finish_and_clear();
            return search_live(
                &args.query,
                &args.format,
                std::path::Path::new("."),
                args.limit,
            );
        }

        let query_vec = query_vec_result.map_err(|e| {
            anyhow::anyhow!(
                "{e}\n\
                 No embedder configured. Run `spelunk index` with a server_url to enable \
                 semantic search, or use `--mode text` or `--mode ast-grep`."
            )
        })?;
        let query_blob = vec_to_blob(&query_vec);

        // Budget mode overfetches a candidate pool; limit is applied after packing.
        let fetch_limit = if let Some(budget) = args.budget {
            (budget / 50).clamp(20, 100)
        } else {
            args.limit.min(100)
        };

        sp.set_message("Searching…");
        let res = if let Some(snap_id) = snapshot_id {
            let db = Database::open(&db_path)?;
            db.search_snapshot(snap_id, &query_blob, fetch_limit)?
        } else {
            search_all_dbs_linearrag(
                &db_path,
                &dep_projects,
                &args.query,
                &query_vec,
                fetch_limit,
            )?
        };
        sp.finish_and_clear();

        // Auto mode: stale index + empty results → fall back to ast-grep silently.
        if auto_mode && res.is_empty() && !args.no_stale_check && index_is_stale(&db_path) {
            return search_live(
                &args.query,
                &args.format,
                std::path::Path::new("."),
                args.limit,
            );
        }

        res
    };

    if results.is_empty() {
        println!("No results found.");
        return Ok(());
    }

    // ── Graph-aware enrichment (primary DB only) ──────────────────────────────
    if args.graph
        && let Ok(primary_db) = Database::open(&db_path)
    {
        let seen_ids: std::collections::HashSet<i64> = results.iter().map(|r| r.chunk_id).collect();
        let names: Vec<&str> = results.iter().filter_map(|r| r.name.as_deref()).collect();

        if !names.is_empty()
            && let Ok(neighbor_ids) = primary_db.graph_neighbor_chunks(&names)
        {
            let new_ids: Vec<i64> = neighbor_ids
                .into_iter()
                .filter(|id| !seen_ids.contains(id))
                .take(args.graph_limit)
                .collect();

            if !new_ids.is_empty()
                && let Ok(mut extra) = primary_db.chunks_by_ids(&new_ids)
            {
                for r in &mut extra {
                    r.from_graph = true;
                }
                results.extend(extra);
            }
        }
    }

    // ── Budget-aware packing ──────────────────────────────────────────────────
    if let Some(budget) = args.budget {
        let mut remaining = budget;
        let mut packed: Vec<SearchResult> = Vec::new();
        for chunk in results {
            // Chunks with token_count = 0 (not yet backfilled) get an on-the-fly estimate.
            let tc = if chunk.token_count > 0 {
                chunk.token_count
            } else {
                crate::search::tokens::estimate_tokens(&chunk.content)
            };
            if tc <= remaining {
                remaining -= tc;
                packed.push(chunk);
            }
            if remaining < 10 {
                break;
            }
        }
        let tokens_used = budget - remaining;

        match crate::utils::effective_format(&args.format) {
            "json" => {
                #[derive(serde::Serialize)]
                struct BudgetResponse<'a> {
                    token_budget: usize,
                    tokens_used: usize,
                    tokens_remaining: usize,
                    results: &'a [SearchResult],
                }
                let resp = BudgetResponse {
                    token_budget: budget,
                    tokens_used,
                    tokens_remaining: remaining,
                    results: &packed,
                };
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            "jsonl" => {
                for item in &packed {
                    println!("{}", serde_json::to_string(item)?);
                }
            }
            _ => {
                print_results_text(&packed);
                println!("tokens used: {tokens_used}/{budget}");
            }
        }
        return Ok(());
    }

    match crate::utils::effective_format(&args.format) {
        "json" => println!("{}", serde_json::to_string_pretty(&results)?),
        "jsonl" => {
            for item in &results {
                println!("{}", serde_json::to_string(item)?);
            }
        }
        _ => print_results_text(&results),
    }

    Ok(())
}

/// Emit a staleness warning to stderr if the index appears out of date.
/// Silently skips if the DB doesn't exist or the probe returns an error.
pub(crate) fn maybe_warn_stale(db_path: &std::path::Path) {
    if !db_path.exists() {
        return;
    }
    if let Ok(db) = Database::open(db_path)
        && let Ok(report) = db.sample_staleness_check(20)
        && report.stale > 0
    {
        eprintln!(
            "warning: index may be stale ({}/{} sampled files changed). \
             Run `spelunk index .` to refresh.",
            report.stale, report.sampled
        );
    }
}

/// Return `true` when the index exists and the staleness probe indicates changed files.
pub(crate) fn index_is_stale(db_path: &std::path::Path) -> bool {
    if !db_path.exists() {
        return false;
    }
    Database::open(db_path)
        .ok()
        .and_then(|db| db.sample_staleness_check(20).ok())
        .is_some_and(|report| report.stale > 0)
}

/// Resolve the primary DB path and any dep projects via the registry.
/// Errors if the resolved DB does not exist on disk.
pub(crate) fn resolve_project_and_deps(
    explicit_db: Option<&std::path::PathBuf>,
    cfg: &Config,
) -> Result<(std::path::PathBuf, Vec<Project>)> {
    let resolved = resolve_project_context(explicit_db.map(|p| p.as_path()), &cfg.db_path)?;

    if !resolved.db_path.exists() {
        if explicit_db.is_some() {
            anyhow::bail!(
                "Database not found at '{}'. Run `spelunk index <path>` first.",
                resolved.db_path.display()
            );
        }
        anyhow::bail!(
            "No index found (checked current directory and parents).\n\
             Run `spelunk index <path>` inside your project first."
        );
    }

    // If the registry returned a project whose DB no longer exists, the
    // existence check above would have caught it via resolved.db_path.
    Ok((resolved.db_path, resolved.deps))
}

/// Annotate results with governing specs from the primary DB, and set
/// `project_name` / `project_path` on dep results.
fn annotate_dep_results(
    results: &mut [SearchResult],
    project_name: Option<String>,
    project_path: String,
) {
    for r in results.iter_mut() {
        r.project_name = project_name.clone();
        r.project_path = Some(project_path.clone());
    }
}

/// Populate `governing_specs` on each result using the primary DB.
fn annotate_specs(all: &mut [SearchResult], primary_db_path: &std::path::Path) {
    if let Ok(primary_db) = Database::open(primary_db_path) {
        let file_paths: Vec<String> = all.iter().map(|r| r.file_path.clone()).collect();
        if let Ok(all_specs) = primary_db.specs_for_files(&file_paths)
            && !all_specs.is_empty()
        {
            for result in all.iter_mut() {
                if let Ok(per) = primary_db.specs_for_files(std::slice::from_ref(&result.file_path))
                {
                    result.governing_specs = per.into_iter().map(|(p, _)| p).collect();
                }
            }
        }
    }
}

/// Search a primary DB and any dep projects, merge results by distance, return top `limit`.
/// LinearRAG search across a primary DB and any dep projects.
/// Runs LinearRAG on each DB independently and merges by score (distance).
pub(crate) fn search_all_dbs_linearrag(
    primary_db_path: &std::path::Path,
    dep_projects: &[Project],
    query: &str,
    query_vec: &[f32],
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let primary_db = Database::open(primary_db_path)?;
    let fetch = (limit * 2).max(limit + 10);
    let mut all = rag::linearrag_search(&primary_db, query_vec, query, fetch).unwrap_or_default();

    for dep in dep_projects {
        match Database::open(&dep.db_path) {
            Ok(dep_db) => match rag::linearrag_search(&dep_db, query_vec, query, fetch) {
                Ok(mut dep_results) => {
                    let name = project_display_name(&dep.root_path);
                    let root = dep.root_path.to_string_lossy().into_owned();
                    annotate_dep_results(&mut dep_results, Some(name), root);
                    all.append(&mut dep_results);
                }
                Err(e) => {
                    tracing::warn!(
                        "linearrag search failed on dep {}: {e}",
                        dep.db_path.display()
                    )
                }
            },
            Err(e) => tracing::warn!("could not open dep DB {}: {e}", dep.db_path.display()),
        }
    }

    // Sort by ascending distance (lower = better score in LinearRAG output).
    all.sort_by(|a, b| {
        a.distance
            .partial_cmp(&b.distance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut seen = std::collections::HashSet::new();
    all.retain(|r| seen.insert((r.file_path.clone(), r.start_line, r.end_line)));
    all.truncate(limit);

    annotate_specs(&mut all, primary_db_path);

    Ok(all)
}

/// Run an ast-grep pattern search for `query` over the working tree rooted at `root`.
///
/// This is the zero-infra fallback: no index and no embedder required.
/// It mirrors the `graph_live` pattern in `graph.rs`, but maps ast-grep matches
/// into `SearchResult` structs so the output shape is **identical** to the
/// regular/semantic search paths.
///
/// Field mapping from ast-grep JSON to `SearchResult`:
/// - `path`                → `file_path`
/// - `range.start.line`    → `start_line` (ast-grep 0-indexed → spelunk 1-indexed)
/// - `range.end.line`      → `end_line`   (same conversion)
/// - `text`                → `content`
/// - `language`            → `language`   (defaults to "unknown")
/// - `chunk_id`            → `-1` sentinel (not indexed)
/// - `node_type`           → `"live"`
/// - `distance`            → `0.0` (not meaningful for pattern search)
pub(crate) fn search_live(
    query: &str,
    format: &str,
    root: &std::path::Path,
    limit: usize,
) -> Result<()> {
    if std::process::Command::new("ast-grep")
        .arg("--version")
        .output()
        .is_err()
    {
        anyhow::bail!(
            "ast-grep not found. Install with: brew install ast-grep\n\
             (or: cargo install ast-grep --locked)\n\
             Run `spelunk index .` to enable index-backed search."
        );
    }

    let out = std::process::Command::new("ast-grep")
        .args(["run", "--pattern", query, "--json"])
        .arg(root)
        .output()?;

    if !out.status.success() && out.stdout.is_empty() {
        println!("No results found.");
        return Ok(());
    }

    let raw: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();

    // Map ast-grep matches to the canonical SearchResult shape so downstream
    // consumers (agents, benchmarks) see a consistent structure regardless of
    // which backend produced the results.
    let results: Vec<SearchResult> = raw
        .into_iter()
        .filter_map(|m| {
            let file_path = m["path"].as_str()?.to_string();
            let start_raw = m["range"]["start"]["line"].as_u64().unwrap_or(0) as usize;
            let end_raw = m["range"]["end"]["line"]
                .as_u64()
                .unwrap_or(start_raw as u64) as usize;
            let start_line = start_raw + 1;
            let end_line = end_raw + 1;
            let content = m["text"].as_str().unwrap_or("").to_string();
            let language = m["language"].as_str().unwrap_or("unknown").to_string();
            Some(SearchResult {
                chunk_id: -1,
                file_path,
                language,
                node_type: "live".to_string(),
                name: None,
                start_line,
                end_line,
                content,
                distance: 0.0,
                from_graph: false,
                governing_specs: vec![],
                token_count: 0,
                project_name: None,
                project_path: None,
                summary: None,
            })
        })
        .take(limit)
        .collect();

    if results.is_empty() {
        println!("No results found.");
        return Ok(());
    }

    match crate::utils::effective_format(format) {
        "json" => println!("{}", serde_json::to_string_pretty(&results)?),
        "jsonl" => {
            for item in &results {
                println!("{}", serde_json::to_string(item)?);
            }
        }
        _ => print_results_text(&results),
    }

    Ok(())
}
