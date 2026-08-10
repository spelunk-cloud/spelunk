//! In-process reads of spelunk's notes-ref OIDs — no git subprocess.
//!
//! ADR-077 D2 gates the read-path merge and import on whether the notes refs
//! moved since the last import, and that gate must be cheap: the steady-state
//! read has to spawn zero git subprocesses. ADR-069 D5 measured the same trade
//! for the merge — a `git rev-parse` guard costs ~8ms, an in-process read of
//! the ref file ~17µs — and chose the in-process read.
//!
//! So this resolves the git common dir by walking up for `.git` (a dir, or a
//! `gitdir:` pointer file for a linked worktree) and reads the ref straight
//! from the loose ref file or `packed-refs`. Notes refs live in the **common**
//! dir, shared across worktrees, which is where the merge and the fetch write
//! them.

use std::path::{Path, PathBuf};

use super::{SPELUNK_NOTES_REF, SPELUNK_TRACKING_REF};

/// The git ref store for a repo, resolved in-process for reading notes-ref OIDs.
pub struct NotesRefs {
    /// Directory holding `refs/` and `packed-refs`, shared across worktrees.
    common_dir: PathBuf,
    /// The worktree directory that contained `.git`, for callers that need a
    /// `git_root` to hand the subprocess helpers (merge / import). `None` when
    /// `start` sat directly on a bare/`.git` directory with no parent worktree.
    workdir: Option<PathBuf>,
}

impl NotesRefs {
    /// Discover the git ref store by walking up from `start` (or the process
    /// CWD when `None`) for a `.git` entry. Returns `None` when `start` is not
    /// inside a git repo. No subprocess: reads `.git`, an optional `commondir`
    /// pointer, and later the ref itself directly off disk.
    pub fn discover(start: Option<&Path>) -> Option<Self> {
        let start = match start {
            Some(p) => p.to_path_buf(),
            None => std::env::current_dir().ok()?,
        };

        let mut dir = start.as_path();
        let (dot_git, workdir) = loop {
            let candidate = dir.join(".git");
            if candidate.exists() {
                break (candidate, Some(dir.to_path_buf()));
            }
            dir = dir.parent()?;
        };

        // `.git` is normally a directory; a linked worktree makes it a file
        // holding `gitdir: <path>`.
        let git_dir = if dot_git.is_dir() {
            dot_git
        } else {
            let content = std::fs::read_to_string(&dot_git).ok()?;
            let rest = content
                .lines()
                .find_map(|l| l.strip_prefix("gitdir:"))?
                .trim();
            let p = Path::new(rest);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                dot_git.parent()?.join(p)
            }
        };

        // A linked worktree's git dir names the shared common dir in `commondir`;
        // the main worktree has none and is itself the common dir.
        let common_dir = read_commondir(&git_dir).unwrap_or(git_dir);

        Some(Self {
            common_dir,
            workdir,
        })
    }

    /// The worktree directory (a `git_root` for the subprocess helpers), if any.
    pub fn workdir(&self) -> Option<&Path> {
        self.workdir.as_deref()
    }

    /// OID of the working ref `refs/notes/spelunk`, or `None` when absent.
    pub fn working_oid(&self) -> Option<String> {
        self.read_ref(SPELUNK_NOTES_REF)
    }

    /// OID of the tracking ref `refs/notes/origin/spelunk` a fetch populates,
    /// or `None` when absent.
    pub fn tracking_oid(&self) -> Option<String> {
        self.read_ref(SPELUNK_TRACKING_REF)
    }

    /// Read one ref's OID: a loose ref file first, then `packed-refs`.
    fn read_ref(&self, refname: &str) -> Option<String> {
        // Loose ref: `<common>/refs/notes/spelunk`. Split on '/' and push each
        // component so the path is correct on Windows too.
        let mut loose = self.common_dir.clone();
        for comp in refname.split('/') {
            loose.push(comp);
        }
        if let Ok(body) = std::fs::read_to_string(&loose) {
            let trimmed = body.trim();
            // Notes refs are never symbolic, but skip a `ref:` line defensively
            // rather than mistake it for an OID.
            if !trimmed.is_empty() && !trimmed.starts_with("ref:") {
                return Some(trimmed.to_string());
            }
        }

        // Packed fallback: `<common>/packed-refs`, lines `<oid> <refname>`.
        let packed = self.common_dir.join("packed-refs");
        if let Ok(content) = std::fs::read_to_string(&packed) {
            for line in content.lines() {
                let line = line.trim();
                // `#` header, `^` peeled-tag continuation: neither is our line.
                if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                    continue;
                }
                let mut parts = line.split_whitespace();
                if let (Some(oid), Some(name)) = (parts.next(), parts.next())
                    && name == refname
                {
                    return Some(oid.to_string());
                }
            }
        }
        None
    }
}

/// Read a git dir's `commondir` pointer (present only in linked worktrees),
/// resolving a relative value against the git dir.
fn read_commondir(git_dir: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
    let rest = content.trim();
    if rest.is_empty() {
        return None;
    }
    let p = Path::new(rest);
    Some(if p.is_absolute() {
        p.to_path_buf()
    } else {
        git_dir.join(p)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real repo with one commit and a spelunk note, returned with its dir.
    fn repo_with_note() -> tempfile::TempDir {
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
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "T"]);
        std::fs::write(dir.path().join("README.md"), "x").expect("write");
        run(&["add", "."]);
        run(&["commit", "--no-gpg-sign", "-m", "first"]);
        dir
    }

    #[test]
    fn reads_a_loose_notes_ref_oid_in_process() {
        let repo = repo_with_note();
        std::process::Command::new("git")
            .args(["notes", "--ref=spelunk", "add", "-m", "hi", "HEAD"])
            .current_dir(repo.path())
            .output()
            .expect("git notes add");

        let refs = NotesRefs::discover(Some(repo.path())).expect("discover");
        let working = refs.working_oid().expect("working ref present");
        assert_eq!(working.len(), 40, "an OID is a 40-char sha1: {working}");
        assert!(
            refs.tracking_oid().is_none(),
            "no fetch happened, so the tracking ref must be absent"
        );
    }

    #[test]
    fn absent_ref_reads_as_none() {
        let repo = repo_with_note();
        let refs = NotesRefs::discover(Some(repo.path())).expect("discover");
        assert!(refs.working_oid().is_none());
        assert!(refs.tracking_oid().is_none());
    }

    #[test]
    fn reads_a_packed_notes_ref_oid() {
        let repo = repo_with_note();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .output()
                .expect("git");
        };
        run(&["notes", "--ref=spelunk", "add", "-m", "hi", "HEAD"]);
        // Force the loose ref into packed-refs, then confirm we still read it.
        run(&["pack-refs", "--all"]);

        let refs = NotesRefs::discover(Some(repo.path())).expect("discover");
        let working = refs.working_oid().expect("packed working ref present");
        assert_eq!(working.len(), 40, "packed OID is a 40-char sha1: {working}");
    }

    #[test]
    fn discover_outside_a_repo_is_none() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        assert!(
            NotesRefs::discover(Some(tmp.path())).is_none(),
            "a plain directory with no .git ancestor is not a repo"
        );
    }
}
