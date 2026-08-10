//! ADR-069 D1/D3/D7: publishing `refs/notes/spelunk` on `git push`.
//!
//! Publishing is coupled to `git push` because that is the only moment that
//! reliably coincides with "this code is being shared". A note on a locally
//! unpushed commit reaches origin while its target object does not, and a fresh
//! clone then cannot resolve it, so the memory is orphaned.
//!
//! The flow lives in `spelunk plumbing publish-notes` (D7); the installed hook
//! is a shim that `exec`s it with the binary's absolute path embedded. These
//! tests drive it end to end through real `git push` invocations.
//!
//! Covered:
//! - the hook runs exactly once per push: the nested notes push must not
//!   re-enter it (a version without `--no-verify` recursed until the process
//!   table was exhausted, while every outer push still reported success).
//! - the three-case exit split: a publish failure exits 0 and the branch push
//!   lands; a removed binary exits non-zero and stops the push; a PATH without
//!   spelunk is irrelevant because the shim embeds an absolute path.
//! - two developers annotating the same commit converge, losing neither entry.
//! - the union keeps every record on its own parseable line: git's newline
//!   normalization is load-bearing here and is owned outside spelunk (D2).
//! - a lost race is retried and converges; a rejection is attempted exactly once.
//! - a fetch failure never destroys the notes already on the remote.
//! - the publish path takes the notes lock, so a concurrent writer cannot eat
//!   the merge (D6/D7).
//! - the merge strands no `NOTES_MERGE_WORKTREE`.
//! - repeated pushes are idempotent: no duplicates, no empty re-push.
//! - graceful skip with no local notes ref and when pushing by URL.
//! - the hook publishes to the remote being pushed to, not a hardcoded `origin`.
//! - a non-spelunk pre-push hook is never clobbered; a moved binary re-resolves.
//!
//! The ambient PATH deliberately does **not** carry the binary under test: the
//! shim embeds an absolute path, so every test here also proves no PATH lookup
//! is involved.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin_in;

use assert_cmd::Command;
use std::path::{Path, PathBuf};
use std::process::Output;
use tempfile::TempDir;

/// Absolute path of the `spelunk` binary under test.
fn spelunk_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_spelunk"))
}

// A `git` invocation in `dir` with an isolated identity and config.
//
// `git push` runs the pre-push hook, which execs a spelunk child. That child
// runs `Config::load`, which reads a config dir and resolves a secret store,
// defaulting to the OS keychain when `SPELUNK_SECRET_STORE` is unset. git is
// the only thing standing between a test and that child, so all of it has to be
// pinned here: pinning it on the spelunk commands a test runs directly leaves
// the hook's child ambient.
//
// `HOME` alone does not pin the config dir, because `spelunk_config_dir()`
// returns `SPELUNK_CONFIG_DIR` before it consults `dirs::home_dir()`. Anything
// this helper does not set is inherited from the test process, so a runner that
// exports `SPELUNK_CONFIG_DIR` (the documented way to isolate the suite from a
// developer's own config) silently wins over `HOME` and points the hook's child
// at a directory no test seeded. `SPELUNK_CONFIG_DIR` is therefore derived from
// `home` and set explicitly, the same way `spelunk_bin_in` does it for the
// direct-spawn path. That also makes the pin work on Windows, where
// `dirs::home_dir()` reads no environment variable at all.
fn git_cmd(home: &Path, dir: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(dir)
        .env("HOME", home)
        .env("SPELUNK_CONFIG_DIR", home.join(".config").join("spelunk"))
        .env("SPELUNK_SECRET_STORE", "file")
        .env_remove("XDG_CONFIG_HOME")
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

/// Run `git args` in `dir` with an explicit `PATH`, returning the `Output`
/// without asserting.
fn git_out_with_path(
    home: &Path,
    dir: &Path,
    path: impl AsRef<std::ffi::OsStr>,
    args: &[&str],
) -> Output {
    git_cmd(home, dir)
        .args(args)
        .env("PATH", path)
        .output()
        .expect("spawn git")
}

/// Run `git args` in `dir`, returning the `Output` without asserting.
fn git_out(home: &Path, dir: &Path, args: &[&str]) -> Output {
    git_cmd(home, dir).args(args).output().expect("spawn git")
}

/// Like [`git_out`] but asserts the command succeeded.
fn git(home: &Path, dir: &Path, args: &[&str]) -> Output {
    let out = git_out(home, dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// `stdout` of `git args`, trimmed, whatever the exit status. Used for notes
/// inspection where a missing ref is a legitimate empty result.
fn git_stdout(home: &Path, dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&git_out(home, dir, args).stdout)
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

// Like `bin`, but runs an arbitrary copy of the binary rather than the one
// cargo built. Used to control the path the shim embeds.
//
// This cannot go through `spelunk_bin_in`, which always resolves the
// cargo-built binary, so it repeats that helper's isolation by hand and has to
// keep `SPELUNK_CONFIG_DIR` among it: without the pin this spawn inherits the
// runner's ambient value, which wins over `HOME`.
fn bin_at(exe: &Path, home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(exe);
    cmd.current_dir(cwd)
        .env("SPELUNK_SECRET_STORE", "file")
        .env("HOME", home)
        .env("SPELUNK_CONFIG_DIR", home.join(".config").join("spelunk"))
        .env_remove("XDG_CONFIG_HOME")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL");
    cmd
}

/// The env var the nested notes push sets, mirrored from the command. A rename
/// there must fail here rather than silently stop guarding anything.
const NOTES_PUSH_SENTINEL: &str = "SPELUNK_NOTES_PUSH";

/// `plumbing publish-notes <remote>` in `repo`, parsed. The hook drops stdout,
/// so the reported outcome is only reachable by running the command directly.
fn publish_notes_json(home: &Path, repo: &Path, remote: &str) -> serde_json::Value {
    let out = bin(home, repo)
        .args(["plumbing", "publish-notes", remote])
        .output()
        .expect("run publish-notes");
    assert!(
        out.status.success(),
        "publish-notes failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .expect("publish-notes emits one JSON object")
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
    install_pre_push_from(&spelunk_exe(), home, repo)
}

/// Install the pre-push hook using the copy of spelunk at `exe`, so the shim
/// embeds `exe`'s path rather than the built binary's.
fn install_pre_push_from(exe: &Path, home: &Path, repo: &Path) -> PathBuf {
    bin_at(exe, home, repo)
        .args(["hooks", "install", "--pre-push"])
        .assert()
        .success();
    hook_path(repo)
}

fn hook_path(repo: &Path) -> PathBuf {
    repo.join(".git").join("hooks").join("pre-push")
}

/// A bare repo standing in for `origin`.
fn bare_origin(home: &Path, dir: &Path) {
    git(home, dir, &["init", "-q", "--bare", "-b", "main"]);
}

/// `path` for embedding in a shell script. Single quotes reach Git Bash with
/// backslashes intact, so a Windows path must arrive forward-slashed.
fn sh_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// Write `body` to `path` and make it executable. Parent dirs are created: a
/// bare repo has no `hooks/` until something needs one.
fn write_executable(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(path).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(path, p).unwrap();
    }
}

/// Reject every `refs/notes/*` update on `origin`, recording one line per
/// attempt in `counter`. Per-ref, so the branch push is untouched.
fn reject_notes_and_count(origin: &Path, counter: &Path) {
    write_executable(
        &origin.join("hooks").join("update"),
        &format!(
            "#!/bin/sh\ncase \"$1\" in refs/notes/*) echo try >> '{}' ; exit 1 ;; esac\nexit 0\n",
            sh_path(counter)
        ),
    );
}

/// Lines in `path`, or 0 when it was never written.
fn line_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

/// Publish `title` from a second clone straight onto `origin`'s notes ref,
/// standing in for a teammate who shared memory first. Returns the annotated
/// object, which is the commit both sides share.
fn teammate_publishes(home: &Path, origin: &Path, dir: &Path, title: &str) -> String {
    clone_dev(home, origin, dir);
    memory_add(home, dir, title);
    git(
        home,
        dir,
        &[
            "push",
            "-q",
            "origin",
            "refs/notes/spelunk:refs/notes/spelunk",
        ],
    );
    git_stdout(home, dir, &["rev-parse", "HEAD"])
}

/// Commit `<name>.txt` in `dir`.
fn commit(home: &Path, dir: &Path, name: &str) {
    std::fs::write(dir.join(format!("{name}.txt")), name).unwrap();
    git(home, dir, &["add", "."]);
    git(home, dir, &["commit", "-q", "-m", name]);
}

/// A dev clone of `origin` with an identity and one commit pushed to `main`.
fn seed_origin(home: &Path, origin: &Path, dir: &Path) {
    git(home, dir, &["init", "-q", "-b", "main"]);
    git(home, dir, &["config", "user.email", "t@example.com"]);
    git(home, dir, &["config", "user.name", "Test"]);
    git(
        home,
        dir,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    commit(home, dir, "seed");
    git(home, dir, &["push", "-q", "-u", "origin", "main"]);
}

/// Clone `origin` into `dir` with an identity, as a second developer.
fn clone_dev(home: &Path, origin: &Path, dir: &Path) {
    git(
        home,
        dir.parent().unwrap(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            dir.to_str().unwrap(),
        ],
    );
    git(home, dir, &["config", "user.email", "t2@example.com"]);
    git(home, dir, &["config", "user.name", "Test2"]);
}

/// The note blob on `object`'s `refs/notes/spelunk`, or empty when absent.
fn note_lines(home: &Path, dir: &Path, object: &str) -> Vec<String> {
    git_stdout(home, dir, &["notes", "--ref=spelunk", "show", object])
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Instrument the installed hook to append one line to `counter` per run.
///
/// Inserted straight after the shebang, ahead of the `exec`, so a re-entry is
/// counted even though the command's own sentinel would exit it early. The count
/// therefore measures git actually invoking the hook, which is exactly what
/// `--no-verify` must prevent; a sentinel-only guard would still show 2 here.
fn instrument_hook(hook_path: &Path, counter: &Path) {
    let body = std::fs::read_to_string(hook_path).unwrap();
    let (shebang, rest) = body.split_once('\n').expect("hook starts with a shebang");
    std::fs::write(
        hook_path,
        format!("{shebang}\necho fired >> '{}'\n{rest}", sh_path(counter)),
    )
    .unwrap();
}

/// How many times the instrumented hook ran.
fn fire_count(counter: &Path) -> usize {
    line_count(counter)
}

/// A bare `origin` plus a seeded dev clone, the shape nearly every test needs.
fn origin_and_dev(home: &Path, tmp: &Path) -> (PathBuf, PathBuf) {
    let origin = tmp.join("origin.git");
    let dev = tmp.join("dev");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&dev).unwrap();
    bare_origin(home, &origin);
    seed_origin(home, &origin, &dev);
    (origin, dev)
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
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    let hook = install_pre_push(home.path(), &dev);
    let counter = tmp.path().join("fires");
    instrument_hook(&hook, &counter);

    memory_add(home.path(), &dev, "recursion-guard-decision");
    let annotated = git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]);
    commit(home.path(), &dev, "second");
    git(home.path(), &dev, &["push", "-q", "origin", "main"]);

    assert_eq!(
        fire_count(&counter),
        1,
        "the hook must run exactly once per push; more means the notes push \
         re-entered it (the recursion the `--no-verify` guard prevents)"
    );

    // And it actually published: a guard that works by doing nothing is no good.
    assert!(
        !git_stdout(home.path(), &origin, &["rev-parse", "refs/notes/spelunk"]).is_empty(),
        "origin should carry refs/notes/spelunk after the push"
    );
    assert!(
        note_lines(home.path(), &origin, &annotated)
            .iter()
            .any(|l| l.contains("recursion-guard-decision")),
        "the pushed note should carry the recorded decision"
    );
}

// ── D3: the three-case exit split ─────────────────────────────────────────────

/// A notes push the remote rejects must not cost the user their branch push.
///
/// A hook exiting non-zero aborts the push outright and origin never receives
/// the commit, so `--best-effort` has to absorb every publish failure.
#[test]
fn failed_notes_push_does_not_block_the_branch_push() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    let attempts = tmp.path().join("attempts");
    reject_notes_and_count(&origin, &attempts);

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "rejected-notes-decision");
    commit(home.path(), &dev, "payload");

    let out = git_out(home.path(), &dev, &["push", "origin", "main"]);
    assert!(
        out.status.success(),
        "the branch push must survive a rejected notes push, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The commit really landed, rather than the push merely reporting success.
    assert_eq!(
        git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]),
        git_stdout(home.path(), &origin, &["rev-parse", "refs/heads/main"]),
        "origin must have received the branch commit"
    );

    // The notes push failed, and the user was told rather than left guessing.
    assert!(
        !git_out(home.path(), &origin, &["rev-parse", "refs/notes/spelunk"])
            .status
            .success(),
        "the rejected notes ref must not exist on origin"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("could not publish memory notes"),
        "the hook should warn on stderr, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A rejection is not a lost race: it fails identically every time, so the
    // command must give up after one attempt. Retrying every failure would park
    // a push behind three network timeouts when the remote is simply unreachable.
    assert_eq!(
        line_count(&attempts),
        1,
        "a rejected notes push must be attempted exactly once, never retried"
    );
}

// A config that will not load must not cost the user their branch push either.
//
// The config loads before the command dispatch, so a broken one aborted the
// push with a bare `?` before `--best-effort` was ever consulted. The failure
// mode reaches users through the keychain, the default store when
// `SPELUNK_SECRET_STORE` is unset, so no malformed file of their own is needed.
//
// The seeded config has to be the *ambient* one, which is the case the hook's
// child hits: it takes no `--config`. `git_cmd` pins `SPELUNK_CONFIG_DIR` at the
// seeded directory and `spelunk_config_dir()` returns that before it consults
// `dirs::home_dir()`, so the premise holds on every platform. That pin is what
// makes this reachable on Windows, where `dirs::home_dir()` calls
// `SHGetKnownFolderPath(FOLDERID_Profile)` and reads no environment variable, so
// a `HOME` redirect alone would leave the child on the real profile.
#[test]
fn an_unloadable_config_does_not_block_the_branch_push() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "broken-config-decision");
    let annotated = git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]);
    commit(home.path(), &dev, "payload");

    // Broken only now: `memory add` above needs a config that loads.
    let cfg_dir = home.path().join(".config").join("spelunk");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "not = valid toml [[[\n").unwrap();

    let out = git_out(home.path(), &dev, &["push", "origin", "main"]);
    assert!(
        out.status.success(),
        "the branch push must survive a config that will not load, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The commit really landed. Without this the assert above passes whenever
    // the hook is simply not reached, which is what the bug did to the push.
    assert_eq!(
        git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]),
        git_stdout(home.path(), &origin, &["rev-parse", "refs/heads/main"]),
        "origin must have received the branch commit"
    );

    // Tolerating the config must not degrade into skipping the publish: the
    // command does not read `cfg`, so it still has everything it needs.
    assert!(
        note_lines(home.path(), &origin, &annotated)
            .iter()
            .any(|l| l.contains("broken-config-decision")),
        "the note must still publish despite the unloadable config"
    );

    // And the user is told why, rather than it passing in silence.
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("config.toml"),
        "the hook should warn about the config on stderr, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// The `--best-effort` config tolerance reached through `--config` rather than an
// ambient config dir. The guard above drives the ambient path the hook's child
// actually takes; this pins the same arm to the explicit flag, so a regression in
// either route is caught on its own. The command ignores `cfg`, so the publish
// still runs and the exit stays 0.
#[test]
fn a_broken_config_is_tolerated_for_a_best_effort_publish() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (_origin, dev) = origin_and_dev(home.path(), tmp.path());

    let cfg = tmp.path().join("broken-config.toml");
    std::fs::write(&cfg, "not = valid toml [[[\n").unwrap();

    let out = bin(home.path(), &dev)
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "publish-notes", "--best-effort", "origin"])
        .output()
        .expect("run publish-notes");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a broken config must not fail a best-effort publish, got {:?}: {stderr}",
        out.status.code()
    );
    assert!(
        stderr.contains("config.toml"),
        "the config must be warned about rather than passing in silence, got: {stderr}"
    );

    // Without the flag the same config still fails loudly: the tolerance is
    // scoped to the hook's own invocation, not granted to every caller.
    let strict = bin(home.path(), &dev)
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "publish-notes", "origin"])
        .output()
        .expect("run publish-notes");
    assert!(
        !strict.status.success(),
        "a broken config must still fail a publish without --best-effort"
    );
}

/// A spelunk that is genuinely gone stops the push, loudly.
///
/// This is the one case allowed to fail: a user is better served by being told a
/// tool it expected is gone than by cruft sitting untidied forever. The embedded
/// path is what separates it from a GUI client's PATH simply lacking spelunk,
/// which a `command -v` guard could not distinguish.
///
/// The exact status is the shell's (126 on bash, 127 on dash); only non-zero is
/// load-bearing, because that is what makes git abort the push.
#[test]
fn a_removed_binary_stops_the_push() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    // Install from a copy, so the shim embeds a path we control and can remove.
    let copy = tmp
        .path()
        .join(format!("spelunk-copy{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(spelunk_exe(), &copy).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    install_pre_push_from(&copy, home.path(), &dev);
    memory_add(home.path(), &dev, "removed-binary-decision");

    std::fs::remove_file(&copy).unwrap();

    commit(home.path(), &dev, "payload");
    let out = git_out(home.path(), &dev, &["push", "origin", "main"]);
    assert!(
        !out.status.success(),
        "a removed spelunk must stop the push rather than fail silently"
    );
    assert_ne!(
        git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]),
        git_stdout(home.path(), &origin, &["rev-parse", "refs/heads/main"]),
        "the push must not have proceeded"
    );
}

/// spelunk missing from the pushing client's PATH must be a non-event.
///
/// `install.sh` falls back to `~/.local/bin` and tells the user to add it to
/// their **shell profile**; macOS GUI apps take their environment from launchd
/// instead, so Tower, GitHub Desktop and VS Code run hooks without it. A
/// `command -v spelunk` guard would silently publish nothing for those users,
/// and dropping the guard without embedding the path would break their push
/// outright. The shim does no PATH lookup, so this publishes normally.
#[cfg(unix)]
#[test]
fn publishes_with_spelunk_absent_from_path() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "no-path-decision");
    let annotated = git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]);
    commit(home.path(), &dev, "no-path");

    // A PATH holding nothing but git itself: spelunk is definitively not on it.
    let bin_dir = tmp.path().join("git-only-bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let git_path = String::from_utf8(
        std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .expect("locate git")
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    std::os::unix::fs::symlink(&git_path, bin_dir.join("git")).unwrap();

    let out = git_out_with_path(
        home.path(),
        &dev,
        bin_dir.display().to_string(),
        &["push", "origin", "main"],
    );
    assert!(
        out.status.success(),
        "the push must succeed with spelunk off PATH: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        note_lines(home.path(), &origin, &annotated)
            .iter()
            .any(|l| l.contains("no-path-decision")),
        "publishing must not depend on a PATH lookup: a GUI client's PATH has no \
         ~/.local/bin, and those users must still publish"
    );
}

// ── D3: retry only a lost race ────────────────────────────────────────────────

/// A teammate landing notes between our fetch and our push is retried, and the
/// retry converges without dropping either side.
///
/// The race is reproduced by serving a stale view (the world before the
/// teammate published) to the first fetch only, so the first push is genuinely
/// non-fast-forward while the second fetch sees the teammate's notes and merges
/// them. Deterministic: the served *view* changes, so nothing depends on when a
/// side effect lands.
#[cfg(unix)]
#[test]
fn a_lost_race_is_retried_and_converges_with_no_loss() {
    let home = TempDir::new().unwrap();
    let home2 = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());
    let stale = tmp.path().join("stale.git");
    let teammate = tmp.path().join("teammate");

    // Snapshot origin before any notes exist: this is the stale view.
    git(
        home.path(),
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            origin.to_str().unwrap(),
            stale.to_str().unwrap(),
        ],
    );

    let shared = teammate_publishes(home2.path(), &origin, &teammate, "teammate-raced-decision");
    assert_eq!(
        shared,
        git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]),
        "setup: both sides must annotate the same commit"
    );

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "dev-raced-decision");

    let stamp = tmp.path().join("served-stale");
    let calls = tmp.path().join("upload-pack-calls");
    let wrapper = tmp.path().join("uploadpack.sh");
    write_executable(
        &wrapper,
        &format!(
            "#!/bin/sh\n\
             echo call >> '{}'\n\
             if [ -f '{}' ]; then exec git upload-pack '{}'; fi\n\
             : > '{}'\n\
             exec git upload-pack '{}'\n",
            calls.display(),
            stamp.display(),
            origin.display(),
            stamp.display(),
            stale.display(),
        ),
    );
    git(
        home.path(),
        &dev,
        &[
            "config",
            "remote.origin.uploadpack",
            wrapper.to_str().unwrap(),
        ],
    );

    commit(home.path(), &dev, "raced");
    let out = git_out(home.path(), &dev, &["push", "origin", "main"]);
    assert!(
        out.status.success(),
        "the branch push must survive the race: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The retry happened: one fetch on the stale view, one on the real origin.
    assert_eq!(
        line_count(&calls),
        2,
        "the command should fetch twice: the first push loses the race, the retry wins"
    );

    // Both entries reached origin, so the lost race cost nobody their memory.
    let published = note_lines(home.path(), &origin, &shared);
    assert!(
        published
            .iter()
            .any(|l| l.contains("teammate-raced-decision")),
        "the teammate's entry must survive our retry: {published:?}"
    );
    assert!(
        published.iter().any(|l| l.contains("dev-raced-decision")),
        "our entry must land on the retry: {published:?}"
    );
}

// ── D3: never destroy what is already published ───────────────────────────────

/// A fetch that fails must never cost a teammate their published notes.
///
/// With the fetch broken there is no tracking ref, so nothing is merged and our
/// notes ref is missing the teammate's entry. Publishing it anyway (a forced
/// push) would replace their memory with ours; the correct outcome is a
/// rejected notes push, an unaffected branch push, and their entry intact.
///
/// A plain 2-dev divergence test cannot catch a force-push: the union merge runs
/// first and carries both sides, so forcing would look identical. Breaking the
/// fetch is what makes the local ref genuinely diverge.
#[cfg(unix)]
#[test]
fn a_fetch_failure_must_not_destroy_a_teammates_notes() {
    let home = TempDir::new().unwrap();
    let home2 = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());
    let teammate = tmp.path().join("teammate");

    let shared = teammate_publishes(
        home2.path(),
        &origin,
        &teammate,
        "teammate-published-decision",
    );

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "dev-unmergeable-decision");

    // Break only the fetch: push rides receive-pack and is unaffected.
    git(
        home.path(),
        &dev,
        &[
            "config",
            "remote.origin.uploadpack",
            "/nonexistent/upload-pack",
        ],
    );

    commit(home.path(), &dev, "payload");
    let out = git_out(home.path(), &dev, &["push", "origin", "main"]);
    assert!(
        out.status.success(),
        "the branch push must survive a broken fetch: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]),
        git_stdout(home.path(), &origin, &["rev-parse", "refs/heads/main"]),
        "origin must have received the branch commit"
    );

    // The whole point: their memory is still there.
    let published = note_lines(home.path(), &origin, &shared);
    assert!(
        published
            .iter()
            .any(|l| l.contains("teammate-published-decision")),
        "a fetch failure must never overwrite the teammate's published notes: {published:?}"
    );
}

// ── D6/D7: the publish path joins the lock protocol ───────────────────────────

/// The publish flow takes the same cross-process notes lock as every other
/// writer, so a concurrent `memory add` cannot silently eat the merged entries
/// with its read-modify-write.
///
/// This is the gap moving publish out of shell closes: `flock(1)` is util-linux,
/// absent from stock macOS and Git for Windows, so the shell hook merged
/// unlocked. Proven by contention: with the lock held elsewhere the command
/// blocks on the lock budget, where an unlocked merge would return immediately.
#[test]
fn the_publish_path_takes_the_notes_lock() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (_origin, dev) = origin_and_dev(home.path(), tmp.path());
    memory_add(home.path(), &dev, "locked-decision");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let held = rt
        .block_on(spelunk_core::storage::lock_notes(Some(&dev)))
        .expect("setup: the notes lock must be free to start with");

    let start = std::time::Instant::now();
    bin(home.path(), &dev)
        .args(["plumbing", "publish-notes", "origin"])
        .assert()
        .success();
    let contended = start.elapsed();
    drop(held);

    let start = std::time::Instant::now();
    bin(home.path(), &dev)
        .args(["plumbing", "publish-notes", "origin"])
        .assert()
        .success();
    let free = start.elapsed();

    assert!(
        contended >= std::time::Duration::from_secs(2),
        "publish must contend on the notes lock; it returned in {contended:?} with the \
         lock held, so its merge ran unlocked and a concurrent `memory add` could eat it \
         (uncontended run took {free:?})"
    );
}

/// A publish that could not take the lock skips, says so, and exits 0.
///
/// The merge is what carries the remote's side, so skipping it and pushing
/// anyway offers the remote a ref that is still diverged: the push is rejected
/// non-fast-forward and the user is handed a retry hint for a race that never
/// happened. Reporting `published: true` for it is the worse half, claiming
/// work that did not happen.
///
/// Diverged on purpose. Converged, the push succeeds and every wrong answer
/// still looks like success, which is the vacuity this test exists to avoid.
#[test]
fn a_publish_that_cannot_lock_skips_rather_than_misreporting_a_push() {
    let home1 = TempDir::new().unwrap();
    let home2 = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home1.path(), tmp.path());

    // A teammate's entry on origin that we have never fetched, so our notes ref
    // is genuinely non-fast-forward against it.
    let dev2 = tmp.path().join("dev2");
    let shared = teammate_publishes(home2.path(), &origin, &dev2, "teammate-decision");
    memory_add(home1.path(), &dev, "our-decision");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let held = rt
        .block_on(spelunk_core::storage::lock_notes(Some(&dev)))
        .expect("setup: the notes lock must be free to start with");

    let out = bin(home1.path(), &dev)
        .args(["plumbing", "publish-notes", "origin"])
        .output()
        .expect("run publish-notes");
    drop(held);

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    // D8: publish is idempotent, so contention skips. It never fails the caller,
    // with or without --best-effort.
    assert!(
        out.status.success(),
        "a contended publish must not fail the caller, got {:?}: {stderr}",
        out.status.code()
    );

    let json: serde_json::Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .expect("publish-notes emits one JSON object");
    assert_eq!(
        json["published"], false,
        "a skipped merge must not be reported as published: {json}"
    );
    assert_eq!(
        json["skipped"], "lock_unavailable",
        "the skip must name its reason: {json}"
    );

    // The hook drops stdout, so stderr is the only channel that reaches a user
    // pushing through it.
    assert!(
        stderr.contains("lock"),
        "the skip must reach the user on stderr, got: {stderr}"
    );
    assert!(
        !stderr.contains("non-fast-forward"),
        "a skipped merge must not push: the rejection and its retry hint describe \
         a race that never happened, got: {stderr}"
    );

    // Nothing of the teammate's was touched, and ours stayed local to publish
    // on the next push.
    let on_origin = note_lines(home2.path(), &origin, &shared);
    assert!(
        on_origin.iter().any(|l| l.contains("teammate-decision")),
        "a skipped publish must not disturb the teammate's entry: {on_origin:?}"
    );
    assert!(
        !on_origin.iter().any(|l| l.contains("our-decision")),
        "a skipped publish must not push: ours stays local until the next push, \
         got: {on_origin:?}"
    );
}

// ── D2: the merge leaves no wreckage ──────────────────────────────────────────

/// The notes merge must strand no `NOTES_MERGE_WORKTREE`.
///
/// `notes.mergeStrategy` defaults to `manual`, which on a genuine add/add
/// conflict exits 1 and leaves `.git/NOTES_MERGE_WORKTREE` behind for the user
/// to resolve by hand. A push hook cannot ask for that, so the strategy is
/// explicit: the union resolves the conflict and there is nothing to strand.
#[test]
fn the_notes_merge_strands_no_merge_worktree() {
    let home = TempDir::new().unwrap();
    let home2 = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());
    let teammate = tmp.path().join("teammate");

    let shared = teammate_publishes(home2.path(), &origin, &teammate, "their-decision");
    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "our-decision");

    commit(home.path(), &dev, "payload");
    git(home.path(), &dev, &["push", "-q", "origin", "main"]);

    assert!(
        !dev.join(".git").join("NOTES_MERGE_WORKTREE").exists(),
        "the merge must not strand a NOTES_MERGE_WORKTREE; the default `manual` \
         strategy does exactly that, which is why `-s cat_sort_uniq` is explicit"
    );
    assert!(
        !dev.join(".git").join("NOTES_MERGE_REF").exists(),
        "a stuck partial merge must not be left behind"
    );

    let merged = note_lines(home.path(), &dev, &shared);
    assert!(
        merged.iter().any(|l| l.contains("their-decision"))
            && merged.iter().any(|l| l.contains("our-decision")),
        "the union must resolve the conflict, keeping both sides: {merged:?}"
    );
}

// ── D2: divergence unions rather than clobbers ────────────────────────────────

/// Two developers annotating the same commit both survive: the flow fetches and
/// unions with `cat_sort_uniq` before pushing, so the second to push adds to the
/// first's entry rather than replacing it.
#[test]
fn two_dev_divergence_converges_with_no_loss() {
    let home1 = TempDir::new().unwrap();
    let home2 = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev1) = origin_and_dev(home1.path(), tmp.path());
    let dev2 = tmp.path().join("dev2");
    clone_dev(home2.path(), &origin, &dev2);

    // Both annotate the shared seed commit: the divergence is one object with
    // two different note blobs, which is the case `cat_sort_uniq` exists for.
    let shared = git_stdout(home1.path(), &dev1, &["rev-parse", "HEAD"]);
    assert_eq!(
        shared,
        git_stdout(home2.path(), &dev2, &["rev-parse", "HEAD"])
    );

    install_pre_push(home1.path(), &dev1);
    install_pre_push(home2.path(), &dev2);
    memory_add(home1.path(), &dev1, "dev1-only-decision");
    memory_add(home2.path(), &dev2, "dev2-only-decision");

    // Separate branches, so the branch pushes never conflict and the test is
    // about the notes ref alone.
    git(home1.path(), &dev1, &["checkout", "-q", "-b", "feature-1"]);
    commit(home1.path(), &dev1, "one");
    git(home1.path(), &dev1, &["push", "-q", "origin", "feature-1"]);

    git(home2.path(), &dev2, &["checkout", "-q", "-b", "feature-2"]);
    commit(home2.path(), &dev2, "two");
    git(home2.path(), &dev2, &["push", "-q", "origin", "feature-2"]);

    // dev2 pushed second, so its publish had to merge dev1's entry in first.
    let merged = note_lines(home2.path(), &dev2, &shared);
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
        home1.path(),
        &dev1,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/notes/spelunk:refs/notes/origin/spelunk",
        ],
    );
    git(
        home1.path(),
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
    let round_tripped = note_lines(home1.path(), &dev1, &shared);
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

// ── D2: the union welds no records together ───────────────────────────────────

/// Every record survives the union as its own whole line.
///
/// `append_to_git_notes` builds the body with no trailing newline; git adds one
/// when storing via `notes add -F`. That normalization is the only thing keeping
/// `cat_sort_uniq` from welding one side's last line onto the other's first and
/// corrupting both records. It is owned by git rather than by spelunk, so it is
/// pinned here instead of assumed. A substring assertion cannot stand in for
/// this: a welded line still contains both titles.
#[test]
fn the_union_welds_no_records_together() {
    let home = TempDir::new().unwrap();
    let home2 = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());
    let teammate = tmp.path().join("teammate");

    // Both sides annotate the same object, so the union has to concatenate two
    // blobs: the only case where a missing newline welds records.
    let shared = teammate_publishes(home2.path(), &origin, &teammate, "their-decision");
    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "our-decision");

    commit(home.path(), &dev, "payload");
    git(home.path(), &dev, &["push", "-q", "origin", "main"]);

    let merged = note_lines(home.path(), &dev, &shared);
    assert!(
        merged.len() >= 2,
        "the union must keep each record on its own line: {merged:?}"
    );
    for line in &merged {
        if let Err(e) = serde_json::from_str::<serde_json::Value>(line) {
            panic!(
                "every merged line must parse as one whole record, but {line:?} did not ({e}); \
                 git no longer newline-terminates note bodies, so the union welds records"
            );
        }
    }
}

/// Repeated pushes converge: the union is idempotent, so a second sync neither
/// duplicates entries nor fails.
#[test]
fn repeated_syncs_are_idempotent() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "idempotent-decision");
    let annotated = git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]);

    for i in 0..3 {
        commit(home.path(), &dev, &format!("push-{i}"));
        let out = git_out(home.path(), &dev, &["push", "origin", "main"]);
        assert!(
            out.status.success(),
            "push {i} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let lines = note_lines(home.path(), &dev, &annotated);
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
        note_lines(home.path(), &origin, &annotated),
        "local and origin notes must have converged"
    );
}

// ── D3: graceful skips ────────────────────────────────────────────────────────

/// Nothing recorded yet: there is nothing to publish and the flow must stay out
/// of the way. This is every user who installed the hook before their first
/// `memory add`.
#[test]
fn skips_gracefully_with_no_local_notes_ref() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    install_pre_push(home.path(), &dev);
    assert!(
        !git_out(
            home.path(),
            &dev,
            &["rev-parse", "--verify", "refs/notes/spelunk"]
        )
        .status
        .success(),
        "setup: no notes recorded yet"
    );

    commit(home.path(), &dev, "no-notes");
    let out = git_out(home.path(), &dev, &["push", "origin", "main"]);
    assert!(
        out.status.success(),
        "push must succeed with no notes to publish: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !git_out(home.path(), &origin, &["rev-parse", "refs/notes/spelunk"])
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
/// resolve, so the flow skips instead of guessing, and the push is unaffected.
///
/// A surviving push cannot stand in for the skip: a URL fetches and pushes just
/// as well as a remote name, so dropping the guard entirely also leaves the push
/// green while publishing onto `origin`'s tracking ref from a remote the user
/// never named. Both halves of the skip are asserted instead.
#[test]
fn skips_gracefully_when_pushing_without_a_named_remote() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "by-url-decision");
    commit(home.path(), &dev, "by-url");

    let out = git_out(
        home.path(),
        &dev,
        &["push", origin.to_str().unwrap(), "main"],
    );
    assert!(
        out.status.success(),
        "a push by URL must still succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Nothing was published: there was memory to publish and a reachable URL to
    // publish it to, so an unguarded flow would have landed it here.
    assert!(
        !git_out(home.path(), &origin, &["rev-parse", "refs/notes/spelunk"])
            .status
            .success(),
        "a push by URL must publish nothing: the flow skips rather than resolving \
         the URL itself"
    );

    // And it skipped for this reason, not incidentally.
    assert_eq!(
        publish_notes_json(home.path(), &dev, origin.to_str().unwrap())["skipped"],
        "no_such_remote",
        "a URL must skip as an unresolvable remote"
    );
}

/// The recursion sentinel stops a publish that re-entered itself.
///
/// `--no-verify` on the nested notes push is the guard that actually holds, and
/// `hook_publishes_notes_and_fires_exactly_once` pins it. This pins the backstop
/// beneath it, for a client that runs the hook regardless: the second publish
/// must not push. Driven at the command layer because, with `--no-verify`
/// working, nothing reaches this through a real `git push`.
#[test]
fn a_re_entered_publish_stops_at_the_sentinel() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (origin, dev) = origin_and_dev(home.path(), tmp.path());

    // Real memory and a reachable remote: without the sentinel this publishes.
    memory_add(home.path(), &dev, "sentinel-decision");

    let out = bin(home.path(), &dev)
        .args(["plumbing", "publish-notes", "origin"])
        .env(NOTES_PUSH_SENTINEL, "1")
        .output()
        .expect("run publish-notes");
    assert!(
        out.status.success(),
        "a re-entered publish must exit 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let reported: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
            .expect("publish-notes emits one JSON object");
    assert_eq!(
        reported["skipped"], "recursion",
        "a re-entered publish must report the recursion skip: {reported}"
    );
    assert!(
        !git_out(home.path(), &origin, &["rev-parse", "refs/notes/spelunk"])
            .status
            .success(),
        "a re-entered publish must push nothing"
    );
}

// ── D1: publish to the remote actually being pushed to ────────────────────────

/// Memory follows the push: a repo whose only remote is `upstream` publishes
/// there. The remote is whatever git handed the hook, never a hardcoded name.
#[test]
fn publishes_to_the_remote_being_pushed_to() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let upstream = tmp.path().join("upstream.git");
    let dev = tmp.path().join("dev");
    std::fs::create_dir_all(&upstream).unwrap();
    std::fs::create_dir_all(&dev).unwrap();
    bare_origin(home.path(), &upstream);

    git(home.path(), &dev, &["init", "-q", "-b", "main"]);
    git(
        home.path(),
        &dev,
        &["config", "user.email", "t@example.com"],
    );
    git(home.path(), &dev, &["config", "user.name", "Test"]);
    git(
        home.path(),
        &dev,
        &["remote", "add", "upstream", upstream.to_str().unwrap()],
    );
    commit(home.path(), &dev, "seed");
    git(home.path(), &dev, &["push", "-q", "-u", "upstream", "main"]);
    assert!(
        !git_out(home.path(), &dev, &["remote", "get-url", "origin"])
            .status
            .success(),
        "setup: this repo must have no origin remote"
    );

    install_pre_push(home.path(), &dev);
    memory_add(home.path(), &dev, "upstream-decision");
    let annotated = git_stdout(home.path(), &dev, &["rev-parse", "HEAD"]);
    commit(home.path(), &dev, "payload");
    let out = git_out(home.path(), &dev, &["push", "upstream", "main"]);
    assert!(
        out.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        note_lines(home.path(), &upstream, &annotated)
            .iter()
            .any(|l| l.contains("upstream-decision")),
        "the notes must reach the remote being pushed to, not a hardcoded 'origin'"
    );
}

// ── D3: install / uninstall ───────────────────────────────────────────────────

/// A pre-push hook spelunk did not write is left exactly as it was.
#[test]
fn install_bails_on_a_foreign_pre_push_hook() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (_origin, dev) = origin_and_dev(home.path(), tmp.path());

    let hook = hook_path(&dev);
    let foreign = "#!/bin/sh\necho someone else's hook\n";
    write_executable(&hook, foreign);

    bin(home.path(), &dev)
        .args(["hooks", "install", "--pre-push"])
        .assert()
        .failure();

    assert_eq!(
        std::fs::read_to_string(&hook).unwrap(),
        foreign,
        "a foreign hook must be left byte-for-byte alone"
    );
}

#[cfg(unix)]
#[test]
fn installed_hook_is_executable() {
    use std::os::unix::fs::PermissionsExt;
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (_origin, dev) = origin_and_dev(home.path(), tmp.path());

    let hook = install_pre_push(home.path(), &dev);
    let mode = std::fs::metadata(&hook).unwrap().permissions().mode();
    assert_eq!(
        mode & 0o111,
        0o111,
        "git will not run a non-executable hook"
    );
}

#[test]
fn install_is_idempotent() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (_origin, dev) = origin_and_dev(home.path(), tmp.path());

    let hook = install_pre_push(home.path(), &dev);
    let first = std::fs::read_to_string(&hook).unwrap();

    bin(home.path(), &dev)
        .args(["hooks", "install", "--pre-push"])
        .assert()
        .success()
        .stdout(predicates::str::contains("already installed"));

    assert_eq!(
        std::fs::read_to_string(&hook).unwrap(),
        first,
        "a re-install must not churn the hook"
    );
}

/// The embedded path goes stale when the binary moves, and re-installing is the
/// documented fix, so it has to actually re-resolve rather than report "already
/// installed" and leave the dead path in place.
#[test]
fn install_re_resolves_a_moved_binary() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (_origin, dev) = origin_and_dev(home.path(), tmp.path());

    let old = tmp
        .path()
        .join(format!("old-spelunk{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(spelunk_exe(), &old).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let hook = install_pre_push_from(&old, home.path(), &dev);
    assert!(
        std::fs::read_to_string(&hook)
            .unwrap()
            .contains(&sh_path(&old)),
        "setup: the shim must embed the path it was installed from"
    );

    // Re-install from the real binary: the marker still matches, so a
    // marker-only idempotence check would skip the rewrite and strand the path.
    install_pre_push(home.path(), &dev);
    let body = std::fs::read_to_string(&hook).unwrap();
    assert!(
        body.contains(&sh_path(&spelunk_exe())),
        "re-installing must re-resolve the binary path: {body}"
    );
    assert!(
        !body.contains(&sh_path(&old)),
        "the stale path must be gone: {body}"
    );
}

#[test]
fn uninstall_removes_the_pre_push_hook() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (_origin, dev) = origin_and_dev(home.path(), tmp.path());

    let hook = install_pre_push(home.path(), &dev);
    assert!(hook.exists());

    bin(home.path(), &dev)
        .args(["hooks", "uninstall"])
        .assert()
        .success();

    assert!(!hook.exists(), "uninstall must remove the pre-push hook");
}

/// `uninstall` removes spelunk's own hooks and leaves a foreign one alone,
/// rather than refusing to do anything because one file is not ours.
#[test]
fn uninstall_leaves_a_foreign_hook_alone() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (_origin, dev) = origin_and_dev(home.path(), tmp.path());

    let pre_push = install_pre_push(home.path(), &dev);
    let post_commit = dev.join(".git").join("hooks").join("post-commit");
    let foreign = "#!/bin/sh\necho someone else's hook\n";
    write_executable(&post_commit, foreign);

    bin(home.path(), &dev)
        .args(["hooks", "uninstall"])
        .assert()
        .success();

    assert!(!pre_push.exists(), "our hook must go");
    assert_eq!(
        std::fs::read_to_string(&post_commit).unwrap(),
        foreign,
        "a foreign hook must survive uninstall untouched"
    );
}
