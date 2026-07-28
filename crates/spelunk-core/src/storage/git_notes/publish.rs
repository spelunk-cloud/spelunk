//! Publish `refs/notes/spelunk` to a remote: fetch, union-merge, push.
//!
//! Driven by the opt-in pre-push hook, which is a shim around
//! `spelunk plumbing publish-notes` (ADR-069 D1/D3/D7).

use anyhow::{Result, anyhow};
use std::path::Path;
use tokio::process::Command;

use super::{NotesMergeOutcome, SPELUNK_NOTES_REF, SPELUNK_TRACKING_REF, merge_tracking_notes};

/// Set on the nested notes push. `--no-verify` is the real recursion guard; a
/// hook that re-enters despite it stops here.
const NOTES_PUSH_SENTINEL: &str = "SPELUNK_NOTES_PUSH";

/// Under a concurrent 3-way race the third developer only won on attempt 3.
const MAX_PUSH_ATTEMPTS: u32 = 3;

/// What [`publish_notes`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    /// The notes ref reached the remote, on `attempts` pushes.
    Published { attempts: u32 },
    /// Nothing reached the remote; the reason says why.
    Skipped(SkipReason),
}

/// Why [`publish_notes`] had nothing to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Re-entered from the nested notes push.
    Recursion,
    /// No `refs/notes/spelunk` in this repo.
    NoLocalNotes,
    /// The named remote does not resolve. `git push <url>` reaches here.
    NoSuchRemote,
    /// The notes lock was unavailable, so the merge did not run. Nothing is
    /// lost: the records stay on the local ref and publish on the next push.
    LockUnavailable,
}

impl SkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            SkipReason::Recursion => "recursion",
            SkipReason::NoLocalNotes => "no_local_notes",
            SkipReason::NoSuchRemote => "no_such_remote",
            SkipReason::LockUnavailable => "lock_unavailable",
        }
    }
}

/// Publish this repo's memory notes to `remote`.
///
/// Fetches the remote's notes onto the tracking ref, unions them into the
/// working ref under the notes lock, and pushes the result. A lost race is
/// retried up to [`MAX_PUSH_ATTEMPTS`] times.
///
/// Returns `Err` when publishing genuinely failed. Callers driving the pre-push
/// hook must not propagate that as a non-zero exit: a hook exiting non-zero
/// aborts the user's branch push outright (ADR-069 D3).
pub async fn publish_notes(git_root: Option<&Path>, remote: &str) -> Result<PublishOutcome> {
    if std::env::var_os(NOTES_PUSH_SENTINEL).is_some() {
        return Ok(PublishOutcome::Skipped(SkipReason::Recursion));
    }

    if git(
        git_root,
        &["rev-parse", "--verify", "--quiet", SPELUNK_NOTES_REF],
        &[],
    )
    .await
    .is_err()
    {
        return Ok(PublishOutcome::Skipped(SkipReason::NoLocalNotes));
    }

    if git(git_root, &["remote", "get-url", remote], &[])
        .await
        .is_err()
    {
        return Ok(PublishOutcome::Skipped(SkipReason::NoSuchRemote));
    }

    // Onto the tracking ref, never over the working ref: a `+` there
    // force-updates it and silently drops local unpushed notes (D4).
    let fetch_refspec = format!("+{SPELUNK_NOTES_REF}:{SPELUNK_TRACKING_REF}");
    let push_refspec = format!("{SPELUNK_NOTES_REF}:{SPELUNK_NOTES_REF}");

    let mut last_err = String::new();
    for attempt in 1..=MAX_PUSH_ATTEMPTS {
        // A fetch failure is not fatal: the push below is never forced, so at
        // worst it is rejected and nothing of the remote's is lost.
        let _ = git(git_root, &["fetch", "--quiet", remote, &fetch_refspec], &[]).await;

        // Takes the notes lock, so a concurrent `memory add` cannot overwrite
        // the merged entries with its read-modify-write (D6).
        //
        // The merge is what carries the remote's side, so pushing without it
        // offers a still-diverged ref: the rejection that follows describes a
        // race that never happened. Skip instead, and never report the skip as
        // a publish.
        if merge_tracking_notes(git_root).await == NotesMergeOutcome::LockUnavailable {
            return Ok(PublishOutcome::Skipped(SkipReason::LockUnavailable));
        }

        // Never forced: the union above already carries both sides, so forcing
        // could only discard a teammate's memory.
        match git(
            git_root,
            &["push", "--no-verify", "--quiet", remote, &push_refspec],
            &[(NOTES_PUSH_SENTINEL, "1")],
        )
        .await
        {
            Ok(_) => return Ok(PublishOutcome::Published { attempts: attempt }),
            Err(stderr) => {
                if !is_lost_race(&stderr) {
                    return Err(publish_error(remote, &stderr));
                }
                last_err = stderr;
            }
        }
    }

    Err(publish_error(remote, &last_err))
}

fn publish_error(remote: &str, stderr: &str) -> anyhow::Error {
    anyhow!(
        "could not publish memory notes to '{remote}': {}",
        stderr.trim()
    )
}

/// Whether a failed notes push was a lost race, and so worth retrying.
///
/// Stays narrow deliberately: offline and a rejecting remote fail identically
/// three times, so widening this parks the user's push behind three timeouts
/// instead of one (D3).
fn is_lost_race(stderr: &str) -> bool {
    stderr.contains("non-fast-forward") || stderr.contains("fetch first")
}

/// Run git in `dir` with `envs` set. `Ok` is stdout, `Err` is stderr.
async fn git(
    dir: Option<&Path>,
    args: &[&str],
    envs: &[(&str, &str)],
) -> std::result::Result<String, String> {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    match cmd.args(args).output().await {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => Err(String::from_utf8_lossy(&out.stderr).into_owned()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// git's wording for a lost race, pinned against 2.55.0. A predicate that
    /// stops matching these turns a recoverable race into a lost entry.
    #[test]
    fn a_lost_race_is_retried() {
        assert!(is_lost_race(
            " ! [rejected]        refs/notes/spelunk -> refs/notes/spelunk (non-fast-forward)"
        ));
        assert!(is_lost_race(
            " ! [rejected]        refs/notes/spelunk -> refs/notes/spelunk (fetch first)"
        ));
    }

    /// Everything else fails identically three times, so retrying it only costs
    /// the user two more timeouts before the same warning.
    #[test]
    fn nothing_else_is_retried() {
        assert!(!is_lost_race(
            "fatal: 'origin' does not appear to be a git repository"
        ));
        assert!(!is_lost_race(
            "ssh: connect to host example.com port 22: Connection timed out"
        ));
        assert!(!is_lost_race(
            " ! [remote rejected] refs/notes/spelunk -> refs/notes/spelunk (pre-receive hook declined)"
        ));
        assert!(!is_lost_race(
            "fatal: could not read Username for 'https://github.com': terminal prompts disabled"
        ));
    }
}
