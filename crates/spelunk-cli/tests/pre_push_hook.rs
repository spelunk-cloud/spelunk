//! ADR-069 D1/D3: the opt-in pre-push hook that publishes `refs/notes/spelunk`.
//!
//! Publishing is coupled to `git push` because that is the only moment that
//! reliably coincides with "this code is being shared". A note on a locally
//! unpushed commit reaches origin while its target object does not, and a fresh
//! clone then cannot resolve it, so the memory is orphaned.
//!
//! Covered:
//! - the hook runs exactly once per push: the nested notes push must not
//!   re-enter it (a version without `--no-verify` recursed until the process
//!   table was exhausted, while every outer push still reported success).
//! - a failing notes push never blocks the branch push: a hook exiting non-zero
//!   aborts the push outright, so origin never receives the commit.
//! - two developers annotating the same commit converge, losing neither entry.
//! - repeated pushes are idempotent: no duplicates, no empty re-push.
//! - graceful skip with no local notes ref, and when pushing by URL.
//! - a non-spelunk pre-push hook is never clobbered.
//! - uninstall removes the hook.
//!
//! Every spawned `git` gets the built `spelunk` on PATH: the hook opens with a
//! `command -v spelunk` guard, so without it every test would pass vacuously.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin_in;

use assert_cmd::Command;
use std::path::{Path, PathBuf};
use std::process::Output;
use tempfile::TempDir;

/// Directory holding the `spelunk` binary under test, for the hook's
/// `command -v spelunk` guard.
fn spelunk_bin_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_spelunk"))
        .parent()
        .expect("binary has a parent dir")
        .to_path_buf()
}

/// Run `git args` in `dir`, returning the `Output` without asserting.
///
/// Isolated identity/config so it is hermetic, and `spelunk` prepended to PATH
/// so the installed hook clears its own `command -v spelunk` guard.
fn git_out(dir: &Path, args: &[&str]) -> Output {
    let path = format!(
        "{}:{}",
        spelunk_bin_dir().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("PATH", path)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git")
}

/// Like [`git_out`] but asserts the command succeeded.
fn git(dir: &Path, args: &[&str]) -> Output {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// `stdout` of `git args`, trimmed, whatever the exit status. Used for
/// notes inspection where a missing ref is a legitimate empty result.
fn git_stdout(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&git_out(dir, args).stdout)
        .trim()
        .to_string()
}

/// A `spelunk` command with an isolated HOME and no server contact.
fn bin(home: &Path, cwd: &Path) -> Command {
    let mut cmd = spelunk_bin_in(home);
    cmd.current_dir(cwd)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL");
    cmd
}

/// Record one entry through the real `memory add`, so the notes carry the real
/// record shape rather than a hand-rolled blob.
fn memory_add(home: &Path, repo: &Path, title: &str) {
    bin(home, repo)
        .args([
            "memory", "add", "--kind", "decision", "--title", title, "--body", "why",
        ])
        .assert()
        .success();
}

/// `spelunk hooks install --pre-push` in `repo`; returns the hook path.
fn install_pre_push(home: &Path, repo: &Path) -> PathBuf {
    bin(home, repo)
        .args(["hooks", "install", "--pre-push"])
        .assert()
        .success();
    repo.join(".git").join("hooks").join("pre-push")
}

/// A bare repo standing in for `origin`.
fn bare_origin(dir: &Path) {
    git(dir, &["init", "-q", "--bare", "-b", "main"]);
}

/// Commit `<name>.txt` in `dir`.
fn commit(dir: &Path, name: &str) {
    std::fs::write(dir.join(format!("{name}.txt")), name).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", name]);
}

/// A dev clone of `origin` with an identity and one commit pushed to `main`.
fn seed_origin(origin: &Path, dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["remote", "add", "origin", origin.to_str().unwrap()]);
    commit(dir, "seed");
    git(dir, &["push", "-q", "-u", "origin", "main"]);
}

/// Clone `origin` into `dir` with an identity, as a second developer.
fn clone_dev(origin: &Path, dir: &Path) {
    git(
        dir.parent().unwrap(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            dir.to_str().unwrap(),
        ],
    );
    git(dir, &["config", "user.email", "t2@example.com"]);
    git(dir, &["config", "user.name", "Test2"]);
}

/// The note blob on `object`'s `refs/notes/spelunk`, or empty when absent.
fn note_lines(dir: &Path, object: &str) -> Vec<String> {
    git_stdout(dir, &["notes", "--ref=spelunk", "show", object])
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Instrument the installed hook to append one line to `counter` per run.
///
/// Inserted straight after the shebang, ahead of every guard, so a re-entry is
/// counted even though a guard would exit it early. The count therefore measures
/// git actually invoking the hook, which is exactly what `--no-verify` must
/// prevent; a sentinel-only guard would still show 2 here.
fn instrument_hook(hook_path: &Path, counter: &Path) {
    let body = std::fs::read_to_string(hook_path).unwrap();
    let (shebang, rest) = body.split_once('\n').expect("hook starts with a shebang");
    std::fs::write(
        hook_path,
        format!("{shebang}\necho fired >> '{}'\n{rest}", counter.display()),
    )
    .unwrap();
}

/// How many times the instrumented hook ran.
fn fire_count(counter: &Path) -> usize {
    std::fs::read_to_string(counter)
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

// ── D3: the recursion guard ───────────────────────────────────────────────────

/// The hook publishes notes, and runs exactly once doing it.
///
/// Without `--no-verify` the nested notes push re-enters this same hook, which
/// pushes again, which re-enters again: the observed failure recursed 740 levels
/// and stopped only by exhausting the process table, with every outer push
/// failing while the branch push still reported success. One fire is the proof.
#[test]
fn hook_publishes_notes_and_fires_exactly_once() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let origin = tmp.path().join("origin.git");
    let dev = tmp.path().join("dev");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&dev).unwrap();
    bare_origin(&origin);
    seed_origin(&origin, &dev);

    let hook = install_pre_push(home.path(), &dev);
    let counter = tmp.path().join("fires");
    instrument_hook(&hook, &counter);

    memory_add(home.path(), &dev, "recursion-guard-decision");
    let annotated = git_stdout(&dev, &["rev-parse", "HEAD"]);
    commit(&dev, "second");
    git(&dev, &["push", "-q", "origin", "main"]);

    assert_eq!(
        fire_count(&counter),
        1,
        "the hook must run exactly once per push; more means the notes push \
         re-entered it (the recursion the `--no-verify` guard prevents)"
    );

    // And it actually published: a guard that works by doing nothing is no good.
    let on_origin = git_stdout(&origin, &["rev-parse", "refs/notes/spelunk"]);
    assert!(
        !on_origin.is_empty(),
        "origin should carry refs/notes/spelunk after the push"
    );
    assert!(
        note_lines(&origin, &annotated)
            .iter()
            .any(|l| l.contains("recursion-guard-decision")),
        "the pushed note should carry the recorded decision"
    );
}

// ── D3: never block the user's push ───────────────────────────────────────────

/// A notes push the remote rejects must not cost the user their branch push.
///
/// A hook exiting 1 aborts the push outright and origin never receives the
/// commit, so every failure path in the hook has to fall through to `exit 0`.
#[test]
fn failed_notes_push_does_not_block_the_branch_push() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let origin = tmp.path().join("origin.git");
    let dev = tmp.path().join("dev");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&dev).unwrap();
    bare_origin(&origin);
    seed_origin(&origin, &dev);

    // Reject only the notes ref, per-ref, so the branch push is untouched.
    let update_hook = origin.join("hooks").join("update");
    std::fs::create_dir_all(update_hook.parent().unwrap()).unwrap();
    std::fs::write(
        &update_hook,
        "#!/bin/sh\ncase \"$1\" in refs/notes/*) exit 1 ;; esac\nexit 0\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&update_hook).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&update_hook, p).unwrap();
    }

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "rejected-notes-decision");
    commit(&dev, "payload");

    let out = git_out(&dev, &["push", "origin", "main"]);
    assert!(
        out.status.success(),
        "the branch push must survive a rejected notes push, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The commit really landed, rather than the push merely reporting success.
    let local = git_stdout(&dev, &["rev-parse", "HEAD"]);
    let remote = git_stdout(&origin, &["rev-parse", "refs/heads/main"]);
    assert_eq!(local, remote, "origin must have received the branch commit");

    // The notes push failed, and the user was told rather than left guessing.
    assert!(
        git_out(&origin, &["rev-parse", "refs/notes/spelunk"])
            .status
            .success()
            .eq(&false),
        "the rejected notes ref must not exist on origin"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("could not publish memory notes"),
        "the hook should warn on stderr, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── D2/D3: two developers converge ────────────────────────────────────────────

/// Two developers annotating the same commit both survive: the hook fetches and
/// unions with `cat_sort_uniq` before pushing, so the second to push adds to the
/// first's entry rather than replacing it. Never force-push: that would drop it.
#[test]
fn two_dev_divergence_converges_with_no_loss() {
    let home1 = TempDir::new().unwrap();
    let home2 = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let origin = tmp.path().join("origin.git");
    let dev1 = tmp.path().join("dev1");
    let dev2 = tmp.path().join("dev2");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&dev1).unwrap();
    bare_origin(&origin);
    seed_origin(&origin, &dev1);
    clone_dev(&origin, &dev2);

    // Both annotate the shared seed commit: the divergence is one object with
    // two different note blobs, which is the case `cat_sort_uniq` exists for.
    let shared = git_stdout(&dev1, &["rev-parse", "HEAD"]);
    assert_eq!(shared, git_stdout(&dev2, &["rev-parse", "HEAD"]));

    install_pre_push(home1.path(), &dev1);
    install_pre_push(home2.path(), &dev2);
    memory_add(home1.path(), &dev1, "dev1-only-decision");
    memory_add(home2.path(), &dev2, "dev2-only-decision");

    // Separate branches, so the branch pushes never conflict and the test is
    // about the notes ref alone.
    git(&dev1, &["checkout", "-q", "-b", "feature-1"]);
    commit(&dev1, "one");
    git(&dev1, &["push", "-q", "origin", "feature-1"]);

    git(&dev2, &["checkout", "-q", "-b", "feature-2"]);
    commit(&dev2, "two");
    git(&dev2, &["push", "-q", "origin", "feature-2"]);

    // dev2 pushed second, so its hook had to merge dev1's entry in first.
    let merged = note_lines(&dev2, &shared);
    assert!(
        merged.iter().any(|l| l.contains("dev1-only-decision")),
        "dev2 must have merged dev1's entry rather than replacing it: {merged:?}"
    );
    assert!(
        merged.iter().any(|l| l.contains("dev2-only-decision")),
        "dev2 must still have its own entry: {merged:?}"
    );

    // And the union reached origin, so dev1 gets it back on fetch + merge.
    git(
        &dev1,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/notes/spelunk:refs/notes/origin/spelunk",
        ],
    );
    git(
        &dev1,
        &[
            "notes",
            "--ref=spelunk",
            "merge",
            "-s",
            "cat_sort_uniq",
            "refs/notes/origin/spelunk",
        ],
    );
    let round_tripped = note_lines(&dev1, &shared);
    assert!(
        round_tripped
            .iter()
            .any(|l| l.contains("dev2-only-decision")),
        "dev1 must receive dev2's entry: {round_tripped:?}"
    );
    assert!(
        round_tripped
            .iter()
            .any(|l| l.contains("dev1-only-decision")),
        "dev1 must keep its own entry: {round_tripped:?}"
    );
}

/// Repeated pushes converge: the union is idempotent, so a second sync neither
/// duplicates entries nor fails.
#[test]
fn repeated_syncs_are_idempotent() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let origin = tmp.path().join("origin.git");
    let dev = tmp.path().join("dev");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&dev).unwrap();
    bare_origin(&origin);
    seed_origin(&origin, &dev);

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "idempotent-decision");
    let annotated = git_stdout(&dev, &["rev-parse", "HEAD"]);

    for i in 0..3 {
        commit(&dev, &format!("push-{i}"));
        let out = git_out(&dev, &["push", "origin", "main"]);
        assert!(
            out.status.success(),
            "push {i} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let lines = note_lines(&dev, &annotated);
    let hits = lines
        .iter()
        .filter(|l| l.contains("idempotent-decision"))
        .count();
    assert_eq!(
        hits, 1,
        "repeated syncs must not duplicate an entry: {lines:?}"
    );
    assert_eq!(
        lines,
        note_lines(&origin, &annotated),
        "local and origin notes must have converged"
    );
}

// ── D3: graceful skips ────────────────────────────────────────────────────────

/// Nothing recorded yet: the hook has nothing to publish and must stay out of
/// the way. This is every user who installed the hook before their first
/// `memory add`.
#[test]
fn skips_gracefully_with_no_local_notes_ref() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let origin = tmp.path().join("origin.git");
    let dev = tmp.path().join("dev");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&dev).unwrap();
    bare_origin(&origin);
    seed_origin(&origin, &dev);

    install_pre_push(home.path(), &dev);
    assert!(
        !git_out(&dev, &["rev-parse", "--verify", "refs/notes/spelunk"])
            .status
            .success(),
        "setup: no notes recorded yet"
    );

    commit(&dev, "no-notes");
    let out = git_out(&dev, &["push", "origin", "main"]);
    assert!(
        out.status.success(),
        "push must succeed with no notes to publish: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !git_out(&origin, &["rev-parse", "refs/notes/spelunk"])
            .status
            .success(),
        "an empty notes ref must not be invented on origin"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("spelunk:"),
        "a no-op must be silent, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Pushing by URL rather than by remote name: there is no named remote to
/// resolve, so the hook skips instead of guessing, and the push is unaffected.
#[test]
fn skips_gracefully_when_pushing_without_a_named_remote() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let origin = tmp.path().join("origin.git");
    let dev = tmp.path().join("dev");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&dev).unwrap();
    bare_origin(&origin);
    seed_origin(&origin, &dev);

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "by-url-decision");
    commit(&dev, "by-url");

    let out = git_out(&dev, &["push", origin.to_str().unwrap(), "main"]);
    assert!(
        out.status.success(),
        "a push by URL must still succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── D3: never clobber someone else's hook ─────────────────────────────────────

/// A pre-push hook spelunk did not write is left exactly as it was.
#[test]
fn install_bails_on_a_foreign_pre_push_hook() {
    let home = TempDir::new().unwrap();
    let dev = TempDir::new().unwrap();
    git(dev.path(), &["init", "-q", "-b", "main"]);

    let hooks_dir = dev.path().join(".git").join("hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    let hook = hooks_dir.join("pre-push");
    let foreign = "#!/bin/sh\n# someone else's hook\nexit 0\n";
    std::fs::write(&hook, foreign).unwrap();

    bin(home.path(), dev.path())
        .args(["hooks", "install", "--pre-push"])
        .assert()
        .failure();

    assert_eq!(
        std::fs::read_to_string(&hook).unwrap(),
        foreign,
        "a foreign pre-push hook must survive byte-for-byte"
    );
}

/// Re-installing over spelunk's own hook is a no-op, not a failure.
#[test]
fn install_is_idempotent() {
    let home = TempDir::new().unwrap();
    let dev = TempDir::new().unwrap();
    git(dev.path(), &["init", "-q", "-b", "main"]);

    let hook = install_pre_push(home.path(), dev.path());
    let first = std::fs::read_to_string(&hook).unwrap();
    bin(home.path(), dev.path())
        .args(["hooks", "install", "--pre-push"])
        .assert()
        .success();
    assert_eq!(std::fs::read_to_string(&hook).unwrap(), first);
}

/// Uninstall removes spelunk's pre-push hook and leaves a foreign post-commit
/// hook alone.
#[test]
fn uninstall_removes_the_pre_push_hook() {
    let home = TempDir::new().unwrap();
    let dev = TempDir::new().unwrap();
    git(dev.path(), &["init", "-q", "-b", "main"]);

    let hook = install_pre_push(home.path(), dev.path());
    assert!(hook.exists());

    let foreign = dev.path().join(".git").join("hooks").join("post-commit");
    std::fs::write(&foreign, "#!/bin/sh\n# not ours\n").unwrap();

    bin(home.path(), dev.path())
        .args(["hooks", "uninstall"])
        .assert()
        .success();

    assert!(!hook.exists(), "the spelunk pre-push hook must be removed");
    assert!(foreign.exists(), "a foreign hook must be left alone");
}
