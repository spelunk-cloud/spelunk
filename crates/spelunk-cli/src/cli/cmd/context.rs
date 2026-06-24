use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

use super::memory::cross_project::collect_dep_cross_cutting;
use super::memory::print_note_summary;
use crate::storage::memory::Note;
use crate::{config::Config, storage::open_memory_backend};

/// Fallback per-section limit when `--kind` names a kind not in SECTIONS.
const DEFAULT_UNKNOWN_KIND_LIMIT: usize = 20;

/// Kinds for which the cross-project dep pass runs (§3 ADR-003).
/// `handoff` and `question` are strictly local — session/project-scoped noise.
const DEP_PASS_KINDS: &[&str] = &["decision", "requirement"];

/// Agent-facing entry-point command: pull the most relevant memory sections
/// in one shot (handoffs → questions → decisions → requirements).
/// Appends a "conventions" section from the local index when available.
#[derive(Args, Debug)]
pub struct ContextArgs {
    /// Path to the memory database (overrides auto-detect)
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Path to the spelunk index database (overrides auto-detect).
    /// Used to load the conventions section.
    #[arg(long, value_name = "INDEX_DB")]
    pub index_db: Option<PathBuf>,

    /// Storage backend: sqlite (default) or git-notes
    #[arg(long, default_value = "sqlite", value_name = "BACKEND")]
    pub backend: String,

    /// Filter to a specific kind instead of the default multi-section view
    #[arg(short, long, value_name = "KIND")]
    pub kind: Option<String>,

    /// Maximum entries per section (defaults: handoff=3, question=500, decision=10, requirement=500)
    #[arg(short, long, value_name = "N")]
    pub limit: Option<usize>,

    /// Only show entries tagged with this file or directory path
    #[arg(long, value_name = "PATH")]
    pub path: Option<String>,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Skip the conventions section (default: false)
    #[arg(long)]
    pub no_conventions: bool,

    /// Query only the local project's memory, skipping linked project stores
    #[arg(long)]
    pub local_only: bool,
}

struct Section {
    kind: &'static str,
    /// Fetch this many entries before optional path post-filter; 500 is the NoteStore hard-cap.
    default_limit: usize,
}

const SECTIONS: &[Section] = &[
    Section {
        kind: "handoff",
        default_limit: 3,
    },
    Section {
        kind: "question",
        default_limit: 500,
    },
    Section {
        kind: "decision",
        default_limit: 10,
    },
    Section {
        kind: "requirement",
        default_limit: 500,
    },
];

pub async fn context(args: ContextArgs, cfg: Config) -> Result<()> {
    cfg.validate()?;
    let mem_path = args.db.clone().unwrap_or_else(|| {
        crate::config::resolve_db(None, &cfg.db_path).with_file_name("memory.db")
    });

    // Discovery nudge: warn once when unimported server.db notes exist.
    crate::cli::cmd::memory::reconcile::maybe_emit_nudge(&mem_path, &cfg);

    let be = match args.backend.as_str() {
        "git-notes" => Some("git-notes"),
        _ => None,
    };
    let backend = open_memory_backend(&cfg, &mem_path, be).await?;

    let mut sections = collect_sections(
        &*backend,
        args.kind.as_deref(),
        args.limit,
        args.path.as_deref(),
    )
    .await?;

    // Cross-project dep pass (ADR-003): for decision and requirement sections,
    // append locked/cross-project entries from linked projects.
    // `handoff` and `question` are always local (§3 ADR-003).
    if !args.local_only {
        let index_db_path = args
            .index_db
            .clone()
            .unwrap_or_else(|| crate::config::resolve_db(None, &cfg.db_path));
        let mut seen: std::collections::HashSet<(String, i64)> = Default::default();
        // Seed seen from all local notes to avoid printing a dep note that
        // somehow shares an ID with a local note.
        for (_, notes) in &sections {
            for n in notes {
                seen.insert((String::new(), n.id));
            }
        }
        let dep_notes = collect_dep_cross_cutting(&index_db_path, &mut seen).await;

        // Merge dep notes into the appropriate section buckets.
        for dep_note in dep_notes {
            let kind = dep_note.kind.clone();
            if !DEP_PASS_KINDS.contains(&kind.as_str()) {
                continue;
            }
            // If a --kind filter is active, only include matching dep notes.
            if let Some(ref kf) = args.kind
                && &kind != kf
            {
                continue;
            }
            // Find the matching section bucket and append.
            if let Some((_, notes)) = sections.iter_mut().find(|(k, _)| k == &kind) {
                notes.push(dep_note);
            }
        }
    }

    // Load conventions from the index DB (best-effort; skip if unavailable).
    let conventions: Vec<crate::conventions::ConventionRecord> =
        if !args.no_conventions && args.kind.is_none() {
            load_conventions(args.index_db.as_deref(), &cfg)
        } else {
            vec![]
        };

    match crate::utils::effective_format(&args.format) {
        "json" => {
            let output = serde_json::json!({
                "sections": sections,
                "conventions": conventions,
            });
            println!("{output}");
        }
        _ => {
            for (kind, notes) in &sections {
                if notes.is_empty() {
                    continue;
                }
                print_section_header(kind);
                for n in notes {
                    print_note_summary(n);
                }
            }
            if !conventions.is_empty() {
                print_conventions_section(&conventions);
            }
        }
    }
    Ok(())
}

/// Load conventions from the project index DB.
/// Returns an empty vec if the DB doesn't exist or conventions table is empty.
fn load_conventions(
    index_db_override: Option<&std::path::Path>,
    cfg: &Config,
) -> Vec<crate::conventions::ConventionRecord> {
    let db_path = if let Some(p) = index_db_override {
        p.to_path_buf()
    } else {
        crate::config::resolve_db(None, &cfg.db_path)
    };
    if !db_path.exists() {
        return vec![];
    }
    match crate::storage::Database::open(&db_path) {
        Ok(db) => crate::conventions::list_conventions(&db, None).unwrap_or_default(),
        Err(e) => {
            tracing::debug!("conventions: could not open index db: {e}");
            vec![]
        }
    }
}

async fn collect_sections(
    backend: &dyn crate::storage::MemoryBackend,
    kind_filter: Option<&str>,
    limit_override: Option<usize>,
    path_filter: Option<&str>,
) -> Result<Vec<(String, Vec<Note>)>> {
    let mut result = Vec::new();

    let sections: Vec<(&str, usize)> = if let Some(k) = kind_filter {
        let default_limit = SECTIONS
            .iter()
            .find(|s| s.kind == k)
            .map(|s| s.default_limit)
            .unwrap_or(DEFAULT_UNKNOWN_KIND_LIMIT);
        vec![(k, limit_override.unwrap_or(default_limit))]
    } else {
        SECTIONS
            .iter()
            .map(|s| (s.kind, limit_override.unwrap_or(s.default_limit)))
            .collect()
    };

    for (kind, limit) in sections {
        let mut notes = backend.list(Some(kind), limit, false, None).await?;
        if let Some(p) = path_filter {
            notes.retain(|n| n.linked_files.iter().any(|f| f.contains(p)));
        }
        result.push((kind.to_string(), notes));
    }
    Ok(result)
}

fn print_section_header(kind: &str) {
    let label = match kind {
        "handoff" => "Handoffs",
        "question" => "Open questions",
        "decision" => "Decisions",
        "requirement" => "Requirements",
        other => other,
    };
    println!("\x1b[1;34m── {label} \x1b[0m");
    println!();
}

fn print_conventions_section(records: &[crate::conventions::ConventionRecord]) {
    println!("\x1b[1;34m── Conventions \x1b[0m");
    println!();

    // Group by language for readability.
    let mut by_lang: std::collections::BTreeMap<&str, Vec<&crate::conventions::ConventionRecord>> =
        std::collections::BTreeMap::new();
    for r in records {
        by_lang.entry(r.language.as_str()).or_default().push(r);
    }
    for (lang, recs) in &by_lang {
        println!("\x1b[1m{lang}\x1b[0m");
        for r in recs {
            println!(
                "  [{:.0}%] {} — {}",
                r.confidence * 100.0,
                r.category,
                r.description
            );
        }
        println!();
    }
}
