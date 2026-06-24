use anyhow::{Result, anyhow};
use std::collections::HashSet;
use tokio::process::Command;

use super::memory::Note;
use super::note_record::{NoteRecord, record_to_note};

mod backend_impl;

// ── Write-through helper (free function) ─────────────────────────────────────

/// Append a `NoteRecord` as a JSON line to `refs/notes/spelunk` on HEAD.
///
/// Implements read-modify-write with append semantics:
/// 1. Read the existing note blob for HEAD (may be absent).
/// 2. Parse each line as a JSON `NoteRecord` — ignore malformed lines.
/// 3. Append the new record as a new JSON line.
/// 4. Write the combined text back with `git notes add -f`.
///
/// Errors are intentionally non-fatal: the caller should log `tracing::warn!`
/// and continue.  This function returns `Ok(())` on success or propagates
/// an error for the caller to handle gracefully.
///
/// # Arguments
/// * `git_root` — directory passed to `git -C`; `None` uses the process CWD.
/// * `record` — the entry to append.
pub async fn append_to_git_notes(
    git_root: Option<&std::path::Path>,
    record: &NoteRecord,
) -> Result<()> {
    // ── 1. Get HEAD sha ───────────────────────────────────────────────────────
    let head = run_git(git_root, &["rev-parse", "HEAD"])
        .await
        .map(|s| s.trim().to_string())?;

    // ── 2. Read existing note (may not exist) ─────────────────────────────────
    let existing = run_git(git_root, &["notes", "--ref=spelunk", "show", &head])
        .await
        .unwrap_or_default();

    // ── 3. Append new entry ───────────────────────────────────────────────────
    let new_line = serde_json::to_string(record)?;

    let combined = if existing.trim().is_empty() {
        new_line
    } else {
        format!("{}\n{}", existing.trim_end_matches('\n'), new_line)
    };

    // ── 4. Write back ─────────────────────────────────────────────────────────
    run_git(
        git_root,
        &[
            "notes",
            "--ref=spelunk",
            "add",
            "-f",
            "-m",
            &combined,
            &head,
        ],
    )
    .await?;

    Ok(())
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

/// Hard cap on entries returned by `list()`.
///
/// Each entry requires one `git notes show` subprocess call (~13 ms).
/// Without a guard, `list(5000)` would take ~65 seconds.
/// Callers needing unbounded listing should use `--backend sqlite`.
const GIT_NOTES_MAX_LIST: usize = 500;

/// Memory backend backed by `git notes` in the `refs/notes/spelunk` namespace.
///
/// Each memory entry is attached to the `HEAD` commit at write time as a single
/// JSON object.  Multiple entries accumulate across commits as the user works.
///
/// # Concurrency warning
/// `add` uses `git notes add -f` (force-replace).  If two processes add a note
/// to the same `HEAD` simultaneously the second write silently overwrites the
/// first.  For multi-agent workflows use the sqlite backend (the default).
/// See issue #185 for the full analysis.
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

    async fn head_sha(&self) -> Result<String> {
        Ok(self.run(&["rev-parse", "HEAD"]).await?.trim().to_string())
    }

    /// Return (commit-sha, commit-timestamp) pairs for commits that have a
    /// spelunk note, in reverse-chronological (newest first) order.
    async fn noted_commits(&self) -> Result<Vec<(String, i64)>> {
        // `git notes --ref=spelunk list` → "<note-blob-sha> <commit-sha>"
        let list_out = self
            .git()
            .args(["notes", "--ref=spelunk", "list"])
            .output()
            .await?;

        if !list_out.status.success() {
            return Ok(vec![]);
        }

        let noted: HashSet<String> = String::from_utf8_lossy(&list_out.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().nth(1).map(str::to_owned))
            .collect();

        if noted.is_empty() {
            return Ok(vec![]);
        }

        // Walk git log in reverse-chronological order to get commit timestamps.
        let log_out = self.git().args(["log", "--format=%H %at"]).output().await?;

        if !log_out.status.success() {
            return Ok(vec![]);
        }

        let pairs = String::from_utf8_lossy(&log_out.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let sha = parts.next()?.to_owned();
                let ts: i64 = parts.next()?.parse().ok()?;
                noted.contains(&sha).then_some((sha, ts))
            })
            .collect();

        Ok(pairs)
    }

    async fn read_record(&self, commit_sha: &str) -> Result<Option<NoteRecord>> {
        let out = self
            .git()
            .args(["notes", "--ref=spelunk", "show", commit_sha])
            .output()
            .await?;

        if !out.status.success() {
            return Ok(None);
        }

        let json = String::from_utf8_lossy(&out.stdout);
        let record: NoteRecord = serde_json::from_str(json.trim())
            .map_err(|e| anyhow!("parsing spelunk note on {commit_sha}: {e}"))?;
        if record.schema_version > 1 {
            return Err(anyhow::Error::new(
                crate::error::SpelunkError::SchemaMismatch {
                    found: record.schema_version,
                    max_known: 1,
                },
            ));
        }
        Ok(Some(record))
    }

    async fn collect(
        &self,
        kind_filter: Option<&str>,
        include_archived: bool,
        as_of: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Note>> {
        let commits = self.noted_commits().await?;
        let mut notes = Vec::new();

        for (sha, _) in commits {
            if notes.len() >= limit {
                break;
            }
            if let Some(record) = self.read_record(&sha).await? {
                if kind_filter.is_some_and(|k| record.kind != k) {
                    continue;
                }
                if !include_archived && record.status == "archived" {
                    continue;
                }
                if let Some(ts) = as_of {
                    let effective = record.valid_at.unwrap_or(record.created_at);
                    if effective > ts {
                        continue;
                    }
                    if record.invalid_at.is_some_and(|ia| ia <= ts) {
                        continue;
                    }
                }
                notes.push(record_to_note(record));
            }
        }

        Ok(notes)
    }
}
