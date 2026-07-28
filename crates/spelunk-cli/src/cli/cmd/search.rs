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
}

use super::helpers::{project_display_name, require_server_client};
use super::ui::{print_results_text, spinner};
use crate::{
    capability,
    config::Config,
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
        let is_empty = db_result
            .as_ref()
            .ok()
            .and_then(|(db_path, _)| Database::open(db_path).and_then(|db| db.stats()).ok())
            .is_some_and(|s| s.chunk_count == 0);
        if db_result.is_err() || is_empty {
            // No index found (or the index has zero chunks) — fall back to
            // ast-grep silently, mirroring the missing-index case.
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

    // ADR-067 D1: index-free ast-grep touches no index and no global store, so
    // it is never project-gated — run it live before the fail-closed resolve.
    if mode == "ast-grep" {
        return search_live(
            &args.query,
            &args.format,
            std::path::Path::new("."),
            args.limit,
        );
    }

    let (db_path, dep_projects) = resolve_project_and_deps(args.db.as_ref(), &cfg)?;
    crate::storage::record_usage_at(&db_path, "search");

    // Explicit (non-auto) index-backed modes surface an empty index as an
    // actionable error instead of a silent "No results found." — ast-grep
    // returned above, and auto mode already degraded above.
    if !auto_mode
        && Database::open(&db_path)
            .and_then(|db| db.stats())
            .is_ok_and(|s| s.chunk_count == 0)
    {
        // Text mode has no index yet: point at the zero-setup modes that need
        // none (auto default, ast-grep) rather than only demanding an index.
        if mode == "text" {
            return Err(anyhow::anyhow!(
                "no FTS index yet for --mode text. Run `spelunk index <path>` first,\n\
                 or try `spelunk search \"...\" --mode ast-grep` (or omit --mode) for a\n\
                 zero-setup search."
            ));
        }
        return Err(crate::error::SearchError::EmptyIndex.into());
    }

    // Honor the capability tier: when the server was auto-discovered via the
    // loopback probe, `cfg.server_url` is unset; fill it in from the tier so
    // the inference client can be built (mirrors explore.rs / memory/search.rs,
    // see spelunk#316).
    let project_root = db_path.parent().unwrap_or(&db_path);
    // `get_inference_tier` (not `get_tier`): local_first always prefers the
    // local loopback embedder, even with an explicit server_url set
    // (2026-07-23 founder decision).
    let tier = capability::get_inference_tier(&cfg).await;
    let cfg = tier.effective_config(&cfg, project_root);

    // Apply --local-only: discard linked deps.
    let dep_projects = if args.local_only {
        vec![]
    } else {
        dep_projects
    };

    if !args.no_stale_check {
        maybe_warn_stale(&db_path);
    }

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
    if (mode == "semantic" || mode == "hybrid") && !tier.is_server() {
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

    // ── Embedding coverage is a first-class search input (chunk-shaped) ──────
    // A just-built, unembedded index is not stale, so the staleness probe can
    // never see the warmup window; the empty-result path below gates on
    // coverage instead. Invariant: `No results found.` is only ever printed
    // when the corpus that was searched was complete; whenever coverage is
    // partial, the output names what was incomplete. Notices go to stderr so
    // json/jsonl stdout stays machine-clean. No threshold: "incomplete" is a
    // fact, the percentage is reported, and the user judges.
    let coverage: Option<(i64, i64)> = if mode == "text" {
        // FTS is written at parse time and covers every chunk.
        None
    } else {
        Database::open(&db_path)
            .and_then(|db| db.stats())
            .ok()
            .map(|s| (s.embedding_count, s.chunk_count))
    };

    if let Some((embedded, total)) = coverage {
        match coverage_disposition(embedded, total, auto_mode) {
            CoverageDisposition::Complete => {}
            CoverageDisposition::PartialNotice => {
                eprintln!("{}", warmup_notice_partial(embedded, total));
            }
            CoverageDisposition::ZeroFallBack => {
                eprintln!("{}", warmup_notice_zero_auto(total));
                return search_live(
                    &args.query,
                    &args.format,
                    std::path::Path::new("."),
                    args.limit,
                );
            }
            CoverageDisposition::ZeroExplicitError => {
                return Err(anyhow::anyhow!(warmup_error_zero_explicit(mode, total)));
            }
        }
    }
    let coverage_partial = matches!(coverage, Some((e, t)) if e < t);
    // Set when an empty semantic result over a partial corpus was re-run as
    // text search: the FTS corpus is complete, so the plain empty-result line
    // becomes truthful again.
    let mut fell_back_to_text = false;

    let mut results = if mode == "text" {
        // Text mode: FTS5 only, no embedding model required.
        let sp = spinner("Searching (text)…");
        let db = Database::open(&db_path)?;
        let res = db
            .search_text(&args.query, args.limit.min(100))
            .unwrap_or_default();
        sp.finish_and_clear();
        res
    } else {
        // semantic, hybrid, or auto: need an embedding via server.
        //
        // Use the dedicated POST /v1/projects/{id}/search endpoint (#322) when a
        // server is reachable — it applies the code-retrieval prefix server-side
        // and returns the query vector for CLI-side KNN.  The server owns the
        // embedder; the CLI never embeds locally in Tier-1 mode.
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

        // In auto mode, if the embedding call fails (e.g. embedder unreachable or
        // still warming up), fall back to ast-grep. Print a visible, one-line
        // notice first so the degradation isn't silent and a downstream
        // "ast-grep not found" error isn't misattributed.
        if auto_mode && query_vec_result.is_err() {
            sp.finish_and_clear();
            eprint_semantic_unavailable_notice(&tier, &cfg);
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

        // Budget mode overfetches a candidate pool; limit is applied after packing.
        let fetch_limit = if let Some(budget) = args.budget {
            (budget / 50).clamp(20, 100)
        } else {
            args.limit.min(100)
        };

        sp.set_message("Searching…");
        let res = search_all_dbs_linearrag(
            &db_path,
            &dep_projects,
            &args.query,
            &query_vec,
            fetch_limit,
        )?;
        sp.finish_and_clear();

        // Auto mode, empty result set:
        //  - partial coverage → re-run as text search over the complete FTS
        //    corpus rather than reporting an absence KNN over a partial corpus
        //    cannot substantiate (the warmup notice already went to stderr);
        //  - full coverage → today's behaviour: stale index falls back to
        //    ast-grep.
        if auto_mode && res.is_empty() && coverage_partial {
            eprintln!("[no semantic results in the embedded portion; using text search]");
            fell_back_to_text = true;
            let db = Database::open(&db_path)?;
            db.search_text(&args.query, args.limit.min(100))
                .unwrap_or_default()
        } else if auto_mode && res.is_empty() && !args.no_stale_check && index_is_stale(&db_path) {
            return search_live(
                &args.query,
                &args.format,
                std::path::Path::new("."),
                args.limit,
            );
        } else {
            res
        }
    };

    if results.is_empty() {
        if coverage_partial && !fell_back_to_text {
            // Explicit semantic/hybrid over a partial corpus: the plain
            // absence claim is not substantiated, name the incompleteness.
            let (e, t) = coverage.unwrap_or((0, 0));
            println!(
                "No results found in the embedded portion of the index \
                 (searchable {e}/{t} chunks; the rest is not embedded yet)."
            );
        } else {
            println!("No results found.");
        }
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
    // ADR-067: without an explicit --db, refuse when there is no local
    // `.spelunk/` project rather than silently searching the global store. The
    // scoped path also wins over any stray global `index.db`.
    let project_db = match explicit_db {
        Some(_) => None,
        None => Some(crate::config::require_project_db(&cfg.db_path, false)?),
    };

    let resolved = resolve_project_context(explicit_db.map(|p| p.as_path()), &cfg.db_path)?;
    let db_path = project_db.unwrap_or(resolved.db_path);

    if !db_path.exists() {
        if explicit_db.is_some() {
            anyhow::bail!(
                "Database not found at '{}'. Run `spelunk index <path>` first.",
                db_path.display()
            );
        }
        anyhow::bail!(
            "No index found (checked current directory and parents).\n\
             Run `spelunk index <path>` inside your project first."
        );
    }

    // If the registry returned a project whose DB no longer exists, the
    // existence check above would have caught it via db_path.
    Ok((db_path, resolved.deps))
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

/// Run the zero-setup ("ast-grep") search for `query` over the working tree
/// rooted at `root`.
///
/// This is the zero-infra fallback: no index and no embedder required, and no
/// external `ast-grep` binary — matching runs in-process via `ast-grep-core`
/// (see `spelunk_core::search::live`). A structural pattern (with metavariables)
/// matches structurally; a plain string matches case-insensitively by substring
/// on identifier/text nodes, with a literal line scan beneath.
/// It mirrors the `graph_live` pattern in `graph.rs`, but maps matches into
/// `SearchResult` structs so the output shape is **identical** to the
/// regular/semantic search paths.
///
/// Field mapping from a `LiveMatch` to `SearchResult`:
/// - `file_path`  → `file_path`
/// - `start_line` → `start_line` (already 1-indexed by the matcher)
/// - `end_line`   → `end_line`
/// - `text`       → `content`
/// - `language`   → `language`
/// - `chunk_id`   → `-1` sentinel (not indexed)
/// - `node_type`  → `"live"`
/// - `distance`   → `0.0` (not meaningful for pattern search)
pub(crate) fn search_live(
    query: &str,
    format: &str,
    root: &std::path::Path,
    limit: usize,
) -> Result<()> {
    let matches = crate::search::live::search_live_query(query, root, limit);

    // Map structural matches to the canonical SearchResult shape so downstream
    // consumers (agents, benchmarks) see a consistent structure regardless of
    // which backend produced the results.
    let results: Vec<SearchResult> = matches
        .into_iter()
        .map(|m| SearchResult {
            chunk_id: -1,
            file_path: m.file_path,
            language: m.language,
            node_type: "live".to_string(),
            name: None,
            start_line: m.start_line,
            end_line: m.end_line,
            content: m.text,
            distance: 0.0,
            from_graph: false,
            governing_specs: vec![],
            token_count: 0,
            project_name: None,
            project_path: None,
            summary: None,
        })
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

/// What the embedding coverage of the index means for this search
/// (`spelunk search` warmup contract). Three coverage states by two mode
/// classes, exhaustively; there is deliberately no coverage threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoverageDisposition {
    /// Every chunk is embedded (or the index is empty, handled earlier):
    /// today's behaviour, no notice.
    Complete,
    /// `0 < coverage < 100`: run KNN and always emit the one-line warmup
    /// notice, in auto and explicit modes alike, so a thin result set is
    /// never mistaken for a complete one.
    PartialNotice,
    /// Zero coverage in `auto` mode: fall back to the live search, with a
    /// notice naming warmup as the reason.
    ZeroFallBack,
    /// Zero coverage in an explicit `semantic`/`hybrid` mode: an actionable
    /// error naming warmup and the resume command. Never `No results found.`.
    ZeroExplicitError,
}

/// The three-state coverage table (`0` / partial / `100`, by auto vs explicit
/// semantic/hybrid). `embedded >= total` covers the defensive over-count case.
fn coverage_disposition(embedded: i64, total: i64, auto_mode: bool) -> CoverageDisposition {
    if total <= 0 || embedded >= total {
        return CoverageDisposition::Complete;
    }
    if embedded <= 0 {
        if auto_mode {
            CoverageDisposition::ZeroFallBack
        } else {
            CoverageDisposition::ZeroExplicitError
        }
    } else {
        CoverageDisposition::PartialNotice
    }
}

/// One-line warmup notice for a partially-embedded corpus: carries the
/// coverage percentage AND its shape. The queue drains in priority order
/// (`graph_rank DESC, mtime DESC` — most-referenced code first, then most
/// recently modified), so a prefix is the most important/recent code, not a
/// sample across the repo: the user has a complete picture of the code most
/// likely to matter and a blind spot over the rest, and a bare percentage
/// would read as the opposite failure mode (a uniformly thinner picture of
/// everything).
fn warmup_notice_partial(embedded: i64, total: i64) -> String {
    let pct = if total > 0 {
        (embedded.max(0) as u64).saturating_mul(100) / total as u64
    } else {
        0
    };
    format!(
        "[warmup: searchable {embedded}/{total} chunks ({pct}%), front-loaded by importance \
         and recency; a missing result may mean \"not embedded yet\", not \"not in the \
         codebase\" (check `spelunk status`)]"
    )
}

/// Zero-coverage notice for `auto` mode, printed before the live-search
/// fallback: names warmup as the reason.
fn warmup_notice_zero_auto(total: i64) -> String {
    format!(
        "[semantic search is warming up: 0/{total} chunks embedded; using ast-grep. \
         Embeddings build in the background (check `spelunk status`)]"
    )
}

/// Zero-coverage error for explicit `semantic`/`hybrid`: actionable, naming
/// warmup and the resume command.
fn warmup_error_zero_explicit(mode: &str, total: i64) -> String {
    format!(
        "semantic search is still warming up: 0/{total} chunks are embedded, so a {mode} \
         search would search nothing.\n\
         Embeddings build in the background; check `spelunk status`. If no embed worker is \
         running, resume with `spelunk index .`.\n\
         Use `--mode text` or `--mode ast-grep` in the meantime."
    )
}

/// Build the one-line notice explaining why `auto`-mode search is falling back
/// from semantic to ast-grep, differentiating the cases the readiness contract
/// exposes.
///
/// Pure so it can be unit-tested without capturing stderr; `has_server_url` is
/// `cfg.server_url.is_some()`.
///
/// `remote_url` is `Some` when the probed server came from an explicit
/// `server_url` (not loopback auto-discovery). The unavailable-embedder
/// notice must then name that server instead of pointing at `spelunk server
/// logs`, which only reads the local auto-daemon's log and would show clean
/// logs for a failure that lives on the remote server.
///
/// `server_url` is `cfg.server_url` (used only for the offline case, where no
/// probe ever reached the point of populating `remote_url`): the configured
/// server never answered at all, so the notice names it directly and flags
/// that it overrides the auto-discovered local daemon. `is_windows` is
/// injected rather than read from `cfg!(windows)` inline so the function stays
/// pure and the platform-gated hint is unit-testable on any host.
fn semantic_unavailable_message(
    embedder_state: Option<capability::EmbedderState>,
    server_url: Option<&str>,
    remote_url: Option<&str>,
    is_windows: bool,
) -> String {
    use capability::EmbedderState;
    match embedder_state {
        Some(EmbedderState::Loading) => "[semantic search unavailable: model still warming up — \
             retry shortly (`spelunk server status`); using ast-grep]"
            .to_string(),
        Some(EmbedderState::Unavailable) => match remote_url {
            Some(url) => format!(
                "[semantic search unavailable: embedder failed to load on team server {url}; \
                 check that server's own logs; using ast-grep]"
            ),
            None => "[semantic search unavailable: embedder failed to load; \
                 see `spelunk server logs`; using ast-grep]"
                .to_string(),
        },
        Some(_) => "[semantic search unavailable on this server; using ast-grep]".to_string(),
        None => {
            if let Some(url) = server_url {
                let windows_hint = if is_windows {
                    " On Windows, allow the loopback listener through Defender Firewall."
                } else {
                    ""
                };
                format!(
                    "[no server reachable at {url} (the configured server_url, overriding the \
                     auto-discovered local daemon);{windows_hint} using ast-grep]"
                )
            } else {
                "[no server running — start one with `spelunk server start` to enable \
                 semantic search; using ast-grep]"
                    .to_string()
            }
        }
    }
}

/// Print the semantic-unavailable notice to stderr so structured
/// (`--json`/`--jsonl`) output on stdout stays clean.
fn eprint_semantic_unavailable_notice(tier: &capability::Tier, cfg: &Config) {
    eprintln!(
        "{}",
        semantic_unavailable_message(
            tier.embedder_state(),
            cfg.server_url.as_deref(),
            tier.explicit_remote_url(),
            cfg!(windows),
        )
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::EmbedderState;

    // ── coverage_disposition: the three-state warmup table, all six cells ──────

    #[test]
    fn coverage_zero_auto_falls_back_with_notice() {
        assert_eq!(
            coverage_disposition(0, 100, true),
            CoverageDisposition::ZeroFallBack
        );
    }

    #[test]
    fn coverage_zero_explicit_is_an_actionable_error() {
        assert_eq!(
            coverage_disposition(0, 100, false),
            CoverageDisposition::ZeroExplicitError
        );
    }

    #[test]
    fn coverage_partial_auto_runs_knn_with_notice() {
        assert_eq!(
            coverage_disposition(40, 100, true),
            CoverageDisposition::PartialNotice
        );
    }

    #[test]
    fn coverage_partial_explicit_runs_knn_with_the_same_notice() {
        assert_eq!(
            coverage_disposition(40, 100, false),
            CoverageDisposition::PartialNotice
        );
    }

    #[test]
    fn coverage_full_auto_is_todays_behaviour_no_notice() {
        assert_eq!(
            coverage_disposition(100, 100, true),
            CoverageDisposition::Complete
        );
    }

    #[test]
    fn coverage_full_explicit_is_todays_behaviour_no_notice() {
        assert_eq!(
            coverage_disposition(100, 100, false),
            CoverageDisposition::Complete
        );
    }

    #[test]
    fn coverage_has_no_threshold() {
        // "Incomplete" is a fact, not a tunable: 1 missing chunk out of 100k
        // still notices, and 1 embedded chunk out of 100k still serves KNN.
        assert_eq!(
            coverage_disposition(99_999, 100_000, true),
            CoverageDisposition::PartialNotice
        );
        assert_eq!(
            coverage_disposition(1, 100_000, false),
            CoverageDisposition::PartialNotice
        );
    }

    #[test]
    fn coverage_defensive_cases_are_complete() {
        // Empty index (handled by earlier guards) and an over-count must not
        // produce warmup output.
        assert_eq!(
            coverage_disposition(0, 0, true),
            CoverageDisposition::Complete
        );
        assert_eq!(
            coverage_disposition(120, 100, false),
            CoverageDisposition::Complete
        );
    }

    // ── warmup notices: percentage, shape, and actionability ───────────────────

    #[test]
    fn partial_notice_names_coverage_and_its_front_loaded_shape() {
        let n = warmup_notice_partial(11_813, 27_734);
        assert!(n.contains("11813/27734"), "labelled coverage: {n}");
        assert!(n.contains("42%"), "carries the percentage: {n}");
        assert!(
            n.contains("front-loaded by importance and recency"),
            "names the shape so a subsystem miss reads as a blind spot, not a thin sample: {n}"
        );
        assert!(n.contains("spelunk status"), "actionable: {n}");
    }

    /// Regression guard (spelunk-oss embed-queue reorder): the embed queue's
    /// `ORDER BY` changed from raw parse/insertion order (`c.id`) to
    /// `graph_rank DESC, mtime DESC, c.id` — the queue is no longer "the first
    /// N files walked" in any sense. The warmup notice's copy was corrected to
    /// "front-loaded by importance and recency" to match; this guards against
    /// any regression to the stale "indexing order" wording, which described a
    /// mechanism that no longer exists.
    ///
    /// The reorder does not affect the coverage/completeness contract (verified
    /// above: `coverage_disposition` only ever reads `(embedded, total)`
    /// counts, never queue order, so "ordering mistaken for completeness" does
    /// not regress) — this is purely about the notice's user-facing
    /// *explanation* of the ordering mechanism staying accurate.
    #[test]
    fn partial_notice_no_longer_claims_indexing_order_after_the_reorder() {
        let n = warmup_notice_partial(11_813, 27_734);
        assert!(
            !n.contains("indexing order"),
            "the embed queue is no longer ordered by parse/indexing order since the \
             recency+graph_rank reorder (ORDER BY c.graph_rank DESC, f.mtime DESC, c.id) — \
             this notice's copy is stale and describes a mechanism that no longer exists: {n}"
        );
    }

    #[test]
    fn zero_auto_notice_names_warmup_as_the_reason() {
        let n = warmup_notice_zero_auto(27_734);
        assert!(n.contains("warming up"));
        assert!(n.contains("0/27734"));
        assert!(n.contains("ast-grep"));
    }

    #[test]
    fn zero_explicit_error_names_warmup_and_the_resume_command() {
        let e = warmup_error_zero_explicit("semantic", 27_734);
        assert!(e.contains("warming up"));
        assert!(e.contains("spelunk index ."), "resume command: {e}");
        assert!(e.contains("--mode text"), "usable alternative: {e}");
        assert!(
            !e.contains("No results found"),
            "never the empty-result claim: {e}"
        );
    }

    // ── semantic_unavailable_message: auto-mode fallback notice (#5) ────────────

    #[test]
    fn notice_loading_advises_retry() {
        let msg = semantic_unavailable_message(
            Some(EmbedderState::Loading),
            Some("http://x:1"),
            None,
            false,
        );
        assert!(msg.contains("warming up"));
        assert!(msg.contains("ast-grep"));
    }

    #[test]
    fn notice_unavailable_loopback_points_at_logs() {
        // Loopback auto-discovery: the failing embedder IS the local daemon,
        // so `spelunk server logs` is the right place to look.
        let msg = semantic_unavailable_message(
            Some(EmbedderState::Unavailable),
            Some("http://x:1"),
            None,
            false,
        );
        assert!(msg.contains("failed to load"));
        assert!(msg.contains("spelunk server logs"));
    }

    #[test]
    fn notice_unavailable_remote_names_that_server_never_local_logs() {
        // Explicit server_url: `spelunk server logs` reads the LOCAL daemon's
        // log, which is clean when the failure lives on the team server. The
        // notice must name the probed server instead.
        let msg = semantic_unavailable_message(
            Some(EmbedderState::Unavailable),
            Some("http://x:1"),
            Some("https://team.example:7777"),
            false,
        );
        assert!(msg.contains("failed to load"));
        assert!(msg.contains("https://team.example:7777"), "got: {msg}");
        assert!(
            !msg.contains("spelunk server logs"),
            "must not point a remote failure at local logs: {msg}"
        );
    }

    #[test]
    fn notice_no_server_names_the_configured_url_on_windows() {
        // Offline (no reachable server) but a server_url was configured: name
        // the actual URL that was attempted, note it overrides the
        // auto-discovered local daemon, and mention the Windows cause only
        // when actually running on Windows.
        let msg = semantic_unavailable_message(None, Some("https://team.example:7777"), None, true);
        assert!(msg.contains("https://team.example:7777"), "got: {msg}");
        assert!(msg.contains("no server reachable"));
        assert!(msg.contains("Firewall"));
        assert!(msg.contains("overriding"), "got: {msg}");
    }

    #[test]
    fn notice_no_server_with_configured_url_omits_windows_hint_elsewhere() {
        // Same offline+configured-url case, but not on Windows: the
        // Defender-specific hint must not appear.
        let msg =
            semantic_unavailable_message(None, Some("https://team.example:7777"), None, false);
        assert!(msg.contains("https://team.example:7777"), "got: {msg}");
        assert!(msg.contains("no server reachable"));
        assert!(!msg.contains("Firewall"), "got: {msg}");
    }

    #[test]
    fn notice_no_server_no_url_suggests_starting_one() {
        let msg = semantic_unavailable_message(None, None, None, false);
        assert!(msg.contains("spelunk server start"));
    }

    #[test]
    fn notice_is_never_silent() {
        // Every case yields a visible, non-empty notice — the whole point of #5
        // is that the fallback is no longer silent.
        for state in [
            Some(EmbedderState::Loading),
            Some(EmbedderState::Unavailable),
            Some(EmbedderState::Ready),
            Some(EmbedderState::Disabled),
            Some(EmbedderState::Unknown),
            None,
        ] {
            for url in [Some("http://x:1"), None] {
                for remote_url in [None, Some("https://team.example:7777")] {
                    for is_windows in [true, false] {
                        assert!(
                            !semantic_unavailable_message(state, url, remote_url, is_windows)
                                .is_empty()
                        );
                    }
                }
            }
        }
    }

    /// **Gap check:** `/v1/health`'s embedder state stays
    /// `ready` while the embed admission queue sheds a `/search` call with
    /// 429 (health is a separate, unaffected endpoint) - so `search_query`'s
    /// failure surfaces here with `embedder_state = Some(Ready)`. There is no
    /// dedicated "busy" case; it falls into the generic `Some(_)` arm. The
    /// 2026-07-22 live-repro comment on this task asked for the client to
    /// distinguish "server busy (embedding)" from "unavailable" so it can say
    /// "busy, retrying" - that request is NOT implemented: a saturated-but-
    /// healthy server still gets labeled "unavailable," not "busy." This is a
    /// real, verified gap (not a regression from this change, and not fixed
    /// by it), left as a follow-up rather than guessed at here since the
    /// exact wording and whether to plumb the 429/EmbedderBusy reason through
    /// `search_query` is a product/UX call, not a mechanical hardening fix.
    #[test]
    fn notice_ready_embedder_still_says_unavailable_not_busy() {
        let msg = semantic_unavailable_message(Some(EmbedderState::Ready), None, None, true);
        assert!(
            msg.contains("unavailable"),
            "documents current behavior: a Ready-but-saturated embedder gets the generic \
             'unavailable' notice, not a busy/retry-specific one: {msg}"
        );
        assert!(
            !msg.to_lowercase().contains("busy") && !msg.to_lowercase().contains("retry"),
            "if this ever starts mentioning busy/retry, a Busy-aware notice has been added - \
             update this test to lock in the new behavior instead: {msg}"
        );
    }
}
