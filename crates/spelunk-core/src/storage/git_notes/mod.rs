use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::entity_id::note_entity_id;
use super::memory::Note;
use super::note_record::{NoteRecord, now_millis, now_secs, record_to_note};
use fold::fold_records;

mod backend_impl;
mod fold;
mod lock;
mod publish;
mod refs;

pub use lock::{LOCK_WAIT_BUDGET, LockAttempt, NotesLock, lock_notes};
pub use publish::{PublishOutcome, SkipReason, publish_notes};
pub use refs::NotesRefs;

// ── Carry config: surviving history rewrites ─────────────────────────────────

/// The ref spelunk stores memory notes on.
const SPELUNK_NOTES_REF: &str = "refs/notes/spelunk";

/// The tracking ref `git fetch` populates, per the refspec `spelunk init`
/// configures. Fetching straight onto [`SPELUNK_NOTES_REF`] would force-update
/// it and silently destroy local unpushed notes (ADR-069 D4).
const SPELUNK_TRACKING_REF: &str = "refs/notes/origin/spelunk";

/// The namespace git is willing to rewrite notes in.
const NOTES_NAMESPACE: &str = "refs/notes/";

/// What [`ensure_notes_rewrite_ref`] found or did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteRefStatus {
    /// This call added the setting; announce it once.
    Configured,
    /// Already named by an existing value (exactly, or via a glob).
    AlreadyCovered,
    /// Could not be set; the reason is logged. Entries stay at risk.
    Failed,
}

/// Point `notes.rewriteRef` at spelunk's notes ref in this repo.
///
/// Gotcha: git carries a note onto a rewritten commit (`commit --amend`,
/// `rebase`) only if `notes.rewriteRef` names the ref, and it has **no**
/// built-in default, so an unconfigured repo silently orphans every entry.
/// Pre-`init` git notes is the sole store, making that total loss.
///
/// `notes.rewriteMode` is deliberately left alone: its `concatenate` default
/// keeps every JSON line, whereas `overwrite` and `ignore` each drop one side
/// of a squashed pair, causing the loss this is meant to prevent.
///
/// Never returns an error: the write it guards may be an entry's only copy, so
/// a config failure must not sink it.
pub async fn ensure_notes_rewrite_ref(git_root: Option<&std::path::Path>) -> RewriteRefStatus {
    // Reads local, global and system scopes, so a user who set this themselves
    // anywhere is left alone. Absent (exit 1) means unset, not an error.
    let existing = run_git(git_root, &["config", "--get-all", "notes.rewriteRef"])
        .await
        .unwrap_or_default();
    if existing.lines().any(rewrite_ref_covers_spelunk) {
        return RewriteRefStatus::AlreadyCovered;
    }

    // Multi-valued: `--add` composes with any value the user already has, and
    // writes to the repo-local config (never global).
    match run_git(
        git_root,
        &["config", "--add", "notes.rewriteRef", SPELUNK_NOTES_REF],
    )
    .await
    {
        Ok(_) => RewriteRefStatus::Configured,
        Err(e) => {
            tracing::warn!(
                "could not set notes.rewriteRef ({e}); memory will not survive \
                 `git commit --amend` or `git rebase`"
            );
            RewriteRefStatus::Failed
        }
    }
}

/// Whether an existing `notes.rewriteRef` value already names spelunk's ref.
///
/// Values may be globs. git refuses to rewrite notes outside `refs/notes/`, so
/// a glob only counts while it stays inside that namespace: `refs/notes/*`
/// covers us, `refs/*` does not. A false negative only re-adds the exact ref,
/// which stays correct, so matching a trailing `*` is enough.
fn rewrite_ref_covers_spelunk(value: &str) -> bool {
    let value = value.trim();
    if value == SPELUNK_NOTES_REF {
        return true;
    }
    value.strip_suffix('*').is_some_and(|prefix| {
        prefix.starts_with(NOTES_NAMESPACE) && SPELUNK_NOTES_REF.starts_with(prefix)
    })
}

// ── Write-through helper (free function) ─────────────────────────────────────

/// How a writer holds, or legitimately does not hold, the notes lock.
///
/// `Unlocked` is ADR-069 D8's one kept degradation, and it is a **returned
/// value** so a caller can surface it: a `tracing::warn!` reaches nobody
/// without `RUST_LOG`, and a degradation no caller can see is how silent data
/// loss stayed invisible in the first place.
#[must_use]
enum WriterLock {
    /// Held until dropped; the guard is retained only for its `Drop`.
    Held { _guard: NotesLock },
    /// The lock cannot exist here; the write proceeds unserialized.
    Unlocked { path: PathBuf, reason: String },
}

/// Take the notes lock for a writer, per ADR-069 D8: hold it or fail, except
/// where the lock cannot exist at all, which degrades unlocked and loudly.
///
/// `Ok(WriterLock::Unlocked { .. })` is that one degradation. `Err` is
/// contention (someone else holds the lock; writing anyway is the #185 loss)
/// or a failed path resolution (git itself is failing, and the writer's own
/// git calls are next).
async fn writer_lock(git_root: Option<&std::path::Path>) -> Result<WriterLock> {
    match lock_notes(git_root).await? {
        LockAttempt::Acquired(guard) => Ok(WriterLock::Held { _guard: guard }),
        LockAttempt::Contended { path } => Err(anyhow!(
            "the git notes lock ({}) stayed held by other writers for over {:?}; \
             not writing without it, because an unserialized write can silently \
             erase a concurrent writer's entry. Retry the command (many \
             concurrent writers can exceed the wait legitimately); if it \
             persists with nothing else running, a spelunk or git process is \
             stuck holding the lock (it frees itself when that process exits)",
            path.display(),
            lock::LOCK_WAIT_BUDGET,
        )),
        LockAttempt::Unusable { path, reason } => {
            tracing::warn!(
                "git notes lock {} unusable ({reason}); writing without \
                 serialization, so a concurrent memory write could be lost",
                path.display()
            );
            Ok(WriterLock::Unlocked { path, reason })
        }
    }
}

/// Attempts for [`read_note_body`] before its failure is surfaced. The
/// windows-latest losses were transient: the same read succeeded for every
/// sibling writer moments apart, so a brief, bounded retry of a side-effect
/// free read absorbs the flake without hiding a persistent failure.
const NOTE_READ_ATTEMPTS: u32 = 4;

/// Base backoff between read attempts; grows linearly per attempt.
const NOTE_READ_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

/// [`read_note_body`], retried up to [`NOTE_READ_ATTEMPTS`] times.
///
/// Retries only genuine failures, never "no note found": the read has no side
/// effects, so retrying cannot double-apply anything, and a persistent failure
/// still reaches the caller as the `Err` that keeps a writer from wiping the
/// note. Total added wait is bounded well under the lock budget, so a holder's
/// cost stays local work (ADR-069 D9).
async fn read_note_body_with_retry(
    git_root: Option<&std::path::Path>,
    object: &str,
) -> Result<Option<String>> {
    let mut last_err = None;
    for attempt in 1..=NOTE_READ_ATTEMPTS {
        match read_note_body(git_root, object).await {
            Ok(body) => return Ok(body),
            Err(e) => {
                tracing::warn!(
                    "reading existing note failed (attempt {attempt}/{NOTE_READ_ATTEMPTS}): {e}"
                );
                last_err = Some(e);
                if attempt < NOTE_READ_ATTEMPTS {
                    tokio::time::sleep(NOTE_READ_BACKOFF * attempt).await;
                }
            }
        }
    }
    Err(last_err.expect("at least one attempt ran"))
}

/// Read the spelunk note body on `object`, distinguishing "no note" (`None`)
/// from a failed read (`Err`).
///
/// The distinction is load-bearing: a writer that mistakes a failed read for
/// "no note yet" rewrites the whole note as just its own line, erasing every
/// sibling entry. Seen live on Windows CI, where a transient git failure
/// inside the guarded section wiped 6 of 8 concurrent entries (#185).
///
/// Matches on the exit code, not the message: "no note found" exits 1, while
/// infrastructure failures die with 128, and the message text is localized.
async fn read_note_body(
    git_root: Option<&std::path::Path>,
    object: &str,
) -> Result<Option<String>> {
    let mut cmd = Command::new("git");
    if let Some(d) = git_root {
        cmd.current_dir(d);
    }
    let out = cmd
        .args(["notes", "--ref=spelunk", "show", "--", object])
        .output()
        .await?;

    if out.status.success() {
        return Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()));
    }
    if out.status.code() == Some(1) {
        return Ok(None);
    }
    Err(anyhow!(
        "git notes show -- {object}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// What [`append_to_git_notes`] did, beyond writing the entry.
#[derive(Debug)]
pub struct AppendOutcome {
    /// The carry-config status ensured along the way; a CLI caller announces
    /// it once.
    pub rewrite_ref: RewriteRefStatus,
    /// Set when the write proceeded **without** the notes lock (ADR-069 D8's
    /// one kept degradation, on a filesystem where the lock cannot exist).
    /// The caller must show it to the user: this return value is the only
    /// channel that works without `RUST_LOG`.
    pub lock_degradation: Option<String>,
}

/// Append a `NoteRecord` as a JSON line to `refs/notes/spelunk` on HEAD.
///
/// Read-modify-write with append semantics: the existing blob is read and its
/// lines (spelunk records and foreign content alike) are preserved verbatim;
/// the new record is appended as one JSON line; the combined text is written
/// back with `git notes add -f`.
///
/// Serialized end to end by [`lock_notes`]; without it a concurrent writer
/// reads the same body and silently drops this entry on write-back (#185).
/// Per ADR-069 D8 a contended lock is an `Err` and nothing is written; only a
/// lock that cannot exist on this filesystem degrades to an unlocked write,
/// reported in [`AppendOutcome::lock_degradation`].
///
/// # Arguments
/// * `git_root` — directory passed to `git -C`; `None` uses the process CWD.
/// * `record` — the entry to append.
pub async fn append_to_git_notes(
    git_root: Option<&std::path::Path>,
    record: &NoteRecord,
) -> Result<AppendOutcome> {
    // Touches `git config` only, never the notes ref, so it stays outside the
    // lock: serializing it would widen the guarded section for nothing.
    let rewrite_ref = ensure_notes_rewrite_ref(git_root).await;

    // Guards all four steps (D8). Bind the whole enum: `Held`'s guard must
    // live to the end of the function.
    let lock = writer_lock(git_root).await?;
    let lock_degradation = match &lock {
        WriterLock::Held { .. } => None,
        WriterLock::Unlocked { path, reason } => Some(format!(
            "wrote to git notes without the cross-process lock (lock file {} \
             unusable: {reason}); concurrent memory writes in this repo can \
             lose entries",
            path.display()
        )),
    };

    // ── 1. Get HEAD sha ───────────────────────────────────────────────────────
    let head = run_git(git_root, &["rev-parse", "HEAD"])
        .await
        .map(|s| s.trim().to_string())?;

    // ── 2. Read existing note (may not exist) ─────────────────────────────────
    let existing = read_note_body_with_retry(git_root, &head)
        .await
        .context("could not read the existing note, so not overwriting it")?;

    // ── 3. Append new entry ───────────────────────────────────────────────────
    let new_line = serde_json::to_string(record)?;

    let combined = match existing {
        Some(body) if !body.trim().is_empty() => {
            format!("{}\n{}", body.trim_end_matches('\n'), new_line)
        }
        _ => new_line,
    };

    // ── 4. Write back ─────────────────────────────────────────────────────────
    // The note body is passed via stdin (`-F -`) rather than as a `-m` argv
    // value: this keeps arbitrary/attacker-influenced note content off the
    // process argv (and therefore out of `ps`/process-list visibility) and
    // means the body can never be misparsed as an option, regardless of its
    // contents. `--` guards the trailing `<object>` (HEAD sha) so it can't be
    // interpreted as an option either, even though `head` is always a
    // `rev-parse`-verified sha here.
    run_git_with_stdin(
        git_root,
        &[
            "notes",
            "--ref=spelunk",
            "add",
            "-f",
            "-F",
            "-",
            "--",
            &head,
        ],
        &combined,
    )
    .await?;

    Ok(AppendOutcome {
        rewrite_ref,
        lock_degradation,
    })
}

/// Append a state-update record for an entity that already exists on the
/// carrier: `base` supplies its content (`kind`/`title`/`body`/`tags`/
/// `linked_files`/`source_ref`/`valid_at`) unchanged, while `status`,
/// `invalid_at` and `superseded_by_entity_id` override its mutable state.
///
/// **Never rewrites the entity's existing line(s) in place.** A live-git
/// experiment (three-repo harness) showed why: a rewrite leaves a second
/// machine, which holds the original line plus a divergent local note of its
/// own, with both the rewritten and the stale original line after
/// `cat_sort_uniq` unions them — the entity appears twice, with conflicting
/// `status`. Appending a new line instead, and folding same-`entity_id` copies
/// at read time ([`fold_records`]), converges regardless of merge order
/// (ADR-068 A6): the fold's archival rule is monotonic, so whichever copy
/// carries `status: "archived"` wins.
///
/// This append is **not** the "re-recording an unchanged entry" case ADR-068
/// A6 calls a no-op — that no-op is scoped to a byte-for-byte-unchanged
/// re-record, and does not apply here since this call always changes mutable
/// state. Nothing in this module suppresses same-`entity_id` appends; keep it
/// that way; a guard that did would silently swallow every state update this
/// function writes.
///
/// Shared by three callers, all passing `superseded_by_entity_id: None`
/// except the supersede pair: `memory archive`'s carrier write-through
/// (`archive.rs`), `GitNotesBackend::archive` (git-notes as the primary
/// store, not the carrier), and `memory supersede` / `memory add
/// --supersedes` (the two carriers of a supersede edge, which do pass it).
pub async fn append_state_update(
    git_root: Option<&std::path::Path>,
    base: &Note,
    status: &str,
    invalid_at: Option<i64>,
    superseded_by_entity_id: Option<String>,
) -> Result<AppendOutcome> {
    let record = NoteRecord {
        schema_version: 1,
        id: now_millis(),
        kind: base.kind.clone(),
        title: base.title.clone(),
        body: base.body.clone(),
        tags: base.tags.clone(),
        linked_files: base.linked_files.clone(),
        created_at: now_secs(),
        status: status.to_string(),
        source_ref: base.source_ref.clone(),
        valid_at: base.valid_at,
        invalid_at,
        // Machine-local rowid link: never populated by this path, which keys
        // entities by `entity_id` only (ADR-068 A6).
        superseded_by: None,
        remote_id: None,
        entity_id: Some(note_entity_id(base)),
        superseded_by_entity_id,
    };
    append_to_git_notes(git_root, &record).await
}

// ── Read-path merge: making fetched notes visible ────────────────────────────

/// What [`merge_tracking_notes`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotesMergeOutcome {
    /// The merge ran; any fetched entries are now on the working ref.
    Merged,
    /// Nothing to merge, or the merge failed. The caller reads regardless.
    Skipped,
    /// The lock was unavailable, so the merge was skipped. The union is
    /// idempotent, so the next read catches up.
    LockUnavailable,
}

/// Merge fetched teammate notes ([`SPELUNK_TRACKING_REF`]) into the working ref
/// so `memory list` / `context` can see them.
///
/// Does **no** network. It merges only what the user's own `git fetch` already
/// wrote, which is what lets reads work with the remote unreachable and keeps
/// egress off a path the user never pointed at a remote (ADR-069 D5).
///
/// Never fails the caller: a read must not break because the merge could not
/// run. A missing tracking ref is nothing to do (git exits 128 when both refs
/// are empty, which is the un-fetched solo case), and an unavailable lock skips
/// the merge rather than waiting the caller out.
pub async fn merge_tracking_notes(git_root: Option<&std::path::Path>) -> NotesMergeOutcome {
    // Without this, a concurrent `append_to_git_notes` read-modify-write
    // silently overwrites the merged entries (#185 / ADR-069 D6). Unlike a
    // writer, every non-acquired outcome skips: the union is idempotent, so
    // the next read catches up, and a read must never fail over the lock.
    let _lock = match lock_notes(git_root).await {
        Ok(LockAttempt::Acquired(guard)) => guard,
        Ok(LockAttempt::Contended { .. }) | Ok(LockAttempt::Unusable { .. }) => {
            return NotesMergeOutcome::LockUnavailable;
        }
        Err(e) => {
            tracing::debug!("notes merge skipped, lock path unresolved: {e}");
            return NotesMergeOutcome::LockUnavailable;
        }
    };

    // `-s` is explicit on every call: the `notes.mergeStrategy` default is
    // `manual`, which exits 1 and leaves a stuck `.git/NOTES_MERGE_WORKTREE`.
    // The user's own setting is never written.
    match run_git(
        git_root,
        &[
            "notes",
            "--ref=spelunk",
            "merge",
            "-s",
            "cat_sort_uniq",
            SPELUNK_TRACKING_REF,
        ],
    )
    .await
    {
        Ok(_) => NotesMergeOutcome::Merged,
        Err(e) => {
            tracing::debug!("notes merge from {SPELUNK_TRACKING_REF} skipped: {e}");
            NotesMergeOutcome::Skipped
        }
    }
}

/// Run a git subprocess, optionally in `dir`, and return stdout as a `String`.
/// Returns `Err` if the process fails.
async fn run_git(dir: Option<&std::path::Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd.args(args).output().await?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(anyhow!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Run a git subprocess, optionally in `dir`, writing `stdin_data` to its
/// stdin and returning stdout as a `String`. Used with `-F -` invocations so
/// note bodies never appear on argv.
async fn run_git_with_stdin(
    dir: Option<&std::path::Path>,
    args: &[&str],
    stdin_data: &str,
) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open stdin for git {}", args.join(" ")))?;
        stdin.write_all(stdin_data.as_bytes()).await?;
        // Drop closes stdin so git sees EOF.
    }
    let out = child.wait_with_output().await?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(anyhow!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Hard cap on entries returned by `list()`.
///
/// Bounds output only, not work: the entity fold has to read every reachable
/// note blob whatever the limit.
/// Callers needing unbounded listing should use `--backend sqlite`.
const GIT_NOTES_MAX_LIST: usize = 500;

/// Memory backend backed by `git notes` in the `refs/notes/spelunk` namespace.
///
/// The note on a commit is JSON Lines: one `NoteRecord` per line, possibly
/// interleaved with foreign content (prose, other tools' lines). Reads skip
/// foreign lines; writes preserve them and every sibling record verbatim.
/// Multiple entries accumulate within a commit's note and across commits.
///
/// # Concurrency
/// `add` and `archive` both do read-modify-write and rewrite the note with
/// `git notes add -f`, appending a new JSON line rather than mutating an
/// existing one (`archive` appends a `status: "archived"` state-update via
/// [`append_state_update`], resolving its target by `entity_id` first, per
/// ADR-068 A6). Each is serialized by [`lock_notes`], which is keyed on the
/// git **common** dir so that worktrees sharing one notes ref contend on one
/// lock (#185).
///
/// # Unsupported methods
/// Semantic search (`search`, `search_hybrid`, `search_timeline`, `search_text`),
/// graph edges (`add_edge`, `get_edges`), `supersede`, `harvested_shas`, and
/// `has_source_ref` all return `Err` with a clear message rather than silently
/// returning empty results.
pub struct GitNotesBackend {
    git_root: Option<std::path::PathBuf>,
}

impl Default for GitNotesBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl GitNotesBackend {
    pub fn new() -> Self {
        Self { git_root: None }
    }

    /// Create a backend pinned to `root` — useful for testing with a temporary repo.
    pub fn with_root(root: std::path::PathBuf) -> Self {
        Self {
            git_root: Some(root),
        }
    }

    fn git(&self) -> Command {
        let mut cmd = Command::new("git");
        if let Some(ref root) = self.git_root {
            cmd.current_dir(root);
        }
        cmd
    }

    /// Exposes the configured root to the free-function carrier helpers
    /// (`append_state_update` et al.), which take `Option<&Path>` rather than
    /// `&GitNotesBackend`: they are shared with the SQLite-primary write-through
    /// path, which has no `GitNotesBackend` to borrow from.
    fn git_root(&self) -> Option<&std::path::Path> {
        self.git_root.as_deref()
    }

    async fn run(&self, args: &[&str]) -> Result<String> {
        let out = self.git().args(args).output().await?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(anyhow!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// Write a spelunk note body to `object` via `git notes add -f -F - --
    /// <object>`, passing `body` over stdin. Keeps note content (which may
    /// contain arbitrary user/LLM text) off argv, and the `--` separator
    /// stops `object` from being parsed as an option.
    async fn add_note_stdin(&self, object: &str, body: &str) -> Result<()> {
        let mut cmd = self.git();
        cmd.args([
            "notes",
            "--ref=spelunk",
            "add",
            "-f",
            "-F",
            "-",
            "--",
            object,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| anyhow!("failed to open stdin for git notes add"))?;
            stdin.write_all(body.as_bytes()).await?;
        }
        let out = child.wait_with_output().await?;
        if out.status.success() {
            Ok(())
        } else {
            Err(anyhow!(
                "git notes add failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    async fn head_sha(&self) -> Result<String> {
        Ok(self.run(&["rev-parse", "HEAD"]).await?.trim().to_string())
    }

    /// `(commit_sha, note_blob_sha)` for every commit reachable from HEAD that
    /// carries a spelunk note, in reverse-chronological (newest first) order.
    ///
    /// Only commits reachable from HEAD are listed: memory travels with the
    /// code that carries it, so a teammate's note on a fetched-but-unmerged
    /// commit stays invisible until that commit is merged.
    async fn noted_commits(&self) -> Result<Vec<(String, String)>> {
        // `git notes --ref=spelunk list` → "<note-blob-sha> <commit-sha>"
        let list_out = self
            .git()
            .args(["notes", "--ref=spelunk", "list"])
            .output()
            .await?;

        if !list_out.status.success() {
            return Ok(vec![]);
        }

        let listing = String::from_utf8_lossy(&list_out.stdout);
        let noted: HashMap<&str, &str> = listing
            .lines()
            .filter_map(|l| {
                let mut parts = l.split_whitespace();
                let blob = parts.next()?;
                let commit = parts.next()?;
                Some((commit, blob))
            })
            .collect();

        if noted.is_empty() {
            return Ok(vec![]);
        }

        let log_out = self.git().args(["log", "--format=%H"]).output().await?;

        if !log_out.status.success() {
            return Ok(vec![]);
        }

        let pairs = String::from_utf8_lossy(&log_out.stdout)
            .lines()
            .filter_map(|line| {
                let commit = line.trim();
                noted
                    .get(commit)
                    .map(|blob| (commit.to_owned(), (*blob).to_owned()))
            })
            .collect();

        Ok(pairs)
    }

    /// Note blob shas only, for the lenient batch read `folded_records` uses:
    /// listing/lookup reads must not break because one historical note is
    /// unreadable (see [`read_note_blobs`](Self::read_note_blobs)).
    async fn noted_blobs(&self) -> Result<Vec<String>> {
        Ok(self
            .noted_commits()
            .await?
            .into_iter()
            .map(|(_, blob)| blob)
            .collect())
    }

    /// Read every listed note blob in one `git cat-file --batch`, in the order
    /// given.
    ///
    /// The fold needs every reachable blob, so a per-commit `git notes show`
    /// would cost one subprocess each (~13 ms). Write paths keep `show`: they
    /// read exactly one note.
    async fn read_note_blobs(&self, blob_shas: &[String]) -> Result<Vec<String>> {
        if blob_shas.is_empty() {
            return Ok(vec![]);
        }

        let mut cmd = self.git();
        cmd.args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open stdin for git cat-file --batch"))?;

        let mut request = blob_shas.join("\n");
        request.push('\n');

        // Write and drain concurrently: git blocks once the stdout pipe fills,
        // so writing the whole request first would deadlock on a big enough repo.
        let writer = async move {
            stdin.write_all(request.as_bytes()).await?;
            stdin.shutdown().await
        };
        let (write_res, out) = tokio::join!(writer, child.wait_with_output());

        let out = out?;
        if !out.status.success() {
            return Err(anyhow!(
                "git cat-file --batch: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        write_res?;

        parse_cat_file_batch(&out.stdout)
    }

    /// Read the raw note blob for `commit_sha` (empty string if no note).
    ///
    /// A failed read is an `Err`, never an empty blob: `append_record` writes
    /// back what this returns, so conflating the two turns one transient git
    /// failure into a wiped note (#185).
    async fn read_note_blob(&self, commit_sha: &str) -> Result<String> {
        Ok(
            read_note_body_with_retry(self.git_root.as_deref(), commit_sha)
                .await?
                .unwrap_or_default(),
        )
    }

    /// Append `record` as a new JSON line to `object`'s note, preserving every
    /// existing line (spelunk records and foreign content) byte-for-byte.
    async fn append_record(&self, object: &str, record: &NoteRecord) -> Result<()> {
        // git notes is the primary store on this path (`--backend git-notes`),
        // so an unconfigured carry ref orphans the only copy. Status is dropped:
        // this path has no command output to announce on.
        ensure_notes_rewrite_ref(self.git_root.as_deref()).await;

        let _lock = writer_lock(self.git_root.as_deref()).await?;

        let existing = self.read_note_blob(object).await?;
        let new_line = serde_json::to_string(record)?;
        let combined = if existing.trim().is_empty() {
            new_line
        } else {
            format!("{}\n{}", existing.trim_end_matches('\n'), new_line)
        };
        self.add_note_stdin(object, &combined).await
    }

    /// Every entry on the ref, folded to one record per entity, no filtering
    /// or limit truncation — the shared basis for `collect()`'s filtered
    /// listing and `get()`'s single-entity lookup, so both see the same
    /// folded state (ADR-068 A6/E4): a record's `status`/
    /// `superseded_by_entity_id` must reflect every state-update appended
    /// for its entity (e.g. via `append_state_update`), not just whichever
    /// raw line happens to carry its original numeric `id`.
    ///
    /// The only site that sees every commit's records, so the only site that
    /// can fold an entity's copies together.
    async fn folded_records(&self) -> Result<Vec<NoteRecord>> {
        let blob_shas = self.noted_blobs().await?;

        let mut records = Vec::new();
        for blob in self.read_note_blobs(&blob_shas).await? {
            records.extend(parse_records(&blob)?);
        }

        // Fold before every filter below. Dropping an archived copy first would
        // leave a surviving active copy to resurrect the entity.
        Ok(fold_records(records))
    }

    /// Every spelunk record on the ref, each paired with the commit its note is
    /// anchored to (newest commit first). Unlike [`folded_records`] this keeps
    /// the per-commit provenance the `--source-ref` anchor lookup needs, so it
    /// does not fold; callers fold (or anchor) as they need.
    ///
    /// `noted_commits` and `read_note_blobs` share one order (the latter reads
    /// the former's blob shas in request order), so zipping them attributes each
    /// blob's records to the right commit.
    async fn records_with_commit(&self) -> Result<Vec<(String, NoteRecord)>> {
        let noted = self.noted_commits().await?;
        let blob_shas: Vec<String> = noted.iter().map(|(_, blob)| blob.clone()).collect();
        let blobs = self.read_note_blobs(&blob_shas).await?;

        let mut out = Vec::new();
        for ((commit, _blob), body) in noted.iter().zip(blobs.iter()) {
            for record in parse_records(body)? {
                out.push((commit.clone(), record));
            }
        }
        Ok(out)
    }

    /// The `entity_id`s of every entry whose memory note is anchored to a commit
    /// whose sha begins with `sha_prefix`.
    ///
    /// The anchor — the git-notes attachment (commit → note object) — is the
    /// only place a `memory add` entry records which commit it belongs to: its
    /// SQLite `source_ref` column stays NULL (that column is harvest provenance,
    /// ADR-062), so a `source_ref` column query can never surface it. Prefix
    /// matching mirrors that column's `LIKE 'prefix%'` semantics — a plain
    /// string prefix over the full commit sha — rather than git's own
    /// abbreviated-object resolution, so a prefix that is ambiguous to git still
    /// matches every noted commit it is a prefix of.
    pub async fn entity_ids_anchored_to(&self, sha_prefix: &str) -> Result<Vec<String>> {
        let records = self.records_with_commit().await?;
        Ok(fold::anchor_commits(&records)
            .into_iter()
            .filter(|(_entity, commit)| commit.starts_with(sha_prefix))
            .map(|(entity, _commit)| entity)
            .collect())
    }

    /// Note-anchored entries whose anchor commit begins with `sha_prefix`, as
    /// folded `Note`s. The git-notes analogue of the SQLite `source_ref` filter,
    /// used when git notes is the primary store (`--backend git-notes` / the
    /// pre-init carrier); the SQLite-primary path resolves the same anchors via
    /// [`entity_ids_anchored_to`] and reads the authoritative rows back instead.
    async fn list_anchored_to(
        &self,
        sha_prefix: &str,
        include_archived: bool,
        as_of: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Note>> {
        let records = self.records_with_commit().await?;
        let anchors = fold::anchor_commits(&records);
        // Fold across every commit so an entry's `status` reflects a
        // state-update appended on a later commit, not just its original line.
        let mut folded = fold_records(records.into_iter().map(|(_, r)| r).collect());

        folded.retain(|record| {
            anchors
                .get(&record.resolve_entity_id())
                .is_some_and(|commit| commit.starts_with(sha_prefix))
                && record_in_window(record, include_archived, as_of)
        });

        // Match `collect`'s ordering and newest-wins truncation exactly.
        folded.sort_by_key(|r| r.created_at);
        if folded.len() > limit {
            folded.drain(..folded.len() - limit);
        }
        Ok(folded.into_iter().map(record_to_note).collect())
    }

    async fn collect(
        &self,
        kind_filter: Option<&str>,
        include_archived: bool,
        as_of: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Note>> {
        let mut folded = self.folded_records().await?;

        folded.retain(|record| {
            if kind_filter.is_some_and(|k| record.kind != k) {
                return false;
            }
            record_in_window(record, include_archived, as_of)
        });

        // Stable over first-encounter order, so ties keep blob order (D2).
        folded.sort_by_key(|r| r.created_at);
        if folded.len() > limit {
            // Keep the newest, as the sqlite backend's `ORDER BY created_at
            // DESC LIMIT` does. Folding first is what makes this exact.
            folded.drain(..folded.len() - limit);
        }

        Ok(folded.into_iter().map(record_to_note).collect())
    }
}

/// Whether a folded record survives the archived / point-in-time gate shared by
/// [`GitNotesBackend::collect`] and [`GitNotesBackend::list_anchored_to`].
/// `kind` filtering (only `collect` does it) stays with the caller.
///
/// A point-in-time (`as_of`) query is governed entirely by the temporal window,
/// independent of archived status: an entry archived or superseded AFTER T was
/// live at T and must be returned, so the archived gate is skipped whenever
/// `as_of` is set and `include_archived` then only affects the current-view
/// listing.
fn record_in_window(record: &NoteRecord, include_archived: bool, as_of: Option<i64>) -> bool {
    if let Some(ts) = as_of {
        let effective = record.valid_at.unwrap_or(record.created_at);
        if effective > ts {
            return false;
        }
        if record.invalid_at.is_some_and(|ia| ia <= ts) {
            return false;
        }
        return true;
    }
    if !include_archived && record.status == "archived" {
        return false;
    }
    true
}

/// Permissively parse the spelunk records from one note blob.
///
/// The blob is JSON Lines interleaved with foreign content (prose, other
/// tools' lines). Foreign lines are skipped without error; only a record from
/// a newer, incompatible `schema_version` returns an error.
fn parse_records(blob: &str) -> Result<Vec<NoteRecord>> {
    let mut records = Vec::new();
    for line in blob.lines() {
        match parse_spelunk_line(line) {
            Some(record) => {
                if record.schema_version > 1 {
                    return Err(anyhow::Error::new(
                        crate::error::SpelunkError::SchemaMismatch {
                            found: record.schema_version,
                            max_known: 1,
                        },
                    ));
                }
                records.push(record);
            }
            None => continue, // foreign line: skip, never error
        }
    }
    Ok(records)
}

/// Split `git cat-file --batch` output into one body per requested object.
///
/// Each record is `<sha> <type> <size>\n<size bytes>\n`. The size header is the
/// only safe delimiter: a note body contains newlines of its own.
fn parse_cat_file_batch(out: &[u8]) -> Result<Vec<String>> {
    let mut bodies = Vec::new();
    let mut rest = out;

    while !rest.is_empty() {
        let nl = rest
            .iter()
            .position(|&b| b == b'\n')
            .ok_or_else(|| anyhow!("git cat-file --batch: header with no newline"))?;
        let header = String::from_utf8_lossy(&rest[..nl]).into_owned();
        rest = &rest[nl + 1..];

        let fields: Vec<&str> = header.split(' ').collect();
        match fields.as_slice() {
            // "<sha> missing" / "<sha> ambiguous": no body follows. A read must
            // not break on one unreadable note.
            [_, _] => bodies.push(String::new()),
            [_, _, size] => {
                let size: usize = size
                    .parse()
                    .map_err(|_| anyhow!("git cat-file --batch: bad size in {header:?}"))?;
                if rest.len() < size + 1 {
                    return Err(anyhow!("git cat-file --batch: truncated body"));
                }
                bodies.push(String::from_utf8_lossy(&rest[..size]).into_owned());
                rest = &rest[size + 1..];
            }
            _ => {
                return Err(anyhow!(
                    "git cat-file --batch: unexpected header {header:?}"
                ));
            }
        }
    }

    Ok(bodies)
}

/// Classify one line of a note blob: `Some(record)` if it parses as a JSON
/// *object* deserializing into `NoteRecord`. Non-JSON, non-object JSON, blank,
/// and prose lines are foreign (`None`).
fn parse_spelunk_line(line: &str) -> Option<NoteRecord> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Must be a JSON object; arrays/strings/numbers/null are foreign.
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    if !value.is_object() {
        return None;
    }
    serde_json::from_value(value).ok()
}

/// The framing `git cat-file --batch` emits, pinned against git 2.55.
#[cfg(test)]
mod cat_file_batch {
    use super::*;

    /// One object: `<sha> <type> <size>\n<body>\n`.
    fn framed(sha: &str, body: &str) -> String {
        format!("{sha} blob {}\n{body}\n", body.len())
    }

    #[test]
    fn empty_output_yields_no_bodies() {
        assert!(parse_cat_file_batch(b"").expect("parse").is_empty());
    }

    /// A note body holds newlines of its own, so only the size header can
    /// delimit it: a body line that mimics a header must not split it.
    #[test]
    fn a_body_that_mimics_a_header_is_not_split() {
        let body = "line one\ndeadbeef blob 99\nline three";

        assert_eq!(
            parse_cat_file_batch(framed("aaa", body).as_bytes()).expect("parse"),
            vec![body]
        );
    }

    /// One unreadable note must not fail the whole read: git reports
    /// `<sha> missing` with no body and still exits 0.
    #[test]
    fn a_missing_object_yields_an_empty_body_and_the_batch_survives() {
        let out = format!(
            "{}bbb missing\n{}",
            framed("aaa", "first"),
            framed("ccc", "third")
        );

        assert_eq!(
            parse_cat_file_batch(out.as_bytes()).expect("parse"),
            vec!["first", "", "third"]
        );
    }

    /// `git notes add --allow-empty` writes the empty blob. Git still emits the
    /// body's trailing newline, so a zero-length body must not read as truncated.
    #[test]
    fn an_empty_blob_parses_as_an_empty_body() {
        assert_eq!(
            parse_cat_file_batch(b"aaa blob 0\n\n").expect("parse"),
            vec![""]
        );
    }

    #[test]
    fn a_body_shorter_than_its_header_claims_is_an_error() {
        assert!(parse_cat_file_batch(b"aaa blob 99\nshort\n").is_err());
    }

    /// Records are consumed in request order, so a body can never be attributed
    /// to the wrong note.
    #[test]
    fn bodies_come_back_in_request_order() {
        let out = format!("{}{}", framed("aaa", "one"), framed("bbb", "two"));

        assert_eq!(
            parse_cat_file_batch(out.as_bytes()).expect("parse"),
            vec!["one", "two"]
        );
    }

    /// A repo carrying one note, and that note's blob sha.
    fn repo_with_one_note() -> (tempfile::TempDir, String) {
        crate::test_support::isolate_git_config();
        let dir = tempfile::TempDir::new().expect("tempdir");
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            out
        };

        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "T"]);
        std::fs::write(dir.path().join("README.md"), "x").expect("write");
        run(&["add", "."]);
        run(&["commit", "--no-gpg-sign", "-m", "first"]);
        run(&["notes", "--ref=spelunk", "add", "-m", "one\ntwo", "HEAD"]);

        let listing =
            String::from_utf8(run(&["notes", "--ref=spelunk", "list"]).stdout).expect("utf8");
        let blob = listing
            .split_whitespace()
            .next()
            .expect("a note blob sha")
            .to_string();

        (dir, blob)
    }

    /// The batch has to drain stdout while it writes stdin. Sending the whole
    /// request first deadlocks: git stops reading once its stdout pipe fills,
    /// and the fold reads every reachable blob, so `GIT_NOTES_MAX_LIST` does not
    /// bound the request size.
    ///
    /// 5000 shas is ~205 KiB in and ~340 KiB out, past the 64 KiB pipe buffer
    /// both ways. A regression here hangs, so the read is bounded to fail loudly.
    #[tokio::test]
    async fn a_request_past_the_pipe_buffer_does_not_deadlock() {
        let (dir, blob) = repo_with_one_note();
        let backend = GitNotesBackend::with_root(dir.path().to_path_buf());

        let bodies = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            backend.read_note_blobs(&vec![blob; 5000]),
        )
        .await
        .expect("deadlocked: stdout must drain while stdin is written")
        .expect("read");

        assert_eq!(bodies.len(), 5000, "one body per requested sha");
        assert!(
            bodies.iter().all(|b| b == "one\ntwo\n"),
            "every body must survive the batch intact"
        );
    }
}
