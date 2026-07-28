use anyhow::Result;
use clap::Args;
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct GraphArgs {
    /// Symbol name or file path to look up in the graph
    pub symbol: String,

    /// Filter to a specific edge kind: imports, calls, extends, implements
    #[arg(long)]
    pub kind: Option<String>,

    /// Output format: text, json, or jsonl
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Path to the SQLite database (overrides config)
    #[arg(short, long)]
    pub db: Option<PathBuf>,

    /// Skip the lightweight staleness probe (suppress stale-index warning)
    #[arg(long)]
    pub no_stale_check: bool,

    /// Skip the index and scan live files with in-process structural matching
    #[arg(long)]
    pub live: bool,
}

use super::color::cprintln;
use super::helpers::open_project_db;
use super::search::{index_is_stale, maybe_warn_stale};
use crate::config::Config;
use crate::storage::GraphEdge;

pub fn graph(args: GraphArgs, cfg: Config) -> Result<()> {
    let symbol = &args.symbol;
    let is_file_query = symbol.contains('/')
        || symbol.contains('\\')
        || symbol.ends_with(".rs")
        || symbol.ends_with(".py")
        || symbol.ends_with(".go")
        || symbol.ends_with(".java")
        || symbol.ends_with(".ts")
        || symbol.ends_with(".js");

    // --live forces ast-grep regardless of index state (symbol queries only).
    if args.live && !is_file_query {
        return graph_live(symbol, &args.format, &args.kind, Path::new("."));
    }

    // ADR-067: resolves fail-closed (no global fallback) via open_project_db.
    let db_result = open_project_db(args.db.as_deref(), &cfg.db_path);

    // No local project or no index — run the live ast-grep graph for symbol
    // queries (mirrors search's auto/live posture) rather than reading the
    // machine-global store. File queries need the index, so they propagate the
    // refuse error below.
    if db_result.is_err() && !is_file_query {
        return graph_live(symbol, &args.format, &args.kind, Path::new("."));
    }

    let (db_path, db) = db_result?;

    if !args.no_stale_check {
        maybe_warn_stale(&db_path);
    }

    let mut edges = if is_file_query {
        db.edges_for_file(symbol)?
    } else {
        db.edges_for_symbol(symbol)?
    };

    // Stale index + empty results → fall back to ast-grep for symbol queries.
    if edges.is_empty() && !is_file_query && !args.no_stale_check && index_is_stale(&db_path) {
        eprintln!("note: index is stale — falling back to live ast-grep scan");
        return graph_live(symbol, &args.format, &args.kind, Path::new("."));
    }

    // Symbol query with nothing for this symbol: an index that holds no graph
    // edges at all auto-falls-back to the live scan (same posture as the
    // no-project case); a populated graph that simply lacks this symbol gets an
    // unambiguous message. Never suggest `init` here — already initialized.
    if edges.is_empty() && !is_file_query {
        if db.has_any_graph_edges()? {
            println!(
                "No calls to '{symbol}' found in the indexed graph. Try 'spelunk graph {symbol} --live' for a structural scan."
            );
            return Ok(());
        }
        return graph_live(symbol, &args.format, &args.kind, Path::new("."));
    }

    if let Some(kind) = &args.kind {
        edges.retain(|e| e.kind == *kind);
    }

    if edges.is_empty() {
        println!("No graph edges found for '{symbol}'.");
        return Ok(());
    }

    match crate::utils::effective_format(&args.format) {
        "json" => println!("{}", serde_json::to_string_pretty(&edges)?),
        "jsonl" => {
            for edge in &edges {
                println!("{}", serde_json::to_string(edge)?);
            }
        }
        _ => print_edges(&edges, symbol),
    }

    Ok(())
}

/// Run a structural ("ast-grep") caller search for `symbol` over the working
/// tree rooted at `root`, in-process via `ast-grep-core` (no external binary).
fn graph_live(symbol: &str, format: &str, kind_filter: &Option<String>, root: &Path) -> Result<()> {
    // Match call sites of `symbol`, e.g. `foo(...)`. The matcher walks the tree
    // per-language; the pattern only compiles/matches where it's syntactically
    // valid, so languages where `symbol($$$)` is not a valid call are skipped.
    let pattern = format!("{symbol}($$$)");
    // Graph fallback is unranked, so an ample cap keeps it useful without
    // unbounded memory on huge trees.
    let matches = crate::search::live::search_live_matches(&pattern, root, 1000);

    let mut edges: Vec<GraphEdge> = matches
        .into_iter()
        .map(|m| GraphEdge {
            source_file: m.file_path,
            source_name: None,
            target_name: symbol.to_string(),
            kind: "calls".to_string(),
            // `LiveMatch` reports 1-indexed lines; the previous subprocess path
            // emitted ast-grep's 0-indexed `range.start.line`. Preserve the
            // original 0-indexed semantics for the graph edge `line` field.
            line: m.start_line.saturating_sub(1),
        })
        .collect();

    if let Some(k) = kind_filter {
        edges.retain(|e| &e.kind == k);
    }

    if edges.is_empty() {
        // Disambiguate a leaf/no-call symbol (source present) from an empty tree
        // (e.g. an umbrella repo with uninitialized submodules). The empty-tree
        // branch never suggests `init`; only the source-present branch does, since
        // the live scan is structurally call-syntax-only and an empty result there
        // means "no bare calls", not "unused".
        if crate::search::live::has_scannable_source(root) {
            println!(
                "No call-site invocations of '{symbol}' found (live structural scan matches '{symbol}(...)' calls only)."
            );
            println!(
                "Class, constant, association, and receiver-method references never take that form. Run 'spelunk init' to build an index with imports/extends/implements plus call edges that surface them."
            );
        } else {
            println!(
                "No scannable source files under this directory (live scan). Check you're in a populated subdirectory, or that git submodules are initialized."
            );
        }
        return Ok(());
    }

    match crate::utils::effective_format(format) {
        "json" => println!("{}", serde_json::to_string_pretty(&edges)?),
        "jsonl" => {
            for edge in &edges {
                println!("{}", serde_json::to_string(edge)?);
            }
        }
        _ => {
            cprintln!("\x1b[1mIncoming to '{symbol}' (live scan — no ranking):\x1b[0m");
            for e in &edges {
                let loc = e.source_name.as_deref().unwrap_or(&e.source_file);
                cprintln!(
                    "  \x1b[36m{}\x1b[0m  {}  \x1b[2m({}:{})\x1b[0m",
                    e.kind,
                    e.source_file,
                    loc,
                    e.line
                );
            }
        }
    }

    Ok(())
}

fn print_edges(edges: &[GraphEdge], query: &str) {
    let outgoing: Vec<_> = edges
        .iter()
        .filter(|e| e.source_name.as_deref() == Some(query) || e.source_file == query)
        .collect();
    let incoming: Vec<_> = edges.iter().filter(|e| e.target_name == query).collect();
    let other: Vec<_> = edges
        .iter()
        .filter(|e| {
            e.source_name.as_deref() != Some(query)
                && e.source_file != query
                && e.target_name != query
        })
        .collect();

    if !outgoing.is_empty() {
        cprintln!("\x1b[1mOutgoing from '{query}':\x1b[0m");
        for e in &outgoing {
            let loc = e.source_name.as_deref().unwrap_or(&e.source_file);
            cprintln!(
                "  \x1b[33m{}\x1b[0m  {}  \x1b[2m({}:{})\x1b[0m",
                e.kind,
                e.target_name,
                loc,
                e.line
            );
        }
        println!();
    }
    if !incoming.is_empty() {
        cprintln!("\x1b[1mIncoming to '{query}':\x1b[0m");
        for e in &incoming {
            let loc = e.source_name.as_deref().unwrap_or(&e.source_file);
            cprintln!(
                "  \x1b[36m{}\x1b[0m  {}  \x1b[2m({}:{})\x1b[0m",
                e.kind,
                e.source_file,
                loc,
                e.line
            );
        }
        println!();
    }
    if !other.is_empty() {
        cprintln!("\x1b[1mRelated edges:\x1b[0m");
        for e in &other {
            let loc = e.source_name.as_deref().unwrap_or(&e.source_file);
            cprintln!(
                "  {} -- \x1b[33m{}\x1b[0m --> {}  \x1b[2m({}:{})\x1b[0m",
                loc,
                e.kind,
                e.target_name,
                e.source_file,
                e.line
            );
        }
    }
}
