//! Integration test for worktree → main-index resolution over a REAL
//! `git worktree`.
//!
//! The `config.rs` unit fixtures build a `.git`-*file* by hand, on which
//! `gix::discover` fails — so they exercise only the manual fallback branch of
//! `resolve_main_worktree_root`. This test creates a genuine linked worktree via
//! `git worktree add`, so `gix::discover` succeeds and the primary gix branch is
//! covered end to end. It also guards the real-path shape gix returns (e.g. the
//! macOS `/var` → `/private/var` symlink), which string-only fixtures cannot.
//!
//! Requires `git` on PATH; fully hermetic: everything lives under a TempDir
//! that is removed on drop, taking the linked worktree with it (no orphans),
//! and every git spawn goes through `common::git_command`, which shadows the
//! developer's ambient global/system git config, so an ambient `core.hooksPath`
//! or `commit.gpgsign` can never reach these setup commands either.

mod common;

use spelunk_core::config::find_project_db;
use spelunk_core::utils::resolve_main_worktree_root;

fn git(cwd: &std::path::Path, args: &[&str]) -> std::process::Output {
    let out = common::git_command(cwd)
        .args(args)
        .output()
        .expect("git command");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Compare two paths after canonicalising both, so a symlinked temp prefix
/// (macOS `/var` vs `/private/var`) does not spuriously fail the assertion.
fn same_path(a: &std::path::Path, b: &std::path::Path) {
    let ca = std::fs::canonicalize(a).unwrap();
    let cb = std::fs::canonicalize(b).unwrap();
    assert_eq!(ca, cb, "{} != {}", a.display(), b.display());
}

#[test]
fn real_git_worktree_resolves_to_main_index() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let main_root = tmp.path().join("main");
    let wt_root = tmp.path().join("feat-branch");
    std::fs::create_dir_all(&main_root).unwrap();

    // Real main repo with one commit so a worktree can be added.
    git(&main_root, &["init", "-b", "main"]);
    git(&main_root, &["config", "user.email", "test@example.com"]);
    git(&main_root, &["config", "user.name", "Test"]);
    std::fs::write(main_root.join("README.md"), "test").unwrap();
    git(&main_root, &["add", "."]);
    git(
        &main_root,
        &[
            "commit",
            "--no-gpg-sign",
            "-m",
            "init",
            "--allow-empty-message",
        ],
    );

    // Add a REAL linked worktree; wt_root/.git becomes a gitdir file pointing at
    // <main>/.git/worktrees/feat-branch — the layout gix::discover resolves.
    git(
        &main_root,
        &["worktree", "add", wt_root.to_str().unwrap(), "-b", "feat"],
    );
    assert!(
        wt_root.join(".git").is_file(),
        "worktree .git must be a file"
    );
    assert!(
        !wt_root.join(".spelunk").exists(),
        "worktree must have no local .spelunk/"
    );

    // The shared index lives only in the main worktree.
    std::fs::create_dir_all(main_root.join(".spelunk")).unwrap();
    let index_db = main_root.join(".spelunk").join("index.db");
    std::fs::write(&index_db, b"").unwrap();

    // Primary gix branch: discovery from inside the linked worktree resolves to
    // the main worktree root, and a read from the worktree finds the main index.
    same_path(&resolve_main_worktree_root(&wt_root), &main_root);
    same_path(
        &find_project_db(&wt_root).expect("worktree resolves to main index"),
        &index_db,
    );

    // A subdirectory inside the worktree resolves the same way (gix walks up).
    let sub = wt_root.join("nested").join("dir");
    std::fs::create_dir_all(&sub).unwrap();
    same_path(&resolve_main_worktree_root(&sub), &main_root);
}

// Proves the "fully hermetic" claim above is real, not aspirational: even
// when the *ambient* environment (as set before this test binary starts,
// which `isolate_git_config`'s process-wide `Once` cannot retroactively
// undo for a call that already ran) points `GIT_CONFIG_GLOBAL` at a global
// config whose `core.hooksPath` hook always fails, `real_git_worktree_resolves_to_main_index`
// still passes. `isolate_git_config` runs inside the child and overwrites
// the inherited value before the child's first git spawn.
//
// This has to re-exec the test binary as a child process: `isolate_git_config`
// is a one-shot `Once` per process, so simulating a hostile *ambient* value
// from within an already-running test (which may run after some other test
// already initialised isolation) cannot exercise the pre-isolation state the
// way a fresh child process, given a hostile environment at start, can.
#[test]
fn real_git_worktree_resolves_to_main_index_survives_a_hostile_ambient_hooks_path() {
    let hostile = tempfile::TempDir::new().expect("tempdir");
    let hooks_dir = hostile.path().join("hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    for hook in ["pre-commit", "post-checkout", "post-commit"] {
        let path = hooks_dir.join(hook);
        std::fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let global_config = hostile.path().join("gitconfig");
    std::fs::write(
        &global_config,
        format!("[core]\n\thooksPath = {}\n", hooks_dir.display()),
    )
    .unwrap();

    let exe = std::env::current_exe().expect("current test binary");
    let status = std::process::Command::new(exe)
        .arg("real_git_worktree_resolves_to_main_index")
        .arg("--exact")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .status()
        .expect("run self as a child process");
    assert!(
        status.success(),
        "the child test failed under a hostile ambient GIT_CONFIG_GLOBAL: isolation did not shadow it"
    );
}

// Commits in a fresh repo with an explicit local `user.name`/`user.email`,
// then asserts the commit's author and committer came from that config, not
// from whatever `GIT_AUTHOR_*`/`GIT_COMMITTER_*` the process happened to
// inherit. Git resolves identity as env-vars-before-config, so a fabricated
// local `user.email` alone proves nothing about isolation; the strong
// assertion is that the *env* did not win.
//
// Runs standalone (covers the ordinary case: no ambient poisoning, still
// green) and is also re-exec'd by the driver test below under a poisoned
// ambient environment, where a real gap would make this specific assertion
// fail rather than some unrelated step.
#[test]
fn git_command_commit_identity_matches_local_config_not_ambient_env() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("f.txt"), "hi").unwrap();
    git(root, &["add", "."]);
    git(
        root,
        &[
            "commit",
            "--no-gpg-sign",
            "-m",
            "init",
            "--allow-empty-message",
        ],
    );

    let out = git(root, &["log", "-1", "--format=%an <%ae>%n%cn <%ce>"]);
    let identity = String::from_utf8_lossy(&out.stdout);
    let mut lines = identity.lines();
    let author = lines.next().unwrap_or_default();
    let committer = lines.next().unwrap_or_default();
    assert_eq!(
        author, "Test <test@example.com>",
        "commit author must come from the repo's own config, not an ambient GIT_AUTHOR_* override"
    );
    assert_eq!(
        committer, "Test <test@example.com>",
        "commit committer must come from the repo's own config, not an ambient GIT_COMMITTER_* override"
    );
}

// Proves the assertion above is not vacuous: re-execs the test binary as a
// fresh child process with `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL`/
// `GIT_COMMITTER_NAME`/`GIT_COMMITTER_EMAIL` poisoned in the *ambient*
// environment (the way a CI runner's bot identity or a developer's shell
// profile might export them), and confirms the commit identity assertion
// above still passes, i.e. `isolate_git_config` genuinely clears these
// before the child's first git spawn rather than merely redirecting config
// files. Needs a fresh process for the same reason as the hooks-path
// driver above: `isolate_git_config`'s `Once` cannot retroactively unpoison
// an environment it already observed.
#[test]
fn git_command_commit_identity_survives_a_hostile_ambient_author_committer_env() {
    let exe = std::env::current_exe().expect("current test binary");
    let status = std::process::Command::new(exe)
        .arg("git_command_commit_identity_matches_local_config_not_ambient_env")
        .arg("--exact")
        .env("GIT_AUTHOR_NAME", "Ambient Poison")
        .env("GIT_AUTHOR_EMAIL", "poison@evil.example")
        .env("GIT_COMMITTER_NAME", "Ambient Poison")
        .env("GIT_COMMITTER_EMAIL", "poison@evil.example")
        .status()
        .expect("run self as a child process");
    assert!(
        status.success(),
        "the child test failed under a hostile ambient GIT_AUTHOR_*/GIT_COMMITTER_* env: isolation did not shadow it"
    );
}
