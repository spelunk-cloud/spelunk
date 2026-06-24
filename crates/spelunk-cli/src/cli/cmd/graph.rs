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

    /// Skip the index and scan live files with ast-grep (requires ast-grep in PATH)
    #[arg(long)]
    pub live: bool,
}

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

    let db_result = open_project_db(args.db.as_deref(), &cfg.db_path);

    // No index present — fall back to ast-grep for symbol queries.
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

/// Run an ast-grep caller search for `symbol` over the working tree rooted at `root`.
fn graph_live(symbol: &str, format: &str, kind_filter: &Option<String>, root: &Path) -> Result<()> {
    if std::process::Command::new("ast-grep")
        .arg("--version")
        .output()
        .is_err()
    {
        anyhow::bail!(
            "ast-grep not found. Install with: brew install ast-grep\n\
             (or: cargo install ast-grep --locked)\n\
             Index-backed graph queries require `spelunk index .` first."
        );
    }

    let pattern = format!("{symbol}($$$)");
    let out = std::process::Command::new("ast-grep")
        .args(["run", "--pattern", &pattern, "--json"])
        .arg(root)
        .output()?;

    if !out.status.success() && out.stdout.is_empty() {
        println!("No graph edges found for '{symbol}' (live scan).");
        return Ok(());
    }

    let matches: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();

    let mut edges: Vec<GraphEdge> = matches
        .into_iter()
        .filter_map(|m| {
            let path = m["path"].as_str()?.to_string();
            let line = m["range"]["start"]["line"].as_u64().unwrap_or(0) as usize;
            Some(GraphEdge {
                source_file: path,
                source_name: None,
                target_name: symbol.to_string(),
                kind: "calls".to_string(),
                line,
            })
        })
        .collect();

    if let Some(k) = kind_filter {
        edges.retain(|e| &e.kind == k);
    }

    if edges.is_empty() {
        println!("No graph edges found for '{symbol}' (live scan).");
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
            println!("\x1b[1mIncoming to '{symbol}' (live scan — no ranking):\x1b[0m");
            for e in &edges {
                let loc = e.source_name.as_deref().unwrap_or(&e.source_file);
                println!(
                    "  \x1b[36m{}\x1b[0m  {}  \x1b[2m({}:{})\x1b[0m",
                    e.kind, e.source_file, loc, e.line
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
        println!("\x1b[1mOutgoing from '{query}':\x1b[0m");
        for e in &outgoing {
            let loc = e.source_name.as_deref().unwrap_or(&e.source_file);
            println!(
                "  \x1b[33m{}\x1b[0m  {}  \x1b[2m({}:{})\x1b[0m",
                e.kind, e.target_name, loc, e.line
            );
        }
        println!();
    }
    if !incoming.is_empty() {
        println!("\x1b[1mIncoming to '{query}':\x1b[0m");
        for e in &incoming {
            let loc = e.source_name.as_deref().unwrap_or(&e.source_file);
            println!(
                "  \x1b[36m{}\x1b[0m  {}  \x1b[2m({}:{})\x1b[0m",
                e.kind, e.source_file, loc, e.line
            );
        }
        println!();
    }
    if !other.is_empty() {
        println!("\x1b[1mRelated edges:\x1b[0m");
        for e in &other {
            let loc = e.source_name.as_deref().unwrap_or(&e.source_file);
            println!(
                "  {} -- \x1b[33m{}\x1b[0m --> {}  \x1b[2m({}:{})\x1b[0m",
                loc, e.kind, e.target_name, e.source_file, e.line
            );
        }
    }
}
