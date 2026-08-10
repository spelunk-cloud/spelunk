//! Cross-process advisory lock serializing whole `spelunk index` runs against
//! one project's DB.
//!
//! Two `spelunk index` processes racing on the same project reproducibly
//! corrupt `index.db` (`SQLITE_CORRUPT`, not merely `SQLITE_BUSY`) - neither
//! `index.db` nor `memory.db` nor `registry.db` sets `PRAGMA busy_timeout`
//! anywhere in this codebase, and SQLite's own per-connection locking does
//! not prevent two independent processes from interleaving writes across a
//! whole multi-transaction run. Serializing whole runs with an OS advisory
//! lock (the same mechanism as `storage::git_notes::lock`) closes that
//! window without needing SQLite itself to change.

use anyhow::Result;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::{Duration, Instant};

const LOCK_FILE_NAME: &str = "index.lock";
/// Holder pid, written and read separately from `LOCK_FILE_NAME` itself:
/// Windows' `LockFileEx` denies even a plain read from a second handle
/// against a locked file's exclusive byte range, unlike a POSIX advisory
/// `flock`, which only blocks other `flock` callers, not ordinary reads. A
/// second process's `holder_pid` lookup would silently degrade to `None` on
/// Windows if it read the locked file itself. This sidecar is never locked,
/// so the read-back works identically on every platform.
const LOCK_PID_FILE_NAME: &str = "index.lock.pid";

/// Held for the lifetime of one `spelunk index` process's DB-writing work.
/// Dropping releases the OS advisory lock (the fd closes), so a killed
/// holder never wedges a future run - there is no stale-lock case to detect
/// or clean up.
pub struct IndexRunLock {
    _file: File,
}

pub enum LockOutcome {
    Acquired(IndexRunLock),
    /// Another process holds the lock. `holder_pid` is best-effort (read
    /// back from the lock file's contents) and purely for the error message
    /// shown to the user - the OS lock itself, not this recorded pid, is
    /// what actually excludes a concurrent writer.
    HeldByOther {
        holder_pid: Option<u32>,
    },
}

/// Try to take the per-project index lock inside `spelunk_dir` (the
/// project's `.spelunk/` directory), non-blocking.
///
/// Non-blocking rather than waited-out (contrast `git_notes::lock`'s bounded
/// poll): an index run's writing window is unbounded - a large repo can
/// embed for minutes - so waiting on it would make a second invocation hang
/// unpredictably instead of failing fast with an actionable message.
pub fn try_acquire(spelunk_dir: &Path) -> Result<LockOutcome> {
    std::fs::create_dir_all(spelunk_dir)?;
    let path = spelunk_dir.join(LOCK_FILE_NAME);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;

    match file.try_lock() {
        Ok(()) => {
            let pid_path = spelunk_dir.join(LOCK_PID_FILE_NAME);
            std::fs::write(&pid_path, std::process::id().to_string()).ok();
            Ok(LockOutcome::Acquired(IndexRunLock { _file: file }))
        }
        Err(TryLockError::WouldBlock) => {
            let pid_path = spelunk_dir.join(LOCK_PID_FILE_NAME);
            let holder_pid = std::fs::read_to_string(&pid_path)
                .ok()
                .and_then(|s| s.trim().parse().ok());
            Ok(LockOutcome::HeldByOther { holder_pid })
        }
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Best-effort read of the pid last written to the lock file, regardless of
/// whether the lock is currently held: `try_acquire` (re)writes this on
/// every successful acquire, so it reflects the most recent holder even
/// after that holder has since released.
fn read_recorded_pid(spelunk_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(spelunk_dir.join(LOCK_PID_FILE_NAME))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Poll for `expected_pid` to appear as the lock file's recorded holder,
/// giving up after `timeout`. A caller that just dropped its own hold and
/// spawned a continuation process uses this to confirm that process -
/// specifically - became the new holder, rather than an unrelated third
/// process that raced into the gap between the drop and the continuation's
/// own acquire attempt, before reporting the handoff as a success.
pub fn wait_for_holder_pid(
    spelunk_dir: &Path,
    expected_pid: u32,
    timeout: Duration,
    poll_interval: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if read_recorded_pid(spelunk_dir) == Some(expected_pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(poll_interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_on_the_same_dir_is_held_by_other() {
        let dir = tempfile::tempdir().unwrap();
        let first = try_acquire(dir.path()).expect("first acquire");
        assert!(matches!(first, LockOutcome::Acquired(_)));

        let second = try_acquire(dir.path()).expect("second acquire attempt");
        assert!(
            matches!(second, LockOutcome::HeldByOther { .. }),
            "a live holder must make a concurrent acquire report contention, not succeed"
        );
    }

    #[test]
    fn holder_pid_is_recorded_for_the_error_message() {
        let dir = tempfile::tempdir().unwrap();
        let _first = try_acquire(dir.path()).expect("first acquire");

        let second = try_acquire(dir.path()).expect("second acquire attempt");
        match second {
            LockOutcome::HeldByOther { holder_pid } => {
                assert_eq!(
                    holder_pid,
                    Some(std::process::id()),
                    "the holder pid recorded in the lock file must be this test process's own \
                     pid (it holds the lock via `first`)"
                );
            }
            LockOutcome::Acquired(_) => panic!("must be held by other"),
        }
    }

    #[test]
    fn lock_is_released_when_the_guard_drops() {
        let dir = tempfile::tempdir().unwrap();
        {
            let first = try_acquire(dir.path()).expect("first acquire");
            assert!(matches!(first, LockOutcome::Acquired(_)));
        } // guard drops here, releasing the OS lock

        let second = try_acquire(dir.path()).expect("second acquire attempt");
        assert!(
            matches!(second, LockOutcome::Acquired(_)),
            "once the first guard drops, a fresh acquire must succeed"
        );
    }

    // ── wait_for_holder_pid: handoff-confirmation polling ────────────────────

    #[test]
    fn wait_for_holder_pid_returns_true_once_content_already_matches() {
        let dir = tempfile::tempdir().unwrap();
        let _held = try_acquire(dir.path()).expect("acquire");
        assert!(wait_for_holder_pid(
            dir.path(),
            std::process::id(),
            Duration::from_millis(200),
            Duration::from_millis(5),
        ));
    }

    #[test]
    fn wait_for_holder_pid_times_out_when_the_recorded_pid_never_matches() {
        let dir = tempfile::tempdir().unwrap();
        // Records our own pid - stands in for the "some other process holds
        // it" case: the pid polled for below is never the one recorded.
        let _held = try_acquire(dir.path()).expect("acquire");

        let started = Instant::now();
        let confirmed = wait_for_holder_pid(
            dir.path(),
            std::process::id().wrapping_add(1),
            Duration::from_millis(150),
            Duration::from_millis(10),
        );
        assert!(
            !confirmed,
            "must not confirm a pid that was never the recorded holder"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "must wait out the full timeout rather than returning early"
        );
    }

    #[test]
    fn wait_for_holder_pid_detects_a_holder_that_appears_after_a_delay() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        let pid = std::process::id();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            let held = try_acquire(&dir_path).expect("delayed acquire");
            std::thread::sleep(Duration::from_millis(500));
            drop(held);
        });

        assert!(
            wait_for_holder_pid(
                dir.path(),
                pid,
                Duration::from_millis(500),
                Duration::from_millis(10),
            ),
            "must detect a holder that appears mid-poll, not just one already present at the \
             first check"
        );
    }
}
