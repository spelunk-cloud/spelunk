//! Cross-process lock serializing the `refs/notes/spelunk` read-modify-write.
//!
//! Git's own ref locking cannot help here: the loss happens at the content
//! layer, not the ref layer, and racing writers each hold the ref lock
//! legitimately in turn. See issue #185 and ADR-069 (D6).
//!
//! Contention policy is the caller's, per ADR-069 D8: a writer that is handed
//! [`LockAttempt::Contended`] must fail, never write unlocked; idempotent
//! callers (the read-path merge, publish) skip and report.

use anyhow::Result;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Lock file name, created inside the git **common** dir.
const LOCK_FILE_NAME: &str = "spelunk-notes.lock";

/// Bounded wait before giving up on a contended lock.
///
/// The OS releases an advisory lock when its holder exits, so there is no
/// crashed-holder case to time out for. Reaching this means either a stuck
/// live holder or a queue of legitimate writers on a slow machine (observed
/// on CI: 8 serialized appends exceed 5s when process spawns are expensive).
/// Either way expiry is reported to the caller, never silently downgraded
/// (ADR-069 D8); a failed writer retries, a wedged holder is a bug.
pub const LOCK_WAIT_BUDGET: Duration = Duration::from_secs(5);

/// Poll interval while the lock is held by another process.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Holds the notes lock for its lifetime; the lock is released on drop.
#[derive(Debug)]
pub struct NotesLock {
    // Dropping the File closes the fd, which releases the OS lock.
    _file: File,
    path: PathBuf,
}

impl NotesLock {
    /// The lock file this guard holds.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// One attempt to take the notes lock (ADR-069 D8).
///
/// The non-acquired arms are distinct because they demand different answers
/// from a writer: `Contended` means serialization exists and someone else has
/// it, so writing anyway is the #185 data loss; `Unusable` means serialization
/// cannot exist here at all.
#[must_use]
#[derive(Debug)]
pub enum LockAttempt {
    /// Held until the guard drops.
    Acquired(NotesLock),
    /// Another holder kept it past [`LOCK_WAIT_BUDGET`]. A writer must fail;
    /// idempotent work skips and reports.
    Contended { path: PathBuf },
    /// The lock file cannot be opened or locked on this filesystem, so
    /// serialization is impossible here. Writers proceed unlocked, loudly;
    /// this is the one degradation kept, and it is kept narrow.
    Unusable { path: PathBuf, reason: String },
}

/// Resolve the lock path: `<git-common-dir>/spelunk-notes.lock`.
///
/// The **common** dir, not the per-worktree git dir: worktrees share one
/// `refs/notes/spelunk`, so a per-worktree lock would fail to serialize the
/// actual contenders.
async fn notes_lock_path(git_root: Option<&Path>) -> Result<PathBuf> {
    // `--path-format=absolute` (git >= 2.31) answers absolute from a main and
    // a linked worktree alike, so every contender computes one identity.
    let flagged = super::run_git(
        git_root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await;

    let common_dir = match flagged.ok().and_then(|out| parse_absolute_dir(&out)) {
        Some(dir) => dir,
        // Older git. Note it does not reject the flag: rev-parse echoes
        // unknown options back with exit 0, which is why the parse above
        // validates the output instead of trusting the exit code.
        None => {
            let raw = super::run_git(git_root, &["rev-parse", "--git-common-dir"]).await?;
            let raw = Path::new(raw.trim());

            // git may answer with a path relative to the dir it ran in.
            if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                resolve_relative_common_dir(raw, git_root)?
            }
        }
    };

    // One spelling for every contender: case, separators, 8.3 short names and
    // symlinks can otherwise differ per worktree, most visibly on Windows.
    let common_dir = std::fs::canonicalize(&common_dir).unwrap_or(common_dir);

    Ok(common_dir.join(LOCK_FILE_NAME))
}

/// Join a relative `--git-common-dir` answer against the caller's `git_root`.
///
/// A `None` git_root must never be resolved against the ambient process CWD:
/// that CWD can change between the git subprocess that produced `raw` and
/// this call, on any thread, in any process running spelunk-cli's test
/// binary or the CLI itself. Erroring here forces every caller to supply an
/// explicit root instead of racing.
fn resolve_relative_common_dir(raw: &Path, git_root: Option<&Path>) -> Result<PathBuf> {
    match git_root {
        Some(root) => Ok(root.join(raw)),
        None => Err(anyhow::anyhow!(
            "git answered a relative --git-common-dir ({}) but no git_root was \
             given to resolve it against; refusing to guess via the ambient \
             process working directory",
            raw.display()
        )),
    }
}

/// The single absolute path in `rev-parse --path-format=absolute` output, or
/// `None` when the output is an echoed unknown flag (git < 2.31).
fn parse_absolute_dir(out: &str) -> Option<PathBuf> {
    let mut lines = out.lines().filter(|l| !l.trim().is_empty());
    let first = lines.next()?.trim();
    if lines.next().is_some() || first.starts_with("--") {
        return None;
    }
    let p = Path::new(first);
    p.is_absolute().then(|| p.to_path_buf())
}

/// Acquire the notes lock, waiting up to [`LOCK_WAIT_BUDGET`].
///
/// `Err` means the lock path could not be resolved, which is git itself
/// failing; a writer's own git calls are about to fail the same way, so this
/// surfaces rather than degrades.
///
/// Held across the whole read-modify-write, not just the write: the race is
/// the gap between reading the note body and writing it back.
pub async fn lock_notes(git_root: Option<&Path>) -> Result<LockAttempt> {
    let path = notes_lock_path(git_root).await?;

    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            return Ok(LockAttempt::Unusable {
                path,
                reason: format!("could not open: {e}"),
            });
        }
    };

    let deadline = Instant::now() + LOCK_WAIT_BUDGET;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(LockAttempt::Acquired(NotesLock { _file: file, path })),
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Ok(LockAttempt::Contended { path });
                }
                tokio::time::sleep(LOCK_POLL_INTERVAL).await;
            }
            Err(TryLockError::Error(e)) => {
                return Ok(LockAttempt::Unusable {
                    path,
                    reason: format!("could not lock: {e}"),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// git < 2.31 echoes the unknown flag back with exit 0, so the fallback
    /// trigger is the output shape, not the exit code. Trusting the exit code
    /// would aim the lock at a path spelled `--path-format=absolute`.
    #[test]
    fn echoed_flag_output_is_rejected() {
        assert_eq!(parse_absolute_dir("--path-format=absolute\n.git\n"), None);
        assert_eq!(parse_absolute_dir(".git\n"), None, "relative is rejected");
        assert_eq!(parse_absolute_dir(""), None);
    }

    #[test]
    fn a_single_absolute_line_is_accepted() {
        #[cfg(unix)]
        assert_eq!(
            parse_absolute_dir("/repo/.git\n"),
            Some(PathBuf::from("/repo/.git"))
        );
        #[cfg(windows)]
        assert_eq!(
            parse_absolute_dir("C:/repo/.git\n"),
            Some(PathBuf::from("C:/repo/.git"))
        );
    }

    #[test]
    fn relative_common_dir_joins_against_the_given_git_root() {
        #[cfg(unix)]
        let root = Path::new("/repo");
        #[cfg(windows)]
        let root = Path::new("C:/repo");
        let raw = Path::new(".git");
        assert_eq!(
            resolve_relative_common_dir(raw, Some(root)).unwrap(),
            root.join(raw)
        );
    }

    #[test]
    fn relative_common_dir_errors_without_a_git_root() {
        let raw = Path::new(".git");
        assert!(
            resolve_relative_common_dir(raw, None).is_err(),
            "a relative git-common-dir with no caller-supplied root must not be \
             guessed against the ambient process CWD"
        );
    }
}
