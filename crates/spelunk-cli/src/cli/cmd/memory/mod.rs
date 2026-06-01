use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct MemoryArgs {
    #[command(subcommand)]
    pub command: MemoryCommand,

    /// Path to the memory database (overrides auto-detect)
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,

    /// Storage backend: auto (default), git-meta, sqlite, or git-notes.
    /// `auto` resolves to server_url → local embedder (sqlite) → git-meta.
    /// Pass `sqlite` to force local semantic search; `git-meta` to force the
    /// zero-infra git-native backend.
    #[arg(long, global = true, default_value = "auto", value_name = "BACKEND")]
    pub backend: String,
}

#[derive(Subcommand, Debug)]
pub enum MemoryCommand {
    /// Store a decision, context, requirement, note, question, answer, handoff, or intent
    Add(MemoryAddArgs),
    /// Semantic search over stored memory
    Search(MemorySearchArgs),
    /// List memory entries (newest first)
    List(MemoryListArgs),
    /// Show the full content of a memory entry
    Show(MemoryShowArgs),
    /// Auto-harvest memory entries from git commit messages using the LLM
    Harvest(MemoryHarvestArgs),
    /// Archive a memory entry (hidden from search and ask, but preserved)
    Archive(MemoryArchiveArgs),
    /// Archive an entry and mark it as superseded by a newer entry
    Supersede(MemorySupersededArgs),
    /// Push all local memory entries to the configured memory server
    Push(MemoryPushArgs),
    /// Show how the team's understanding of a topic evolved over time
    Timeline(MemoryTimelineArgs),
    /// Show the relationship graph for a memory entry
    Graph(MemoryGraphArgs),
    /// List memory entries created after a given Unix timestamp
    Since(MemorySinceArgs),
    /// Stream new memory entries from the server in real time (requires memory_server_url)
    Watch(MemoryWatchArgs),
    /// List all stored antipatterns (shortcut for `list --kind antipattern`)
    Failures(MemoryFailuresArgs),
}

#[derive(Args, Debug)]
pub struct MemoryGraphArgs {
    /// Entry ID to show the relationship graph for
    pub id: i64,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct MemoryTimelineArgs {
    /// Topic to trace through time
    pub query: String,

    /// Number of entries to retrieve before timeline construction
    #[arg(short, long, default_value = "20")]
    pub limit: usize,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct MemoryAddArgs {
    /// Short title summarising the entry (inferred from URL if --from-url is used)
    #[arg(short, long)]
    pub title: Option<String>,

    /// Full body text (omit to open $EDITOR)
    #[arg(short, long)]
    pub body: Option<String>,

    /// Fetch content from a URL (GitHub issue, Linear ticket, or any web page)
    #[arg(long)]
    pub from_url: Option<String>,

    /// Kind: decision, context, requirement, note, question, answer, handoff, intent
    #[arg(short, long, default_value = "note")]
    pub kind: String,

    /// Comma-separated tags (e.g. auth,database)
    #[arg(long)]
    pub tags: Option<String>,

    /// Comma-separated file paths this entry relates to
    #[arg(long)]
    pub files: Option<String>,

    /// When this entry became valid (ISO 8601, e.g. 2026-03-15 or 2026-03-15T10:00:00).
    /// Defaults to now (created_at) when omitted.
    #[arg(long, value_name = "DATE")]
    pub valid_at: Option<String>,

    /// ID of an existing entry that this new entry supersedes.
    /// The old entry's invalid_at is set to now atomically in the same transaction.
    #[arg(long, value_name = "ID")]
    pub supersedes: Option<i64>,

    /// ID of an existing entry this entry relates to (creates a relates_to edge).
    #[arg(long, value_name = "ID")]
    pub relates_to: Option<i64>,
}

#[derive(Args, Debug)]
pub struct MemorySearchArgs {
    /// Natural language query
    pub query: String,

    /// Number of results to return
    #[arg(short, long, default_value = "10")]
    pub limit: usize,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Search mode: hybrid (default), semantic, text
    #[arg(long, default_value = "hybrid")]
    pub mode: String,

    /// Return only entries valid at this point in time (ISO 8601, e.g. 2026-03-15 or 2026-03-15T10:00:00)
    #[arg(long, value_name = "DATE")]
    pub as_of: Option<String>,

    /// Expand results by 1 hop along relates_to edges
    #[arg(long)]
    pub expand_graph: bool,
}

#[derive(Args, Debug)]
pub struct MemoryListArgs {
    /// Filter by kind: decision, context, requirement, note, intent
    #[arg(short, long)]
    pub kind: Option<String>,

    /// Filter by commit SHA (exact or prefix match against source_ref)
    #[arg(long)]
    pub source_ref: Option<String>,

    /// Number of entries to show
    #[arg(short, long, default_value = "20")]
    pub limit: usize,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Include archived entries
    #[arg(long)]
    pub archived: bool,

    /// Return only entries valid at this point in time (ISO 8601, e.g. 2026-03-15 or 2026-03-15T10:00:00)
    #[arg(long, value_name = "DATE")]
    pub as_of: Option<String>,
}

#[derive(Args, Debug)]
pub struct MemoryShowArgs {
    /// Entry ID (from list or search output)
    pub id: i64,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct MemoryHarvestArgs {
    /// Git revision range to analyse, e.g. `HEAD~10..HEAD` or `v0.1.0..HEAD`.
    /// Mutually exclusive with --branch.
    #[arg(long, default_value = "HEAD~10..HEAD", conflicts_with = "branch")]
    pub git_range: String,

    /// Harvest the entire commit history of a branch, e.g. `main` or `master`.
    /// Mutually exclusive with --git-range.
    #[arg(long, conflicts_with = "git_range")]
    pub branch: Option<String>,

    /// Number of commits/sessions to send to the LLM in each request.
    /// Smaller values are more stable; larger values risk hitting context-window limits.
    #[arg(long, default_value_t = 3)]
    pub batch_size: usize,

    /// Source to harvest from: git (default) or claude-code
    #[arg(long, default_value = "git")]
    pub source: String,

    /// Path to Claude Code history file (default: ~/.claude/history.jsonl).
    /// Only used with --source claude-code.
    #[arg(long)]
    pub history_file: Option<std::path::PathBuf>,

    /// Only harvest sessions after this date (ISO 8601, e.g. 2026-04-01).
    /// Only used with --source claude-code.
    #[arg(long)]
    pub since: Option<String>,

    /// Confirm reading git objects or history files (required for --source claude-code and --source entire)
    #[arg(long)]
    pub confirm: bool,

    /// Path to the repo containing refs/entire/checkpoints/v1 (default: current repo).
    /// Only used with --source entire.
    #[arg(long)]
    pub entire_repo: Option<std::path::PathBuf>,

    /// List checkpoints that would be harvested without writing anything.
    /// Only used with --source entire.
    #[arg(long)]
    pub dry_run: bool,

    /// Detach immediately: re-exec spelunk in the background and return.
    /// Useful in git hooks so the hook does not block the git process.
    #[arg(long, default_value_t = false)]
    pub detach: bool,
}

#[derive(Args, Debug)]
pub struct MemoryPushArgs {
    /// Local memory.db to push from (default: same as --db)
    #[arg(long)]
    pub source: Option<std::path::PathBuf>,
    /// Push archived entries too
    #[arg(long)]
    pub include_archived: bool,
}

#[derive(Args, Debug)]
pub struct MemoryArchiveArgs {
    /// ID of the entry to archive (from `spelunk memory list`)
    pub id: i64,
}

#[derive(Args, Debug)]
pub struct MemorySupersededArgs {
    /// ID of the entry to archive (the outdated one)
    pub old_id: i64,
    /// ID of the entry that replaces it (the new one)
    pub new_id: i64,
}

#[derive(Args, Debug)]
pub struct MemorySinceArgs {
    /// Unix epoch seconds (exclusive lower bound for `created_at`)
    pub since: i64,

    /// Maximum number of results to return
    #[arg(short, long, default_value_t = 100)]
    pub limit: usize,

    /// Output format: text, json, or ndjson
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct MemoryWatchArgs {
    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Comma-separated kind filter, e.g. `intent,decision`.
    /// When absent all event kinds are streamed.
    #[arg(long)]
    pub kind: Option<String>,

    /// Resume from a specific sequence ID (seq-NNNNNNN or plain integer).
    /// When set, the server replays missed events before switching to live.
    /// In the default mode the CLI tracks the last-seen ID automatically and
    /// reconnects on transient errors.
    #[arg(long, value_name = "SEQ")]
    pub since_seq: Option<String>,

    /// Maximum number of automatic reconnect attempts on connection error.
    /// Set to 0 to disable reconnection (one-shot mode).
    #[arg(long, default_value_t = 10)]
    pub reconnect_limit: u32,
}

#[derive(Args, Debug)]
pub struct MemoryFailuresArgs {
    /// Number of entries to show
    #[arg(short, long, default_value = "20")]
    pub limit: usize,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Return only entries valid at this point in time (ISO 8601)
    #[arg(long, value_name = "DATE")]
    pub as_of: Option<String>,
}

use super::status::format_age;

mod add;
mod archive;
mod failures;
mod graph_cmd;
mod harvest;
mod harvest_claude;
mod harvest_entire;
mod list;
pub mod push;
mod search;
mod show;
mod since;
mod supersede;
mod timeline;
mod watch;

pub async fn memory(args: MemoryArgs, cfg: crate::config::Config) -> Result<()> {
    cfg.validate()?;
    let mem_path = args.db.clone().unwrap_or_else(|| {
        crate::config::resolve_db(None, &cfg.db_path).with_file_name("memory.db")
    });
    let be = backend_override(&args.backend);
    match args.command {
        MemoryCommand::Add(a) => add::memory_add(a, &mem_path, &cfg, be).await,
        MemoryCommand::Search(a) => search::memory_search(a, &mem_path, &cfg, be).await,
        MemoryCommand::List(a) => list::memory_list(a, &mem_path, &cfg, be).await,
        MemoryCommand::Show(a) => show::memory_show(a, &mem_path, &cfg, be).await,
        MemoryCommand::Harvest(a) => harvest::memory_harvest(a, &mem_path, &cfg, be).await,
        MemoryCommand::Archive(a) => archive::memory_archive(a, &mem_path, &cfg, be).await,
        MemoryCommand::Supersede(a) => supersede::memory_supersede(a, &mem_path, &cfg, be).await,
        MemoryCommand::Push(a) => push::memory_push(a, &mem_path, &cfg, be).await,
        MemoryCommand::Timeline(a) => timeline::memory_timeline(a, &mem_path, &cfg, be).await,
        MemoryCommand::Graph(a) => graph_cmd::memory_graph(a, &mem_path, &cfg, be).await,
        MemoryCommand::Since(a) => since::memory_since(a, &mem_path, &cfg, be).await,
        MemoryCommand::Watch(a) => watch::memory_watch(a, &cfg).await,
        MemoryCommand::Failures(a) => failures::memory_failures(a, &mem_path, &cfg, be).await,
    }
}

/// Convert the `--backend` string to a static override token for `open_memory_backend`.
///
/// Any recognised value produces an explicit override that takes precedence over
/// config-based dispatch. The clap default is `auto`, which returns `None` so that
/// `open_memory_backend` runs its full resolution order (server_url → local
/// embedder → git-meta). `sqlite` is the explicit opt-out for local semantic
/// search; `git-meta`/`git-notes` force the respective git-native backends.
fn backend_override(s: &str) -> Option<&'static str> {
    match s {
        "git-meta" => Some("git-meta"),
        "git-notes" => Some("git-notes"),
        "sqlite" => Some("sqlite"),
        // "auto" and any unknown value fall through to config-based dispatch.
        _ => None,
    }
}

// ── Shared display helpers ────────────────────────────────────────────────────

pub(super) fn print_note_summary(n: &crate::storage::memory::Note) {
    let dist = if let Some(s) = n.score {
        format!("  score: {s:.4}")
    } else {
        n.distance
            .map(|d| format!("  dist: {d:.4}"))
            .unwrap_or_default()
    };
    let archived_badge = if n.status == "archived" {
        " \x1b[31m[archived]\x1b[0m"
    } else {
        ""
    };
    println!(
        "\x1b[1m#{id}\x1b[0m  \x1b[33m[{kind}]\x1b[0m  {title}{archived}{dist_fmt}",
        id = n.id,
        kind = n.kind,
        title = n.title,
        archived = archived_badge,
        dist_fmt = if dist.is_empty() {
            String::new()
        } else {
            format!("\x1b[2m{dist}\x1b[0m")
        },
    );
    println!("     \x1b[2m{}\x1b[0m", format_age(n.created_at));
    if let Some(valid_at) = n.valid_at {
        println!("     \x1b[2mvalid_at: {}\x1b[0m", format_age(valid_at));
    }
    if !n.tags.is_empty() {
        println!("     tags: {}", n.tags.join(", "));
    }
    if !n.linked_files.is_empty() {
        println!("     files: {}", n.linked_files.join(", "));
    }
    if let Some(sup) = n.superseded_by {
        println!("     \x1b[2msuperseded by #{sup}\x1b[0m");
    }
    if !matches!(n.kind.as_str(), "question" | "answer") {
        let preview: Vec<&str> = n.body.lines().take(2).collect();
        for line in &preview {
            println!("     \x1b[2m{line}\x1b[0m");
        }
        if n.body.lines().count() > 2 {
            println!("     \x1b[2m…\x1b[0m");
        }
    } else {
        println!(
            "     \x1b[2m(use `spelunk memory show {}` to read body)\x1b[0m",
            n.id
        );
    }
    println!();
}

/// Open $EDITOR (or $VISUAL, then vi) for the user to write a memory body.
pub(super) fn open_editor_for_body(title: &str) -> Result<String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());

    let tmp = std::env::temp_dir().join(format!("ca_memory_{}.md", std::process::id()));
    std::fs::write(
        &tmp,
        format!(
            "# {title}\n\n\
         # Write your memory entry below. Lines starting with # are ignored.\n\
         # Save and close the editor when done.\n\n"
        ),
    )?;

    let status = std::process::Command::new(&editor)
        .arg(&tmp)
        .status()
        .with_context(|| format!("could not open editor '{editor}'"))?;

    let content = std::fs::read_to_string(&tmp)?;
    std::fs::remove_file(&tmp).ok();

    if !status.success() {
        anyhow::bail!("Editor exited with a non-zero status; entry not saved.");
    }

    let body: String = content
        .lines()
        .filter(|l| !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    if body.is_empty() {
        anyhow::bail!("Body is empty; entry not saved.");
    }
    Ok(body)
}

// Re-export from the shared dates module for use within this submodule tree.
pub(super) use crate::utils::dates::parse_as_of;

/// Convert a `BackendUnsupported` error into a user-friendly message.
/// Pass as `.map_err(backend_err)?` at each call site that invokes an
/// unsupported method on a limited backend.
pub(super) fn backend_err(e: anyhow::Error) -> anyhow::Error {
    if e.downcast_ref::<crate::error::SpelunkError>()
        .is_some_and(|s| matches!(s, crate::error::SpelunkError::BackendUnsupported(_)))
    {
        anyhow::anyhow!(
            "This operation requires the sqlite backend. \
             Re-run without --backend git-meta or --backend git-notes."
        )
    } else {
        e
    }
}
