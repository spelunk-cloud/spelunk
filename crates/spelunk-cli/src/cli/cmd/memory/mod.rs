use super::color::cprintln;
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use spelunk_core::storage::NoteId;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct MemoryArgs {
    #[command(subcommand)]
    pub command: MemoryCommand,

    /// Path to the memory database (overrides auto-detect)
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,

    /// Storage backend: sqlite (default) or git-notes
    #[arg(long, global = true, default_value = "sqlite", value_name = "BACKEND")]
    pub backend: String,
}

#[derive(Subcommand, Debug)]
pub enum MemoryCommand {
    /// Store a memory entry (decision, requirement, note, question, handoff, intent, antipattern, etc.)
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
    /// Push all local memory entries to the configured memory server (one-way)
    Push(MemoryPushArgs),
    /// Pull new memory entries from the configured server into local memory.db
    Pull(MemoryPullArgs),
    /// Two-way sync: push local entries to the server and pull remote entries to local
    Sync(MemorySyncArgs),
    /// Show how the team's understanding of a topic evolved over time
    Timeline(MemoryTimelineArgs),
    /// Show the relationship graph for a memory entry
    Graph(MemoryGraphArgs),
    /// List memory entries created after a given Unix timestamp
    Since(MemorySinceArgs),
    /// Stream new memory entries from the server in real time (requires server_url)
    Watch(MemoryWatchArgs),
    /// List all stored antipatterns (shortcut for `list --kind antipattern`)
    Failures(MemoryFailuresArgs),
    /// Import unique notes from server.db into the local memory.db (recovery / migration tool)
    Reconcile(MemoryReconcileArgs),
    /// Backfill missing local embeddings so semantic search can find notes left unembedded (recovery tool)
    Reindex(MemoryReindexArgs),
    /// Collapse duplicate-entity_id groups already resident in local memory.db (recovery tool)
    Dedupe(MemoryDedupeArgs),
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

    /// Kind: decision, context, requirement, note, question, answer, handoff, intent, antipattern.
    /// An unknown kind is rejected (it would be invisible to `memory list`,
    /// `context`, and `memory failures`).
    #[arg(
        short,
        long,
        default_value = "note",
        value_parser = spelunk_core::storage::parse_note_kind
    )]
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
    pub supersedes: Option<NoteId>,

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

    /// Search only the local project's memory, skipping linked project stores
    #[arg(long)]
    pub local_only: bool,
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

    /// Output format: text, json, or jsonl
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Include archived entries
    #[arg(long)]
    pub archived: bool,

    /// Return only entries valid at this point in time (ISO 8601, e.g. 2026-03-15 or 2026-03-15T10:00:00)
    #[arg(long, value_name = "DATE")]
    pub as_of: Option<String>,

    /// List only local project's memory, skipping linked project stores
    #[arg(long)]
    pub local_only: bool,
}

#[derive(Args, Debug)]
pub struct MemoryShowArgs {
    /// Entry ID (from list or search output)
    pub id: NoteId,

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

    /// Source to harvest from: git (default), claude-code, or failures
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

    /// Confirm reading the Claude Code history file (required for --source claude-code)
    #[arg(long)]
    pub confirm: bool,

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
    /// Push archived entries too (propagates tombstones)
    #[arg(long)]
    pub include_archived: bool,
}

#[derive(Args, Debug)]
pub struct MemoryPullArgs {
    /// Reserved for future filters; pull currently fetches all entries after the
    /// UUID cursor (`MAX(remote_id)` of locally-synced rows; decision #183).
    #[arg(long, hide = true)]
    pub all: bool,
}

#[derive(Args, Debug)]
pub struct MemorySyncArgs {
    /// Local memory.db to sync (default: auto-detected memory.db)
    #[arg(long)]
    pub source: Option<std::path::PathBuf>,
    /// Include archived entries in the push (propagates tombstones)
    #[arg(long)]
    pub include_archived: bool,
    /// Cloud project slug to sync into. Required when no `project_id` is
    /// configured. On first sync the server lazily creates this project from the
    /// slug; repeat syncs with the same slug reuse it. The slug is never
    /// auto-derived from the folder or git remote (project-taxonomy).
    #[arg(long)]
    pub project: Option<String>,
}

#[derive(Args, Debug)]
pub struct MemoryArchiveArgs {
    /// ID of the entry to archive (from `spelunk memory list`)
    pub id: NoteId,
}

#[derive(Args, Debug)]
pub struct MemorySupersededArgs {
    /// ID of the entry to archive (the outdated one)
    pub old_id: NoteId,
    /// ID of the entry that replaces it (the new one)
    pub new_id: NoteId,
}

#[derive(Args, Debug)]
pub struct MemorySinceArgs {
    /// Unix epoch seconds (exclusive lower bound for `created_at`)
    pub since: i64,

    /// Maximum number of results to return
    #[arg(short, long, default_value_t = 100)]
    pub limit: usize,

    /// Output format: text, json, or jsonl
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

#[derive(Args, Debug)]
pub struct MemoryReconcileArgs {
    /// Path to the source server.db (default: ~/.local/state/spelunk/server.db).
    /// Named --source-db to avoid conflicting with the global --db (memory.db path).
    #[arg(long = "source-db")]
    pub source_db: Option<std::path::PathBuf>,

    /// Detect and report candidates without importing anything
    #[arg(long)]
    pub dry_run: bool,

    /// Reconcile every project slug found in server.db (default: active project only)
    #[arg(long)]
    pub all_projects: bool,

    /// Output format: text or json (NDJSON summary object)
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct MemoryReindexArgs {
    /// Re-embed every active note, replacing existing vectors (e.g. after a model or dimension change)
    #[arg(long)]
    pub force: bool,

    /// Also re-embed archived notes (default: active notes only)
    #[arg(long)]
    pub include_archived: bool,

    /// Report how many notes would be embedded, then exit without writing anything
    #[arg(long)]
    pub dry_run: bool,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct MemoryDedupeArgs {
    /// Detect and report duplicate entity_id groups without collapsing anything
    #[arg(long)]
    pub dry_run: bool,

    /// Output format: text or json (NDJSON summary object)
    #[arg(long, default_value = "text")]
    pub format: String,
}

use super::status::format_age;

mod add;
mod archive;
pub(crate) mod cross_project;
mod dedupe;
mod failures;
mod graph_cmd;
mod harvest;
mod harvest_claude;
mod list;
pub(crate) mod outbox;
pub mod push;
pub(crate) mod reconcile;
mod reindex;
mod search;
mod show;
mod since;
mod supersede;
pub mod sync;
mod timeline;
mod watch;

pub async fn memory(args: MemoryArgs, cfg: crate::config::Config) -> Result<()> {
    cfg.validate()?;
    let be = backend_override(&args.backend);
    // ADR-067 fails closed when there is no local `.spelunk/` project rather than
    // silently using the machine-global store. ADR-068 D3 narrows that for
    // `add`/`list` only: with no project DB but CWD inside a git repo they ride
    // the git-notes carrier instead of failing. `pre_init_notes` signals that
    // carrier mode downstream (add skips the absent SQLite primary; list reads
    // from `refs/notes/spelunk`). Store priority is otherwise unchanged (ADR-004).
    let (mem_path, pre_init_notes) = resolve_memory_store(&args, &cfg, be).await?;
    maybe_emit_reembed_notice(&mem_path, pre_init_notes, be);
    match args.command {
        MemoryCommand::Add(a) => add::memory_add(a, &mem_path, &cfg, be, pre_init_notes).await,
        MemoryCommand::Search(a) => search::memory_search(a, &mem_path, &cfg, be).await,
        MemoryCommand::List(a) => list::memory_list(a, &mem_path, &cfg, be, pre_init_notes).await,
        MemoryCommand::Show(a) => show::memory_show(a, &mem_path, &cfg, be).await,
        MemoryCommand::Harvest(a) => harvest::memory_harvest(a, &mem_path, &cfg, be).await,
        MemoryCommand::Archive(a) => archive::memory_archive(a, &mem_path, &cfg, be).await,
        MemoryCommand::Supersede(a) => supersede::memory_supersede(a, &mem_path, &cfg, be).await,
        MemoryCommand::Push(a) => push::memory_push(a, &mem_path, &cfg, be).await,
        MemoryCommand::Pull(a) => sync::memory_pull(a, &mem_path, &cfg).await,
        MemoryCommand::Sync(a) => sync::memory_sync(a, &mem_path, &cfg).await,
        MemoryCommand::Timeline(a) => timeline::memory_timeline(a, &mem_path, &cfg, be).await,
        MemoryCommand::Graph(a) => graph_cmd::memory_graph(a, &mem_path, &cfg, be).await,
        MemoryCommand::Since(a) => since::memory_since(a, &mem_path, &cfg, be).await,
        MemoryCommand::Watch(a) => watch::memory_watch(a, &cfg).await,
        MemoryCommand::Failures(a) => failures::memory_failures(a, &mem_path, &cfg, be).await,
        MemoryCommand::Reconcile(a) => reconcile::memory_reconcile(a, &mem_path, &cfg).await,
        MemoryCommand::Reindex(a) => reindex::memory_reindex(a, &mem_path, &cfg, be).await,
        MemoryCommand::Dedupe(a) => dedupe::memory_dedupe(a, &mem_path).await,
    }
}

/// One-line, `RUST_LOG`-free notice pointing the user at `memory reindex` when
/// the 768→896 store upgrade just dropped their prior note vectors (D5(b)).
///
/// `MemoryStore::open` sets `reembed_needed` only on the single open where that
/// drop ran, so this fires once, not on every command. Skipped for the
/// git-notes carrier / `--backend git-notes` paths (no local sqlite store) and
/// for a path that does not yet exist, so we never create a store just to check
/// (the CloudFirst placeholder path never has a local store to open here).
fn maybe_emit_reembed_notice(
    mem_path: &std::path::Path,
    pre_init_notes: bool,
    be: Option<&'static str>,
) {
    if pre_init_notes || be == Some("git-notes") || !mem_path.exists() {
        return;
    }
    if let Ok(store) = crate::storage::MemoryStore::open(mem_path)
        && let Some(n) = store.reembed_needed
    {
        eprintln!(
            "[spelunk] {n} note(s) need re-embedding for semantic search; \
             run 'spelunk memory reindex'."
        );
    }
}

/// Convert the `--backend` string to a static override token for `open_memory_backend`.
/// Returns `None` for the default "sqlite" to fall through to config-based dispatch.
fn backend_override(s: &str) -> Option<&'static str> {
    match s {
        "git-notes" => Some("git-notes"),
        _ => None,
    }
}

/// Resolve `(mem_path, pre_init_notes)` for the dispatched memory subcommand.
///
/// Store priority is ADR-004's, unchanged: `--db` › a resolvable local
/// `.spelunk/` DB › (for `add`/`list` without a local project) an explicit
/// CloudFirst team `server_url` › the git-notes carrier when CWD is inside a git
/// repo › fail. `open_memory_backend` still makes the final local-vs-remote and
/// `--backend git-notes` choice from `mem_path`/`cfg`; nothing here reshapes it.
///
/// The one behavioural change (ADR-068 D3) is that `add`/`list` do not fail
/// closed pre-`init`: with no project DB but a git repo, they ride the universal
/// git-notes write-through instead. `pre_init_notes` is `true` only in that
/// carrier case (no SQLite primary). Explicit `--backend git-notes` keeps git
/// notes as the *primary* store, so it is not carrier mode; its own `add`
/// writes the record and the write-through is suppressed to avoid a double
/// write. Every other subcommand keeps ADR-067's fail-closed behaviour.
async fn resolve_memory_store(
    args: &MemoryArgs,
    cfg: &crate::config::Config,
    be: Option<&'static str>,
) -> Result<(PathBuf, bool)> {
    use crate::config::SyncMode;

    // `--db` is an explicit override; always honored.
    if let Some(p) = args.db.clone() {
        return Ok((p, false));
    }
    // A resolvable local `.spelunk/` DB is the normal case.
    match crate::config::require_project_db(&cfg.db_path, false) {
        Ok(p) => return Ok((p.with_file_name("memory.db"), false)),
        Err(e) => {
            // Only `add`/`list` narrow the fail-closed bail (ADR-068 D3).
            if !matches!(args.command, MemoryCommand::Add(_) | MemoryCommand::List(_)) {
                return Err(e);
            }
        }
    }
    // No local project, running `add`/`list`. An explicit CloudFirst team
    // `server_url` still owns the store and wins over the carrier;
    // `open_memory_backend` routes remote from this placeholder path.
    if cfg.resolve_mode() == SyncMode::CloudFirst && cfg.server_url.is_some() {
        return Ok((cfg.db_path.with_file_name("memory.db"), false));
    }
    // Inside a git repo: ride the git-notes carrier rather than failing closed.
    // The returned path is a placeholder the pre-init callers never open.
    if git_head_reachable().await {
        return Ok((
            cfg.db_path.with_file_name("memory.db"),
            be != Some("git-notes"),
        ));
    }
    anyhow::bail!(
        "no spelunk project here, and not inside a git repo. \
         Run 'spelunk init' first, or run inside a git repository."
    )
}

/// Whether CWD is inside a git repo with a resolvable HEAD. An empty repo with
/// no commits fails this (`git rev-parse HEAD` errors), matching ADR-068's
/// "no git repo available" case.
async fn git_head_reachable() -> bool {
    use std::process::Stdio;
    tokio::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
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
    let source_badge = n
        .source_project
        .as_deref()
        .map(|p| format!("  \x1b[36m[from: {p}]\x1b[0m"))
        .unwrap_or_default();
    cprintln!(
        "\x1b[1m#{id}\x1b[0m  \x1b[33m[{kind}]\x1b[0m  {title}{archived}{dist_fmt}{source}",
        id = n.id,
        kind = n.kind,
        title = n.title,
        archived = archived_badge,
        dist_fmt = if dist.is_empty() {
            String::new()
        } else {
            format!("\x1b[2m{dist}\x1b[0m")
        },
        source = source_badge,
    );
    cprintln!("     \x1b[2m{}\x1b[0m", format_age(n.created_at));
    if let Some(valid_at) = n.valid_at {
        cprintln!("     \x1b[2mvalid_at: {}\x1b[0m", format_age(valid_at));
    }
    if !n.tags.is_empty() {
        println!("     tags: {}", n.tags.join(", "));
    }
    if !n.linked_files.is_empty() {
        println!("     files: {}", n.linked_files.join(", "));
    }
    if let Some(sup) = &n.superseded_by {
        cprintln!("     \x1b[2msuperseded by #{sup}\x1b[0m");
    }
    if !matches!(n.kind.as_str(), "question" | "answer") {
        let preview: Vec<&str> = n.body.lines().take(2).collect();
        for line in &preview {
            cprintln!("     \x1b[2m{line}\x1b[0m");
        }
        if n.body.lines().count() > 2 {
            cprintln!("     \x1b[2m…\x1b[0m");
        }
    } else {
        cprintln!(
            "     \x1b[2m(use `spelunk memory show {}` to read body)\x1b[0m",
            n.id
        );
    }
    println!();
}

/// Create the draft file used by [`open_editor_for_body`]: an unpredictably-named,
/// `O_EXCL`-created, mode-0600 (unix), `.md`-suffixed temp file pre-populated with
/// `title`. Kept as its own function so tests can exercise draft creation without
/// spawning an editor.
fn create_draft_file(title: &str) -> Result<tempfile::NamedTempFile> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("spelunk_memory_").suffix(".md");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o600));
    }
    let mut file = builder
        .tempfile()
        .context("failed to create temporary draft file")?;

    use std::io::Write;
    write!(
        file,
        "# {title}\n\n\
         # Write your memory entry below. Lines starting with # are ignored.\n\
         # Save and close the editor when done.\n\n"
    )?;
    file.flush()?;
    Ok(file)
}

/// Open $EDITOR (or $VISUAL, then vi) for the user to write a memory body.
pub(super) fn open_editor_for_body(title: &str) -> Result<String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());

    // NamedTempFile is created with a random name via O_EXCL (mode 0600 on unix,
    // set via the Builder above). The handle is kept open across the editor spawn
    // and read back through that same open file descriptor afterwards (not by
    // re-opening the path), so a symlink swapped in at the draft's path during
    // the edit window can't redirect the read-back (TOCTOU-safe by construction:
    // an fd, once open, always refers to the same underlying file regardless of
    // what the path is later made to point at).
    let mut tmp = create_draft_file(title)?;
    let tmp_path = tmp.path().to_path_buf();

    let status = std::process::Command::new(&editor)
        .arg(&tmp_path)
        .status()
        .with_context(|| format!("could not open editor '{editor}'"))?;

    let content = {
        use std::io::{Read, Seek, SeekFrom};
        // The editor wrote to the file via the path, not our fd, so our fd's
        // cursor/state may be stale; seek to the start before reading fresh
        // contents through the retained handle.
        tmp.seek(SeekFrom::Start(0))
            .context("failed to seek draft file for read-back")?;
        let mut buf = String::new();
        tmp.read_to_string(&mut buf)
            .context("failed to read draft file back through the retained handle")?;
        buf
    };
    // `tmp` (NamedTempFile) removes the file on drop.

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
             Re-run without --backend git-notes."
        )
    } else {
        e
    }
}

#[cfg(test)]
mod draft_file_tests {
    use super::create_draft_file;

    #[test]
    fn round_trip_content_is_readable() {
        let file = create_draft_file("My Title").expect("draft file should be created");
        let content = std::fs::read_to_string(file.path()).expect("draft file should be readable");
        assert!(content.contains("# My Title"));
        assert!(content.contains("Write your memory entry below"));
    }

    #[test]
    fn draft_path_has_md_suffix() {
        let file = create_draft_file("t").expect("draft file should be created");
        assert_eq!(file.path().extension().and_then(|e| e.to_str()), Some("md"));
    }

    #[cfg(unix)]
    #[test]
    fn draft_file_mode_is_0600() {
        use std::os::unix::fs::PermissionsExt;

        let file = create_draft_file("t").expect("draft file should be created");
        let mode = std::fs::metadata(file.path())
            .expect("draft file should have metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "draft file must be owner-only");
    }

    /// A local attacker who can predict/guess the draft's location pre-creates a
    /// symlink there pointing at a victim-owned file. Because `NamedTempFile`
    /// creates its file with `O_CREAT | O_EXCL` at an unpredictable, randomised
    /// name, draft creation must never land on — let alone follow/clobber — a
    /// pre-existing path.
    #[cfg(unix)]
    #[test]
    fn preexisting_symlink_at_guessed_path_is_not_clobbered() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir should be created");
        let victim = dir.path().join("victim.md");
        std::fs::write(&victim, "victim contents\n").expect("victim file should be writable");

        // Recreate the *old* predictable-name scheme's guessed path as a symlink
        // to the victim file, as a local attacker would.
        let guessed = dir
            .path()
            .join(format!("ca_memory_{}.md", std::process::id()));
        symlink(&victim, &guessed).expect("symlink should be created");

        // The new draft-creation path never targets `guessed` at all — it asks
        // the OS for a random, exclusively-created name — so the symlink must be
        // left completely untouched.
        let file = create_draft_file("t").expect("draft file should be created");
        assert_ne!(
            file.path(),
            guessed.as_path(),
            "draft must not reuse the old predictable path"
        );

        let victim_contents =
            std::fs::read_to_string(&victim).expect("victim file should still be readable");
        assert_eq!(
            victim_contents, "victim contents\n",
            "pre-existing symlink target must not be clobbered"
        );
        assert!(
            std::fs::symlink_metadata(&guessed)
                .expect("guessed path should still be a symlink")
                .file_type()
                .is_symlink(),
            "pre-existing symlink itself must be left alone"
        );
    }

    /// `open_editor_for_body` never invokes the editor for its own draft-file
    /// lifecycle guarantee — the draft is a `NamedTempFile`, which removes its
    /// backing file on `Drop` regardless of how the scope is exited. This
    /// covers both paths `open_editor_for_body` can take after creating the
    /// draft: the success path (editor exits 0, body read back) and the
    /// editor-failure path (non-zero exit -> `anyhow::bail!`, function returns
    /// `Err` early). In both cases the `NamedTempFile` guard drops and the file
    /// must not be left behind on disk.
    #[test]
    fn draft_file_is_removed_on_drop_after_simulated_success_path() {
        let file = create_draft_file("t").expect("draft file should be created");
        let path = file.path().to_path_buf();
        assert!(
            path.exists(),
            "draft should exist immediately after creation"
        );

        // Simulate the success path: read the body back (as open_editor_for_body
        // does after a zero exit status), then let the guard drop.
        let _content = std::fs::read_to_string(&path).expect("draft should be readable");
        drop(file);

        assert!(
            !path.exists(),
            "draft file must be deleted once the NamedTempFile guard drops (success path)"
        );
    }

    #[test]
    fn draft_file_is_removed_on_drop_after_simulated_editor_failure_path() {
        let file = create_draft_file("t").expect("draft file should be created");
        let path = file.path().to_path_buf();
        assert!(
            path.exists(),
            "draft should exist immediately after creation"
        );

        // Simulate the editor-failure path: open_editor_for_body bails out with
        // an Err before returning, dropping `tmp` as the function unwinds. No
        // read-back happens on this path.
        let result: anyhow::Result<()> = (|| {
            anyhow::bail!("Editor exited with a non-zero status; entry not saved.");
        })();
        assert!(result.is_err());
        drop(file);

        assert!(
            !path.exists(),
            "draft file must be deleted even when the editor-failure path is taken"
        );
    }

    /// SECURITY FIX VERIFICATION: `open_editor_for_body` previously read the
    /// draft body back via `std::fs::read_to_string(&tmp_path)` — i.e. by
    /// re-opening the path — rather than via the already-open
    /// `NamedTempFile` handle it retains (which implements `Read`/`Seek`
    /// directly against the original file descriptor and cannot be
    /// redirected by a path swap). `NamedTempFile`'s `O_EXCL` creation
    /// prevents an attacker from pre-empting draft *creation*, but a
    /// path-based read-back after creation was still vulnerable: if the file
    /// at that (randomised but now-known-to-an-attacker, e.g. via
    /// `/proc/<pid>/fd` or a directory watch) path was removed and replaced
    /// with a symlink to a victim file before read-back ran,
    /// `std::fs::read_to_string` would follow the symlink and return the
    /// victim's content instead of the drafted memory body — silently
    /// injecting attacker-controlled content into the stored memory entry.
    ///
    /// This test performs the same attacker race (remove the draft at its
    /// path, replace it with a symlink to attacker-controlled content) and
    /// then reads back the same way `open_editor_for_body` now does: through
    /// the retained `NamedTempFile` handle (seek-to-start + `Read`), not by
    /// re-opening the path. It asserts the handle-based read-back is
    /// TOCTOU-safe: it returns the *original* draft content (an empty body,
    /// since the editor never actually ran in this test) and does NOT
    /// observe the attacker's swapped-in content, proving the fix closes the
    /// gap. A control assertion also shows that reading via the path
    /// directly (what the old, vulnerable code did) *would* have followed
    /// the symlink, so the contrast is explicit.
    #[cfg(unix)]
    #[test]
    fn handle_based_read_back_ignores_a_post_creation_symlink_swap() {
        use std::io::{Read, Seek, SeekFrom};
        use std::os::unix::fs::symlink;

        let mut file = create_draft_file("t").expect("draft file should be created");
        let tmp_path = file.path().to_path_buf();

        let dir = tmp_path.parent().unwrap();
        let victim = dir.join("attacker_victim.md");
        std::fs::write(&victim, "ATTACKER-CONTROLLED CONTENT\n")
            .expect("victim file should be writable");

        // Attacker wins the race: removes the draft file at its now-known path
        // and replaces it with a symlink to attacker-controlled content. This
        // models the window between draft creation (path becomes known/guessable
        // to a co-resident attacker) and read-back after the editor returns.
        std::fs::remove_file(&tmp_path).expect("should be able to remove the draft for the PoC");
        symlink(&victim, &tmp_path).expect("symlink should be created at the draft's old path");

        // Control: a path-based read-back (the old, vulnerable behaviour)
        // does follow the symlink and would leak attacker content.
        let path_based_content = std::fs::read_to_string(&tmp_path)
            .expect("path-based read-back follows the symlink (control demonstrates the gap)");
        assert_eq!(
            path_based_content, "ATTACKER-CONTROLLED CONTENT\n",
            "control: path-based read-back should still be shown to follow the swapped symlink"
        );

        // This mirrors open_editor_for_body's actual (fixed) read-back: seek
        // the retained handle to the start and read through the open fd,
        // never re-opening by path.
        file.seek(SeekFrom::Start(0))
            .expect("seek on retained handle should succeed");
        let mut handle_based_content = String::new();
        file.read_to_string(&mut handle_based_content).expect(
            "handle-based read-back should succeed even though the path now points elsewhere",
        );

        assert_ne!(
            handle_based_content, "ATTACKER-CONTROLLED CONTENT\n",
            "handle-based read-back must NOT observe the attacker's swapped-in content"
        );
        assert!(
            handle_based_content.contains("# t"),
            "handle-based read-back should still see the original draft content \
             (the title header written at creation), proving it reads through \
             the original fd rather than the swapped path: got {handle_based_content:?}"
        );

        // Best-effort cleanup of the PoC symlink (not the NamedTempFile's own
        // path management, since we've already replaced what's there).
        let _ = std::fs::remove_file(&tmp_path);
    }
}
