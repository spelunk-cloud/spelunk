//! Hook installation must resolve the hooks directory the way git itself
//! does, honoring `core.hooksPath` (set by husky, lefthook, the pre-commit
//! framework, or a team's own shared-hooks convention) instead of assuming
//! the default `.git/hooks`.
//!
//! Before this, `spelunk hooks install` wrote to a hardcoded `$GIT_DIR/hooks`
//! and the `init` install-state detector read that same hardcoded path. On a
//! `core.hooksPath` machine the hook silently never ran while `init` kept
//! reporting it as installed, because installer and detector agreed with
//! each other while both disagreed with git.
//!
//! Covered:
//! - install lands at the git-resolved hooks dir when `core.hooksPath` points
//!   elsewhere, not at the default `.git/hooks`.
//! - a pre-push hook installed at a `core.hooksPath` location is actually
//!   invoked by a real `git push` (the functional proof, not just a file
//!   existence check).
//! - `init`'s summary agrees with where the hook actually landed.
//! - a `core.hooksPath` pointing inside the repo's tracked working tree (the
//!   husky/lefthook pattern) is refused rather than written to silently.
//! - a linked worktree resolves hooks to the main worktree's shared hooks dir.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin_in;

use assert_cmd::Command;
use std::path::{Path, PathBuf};
use std::process::Output;
use tempfile::TempDir;

/// A `git` invocation in `dir` with an isolated identity and config.
fn git_cmd(dir: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

fn git_out(dir: &Path, args: &[&str]) -> Output {
    git_cmd(dir).args(args).output().expect("spawn git")
}

fn git(dir: &Path, args: &[&str]) -> Output {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&git_out(dir, args).stdout)
        .trim()
        .to_string()
}

/// The hooks directory git itself resolves for `dir` (honors `core.hooksPath`
/// and worktrees). The independent reference the tests check installs
/// against, rather than re-deriving spelunk's own resolution logic.
fn git_hooks_dir(dir: &Path) -> PathBuf {
    let raw = git_stdout(dir, &["rev-parse", "--git-path", "hooks"]);
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        dir.join(path)
    }
}

/// A repo with a real identity and one commit.
fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("f.txt"), "x\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

/// A `spelunk` command with an isolated HOME and no server contact.
fn bin(home: &Path, cwd: &Path) -> Command {
    let mut cmd = spelunk_bin_in(home);
    cmd.current_dir(cwd)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL");
    cmd
}

/// Record one memory entry through the real `memory add`.
fn memory_add(home: &Path, repo: &Path, title: &str) {
    bin(home, repo)
        .args([
            "memory", "add", "--kind", "decision", "--title", title, "--body", "why",
        ])
        .assert()
        .success();
}

/// Write an empty spelunk config (`init` needs `--config` but no values here).
fn empty_config(dir: &Path) -> PathBuf {
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, "").unwrap();
    cfg
}

// ── install resolves through git, not a hardcoded .git/hooks ─────────────────

/// With `core.hooksPath` pointed at an untracked directory outside the repo,
/// the post-commit hook must land there, not at the default `.git/hooks`.
#[test]
fn install_post_commit_lands_at_the_core_hooks_path_location() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let custom_hooks = tmp.path().join("external-hooks");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&custom_hooks).unwrap();
    init_repo(&repo);
    git(
        &repo,
        &["config", "core.hooksPath", custom_hooks.to_str().unwrap()],
    );

    // Setup control: confirm git itself resolves hooks to the custom dir, so
    // the assertions below are about the real target, not a guess.
    assert_eq!(
        git_hooks_dir(&repo),
        custom_hooks,
        "setup: git must resolve hooks to the custom dir"
    );

    bin(home.path(), &repo)
        .args(["hooks", "install"])
        .assert()
        .success();

    assert!(
        custom_hooks.join("post-commit").exists(),
        "the hook must land where git will actually run it from"
    );
    assert!(
        !repo.join(".git").join("hooks").join("post-commit").exists(),
        "the hook must not be written to the default .git/hooks when \
         core.hooksPath points elsewhere"
    );
}

/// Same, for the pre-push hook, plus the functional proof: a real `git push`
/// must actually invoke the hook from the resolved location. A file existing
/// at the right path is necessary but not sufficient; git running it is the
/// whole point.
#[test]
fn pre_push_hook_installed_at_core_hooks_path_is_actually_invoked_by_git_push() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let origin = tmp.path().join("origin.git");
    let dev = tmp.path().join("dev");
    let custom_hooks = tmp.path().join("team-hooks");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&dev).unwrap();
    std::fs::create_dir_all(&custom_hooks).unwrap();

    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    init_repo(&dev);
    git(&dev, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&dev, &["push", "-q", "-u", "origin", "main"]);
    git(
        &dev,
        &["config", "core.hooksPath", custom_hooks.to_str().unwrap()],
    );

    bin(home.path(), &dev)
        .args(["hooks", "install", "--pre-push"])
        .assert()
        .success();

    assert!(
        custom_hooks.join("pre-push").exists(),
        "setup: the hook must have landed at the custom hooks dir"
    );
    assert!(
        !dev.join(".git").join("hooks").join("pre-push").exists(),
        "setup: nothing should be at the default .git/hooks location"
    );

    memory_add(home.path(), &dev, "custom-hooks-path-decision");
    std::fs::write(dev.join("f2.txt"), "y\n").unwrap();
    git(&dev, &["add", "."]);
    git(&dev, &["commit", "-q", "-m", "second"]);
    git(&dev, &["push", "-q", "origin", "main"]);

    // The proof that matters: git actually ran the hook from the custom
    // location, so the notes ref reached origin. A hook that only exists on
    // disk but is never invoked would leave this ref absent.
    assert!(
        git_out(&origin, &["rev-parse", "refs/notes/spelunk"])
            .status
            .success(),
        "the pre-push hook installed at the core.hooksPath location must \
         actually run on git push: origin should carry refs/notes/spelunk"
    );
}

// ── the install-state detector must agree with the installer ─────────────────

/// `init`'s summary must say the hook is installed only when it landed where
/// git will actually run it from. Before the fix, the installer and the
/// detector both read the same hardcoded `.git/hooks`, which agreed with each
/// other while disagreeing with git whenever `core.hooksPath` pointed
/// elsewhere.
#[test]
fn init_summary_agrees_with_pre_push_installer_when_core_hooks_path_is_set() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let custom_hooks = tmp.path().join("external-hooks");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&custom_hooks).unwrap();
    init_repo(&repo);
    git(
        &repo,
        &["config", "core.hooksPath", custom_hooks.to_str().unwrap()],
    );

    bin(home.path(), &repo)
        .args(["hooks", "install", "--pre-push"])
        .assert()
        .success();
    assert!(
        custom_hooks.join("pre-push").exists(),
        "setup: the hook must have landed at the custom hooks dir"
    );

    let cfg = empty_config(&repo);
    let out = bin(home.path(), &repo)
        .arg("--config")
        .arg(&cfg)
        .args(["init", "--no-index"])
        .output()
        .expect("spawn spelunk init");
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("pre-push hook installed: your memory publishes on `git push`"),
        "init must report the hook as installed since it landed where git \
         resolves hooks, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("your memory stays local"),
        "init must not claim the hook is missing when it is actually \
         installed at the resolved location, got:\n{stdout}"
    );
}

// ── a tracked, shared hooks dir is refused rather than written to ────────────

/// `core.hooksPath` pointing inside the repo's own tracked working tree (the
/// husky/lefthook pattern) is a different act from writing into local
/// `.git/`: it commits spelunk's hook to every clone. Install must refuse with
/// an explanation rather than write silently.
#[test]
fn install_refuses_when_core_hooks_path_is_inside_the_tracked_working_tree() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    git(&repo, &["config", "core.hooksPath", ".husky"]);

    let out = bin(home.path(), &repo)
        .args(["hooks", "install"])
        .output()
        .expect("run spelunk hooks install");

    assert!(
        !out.status.success(),
        "install must refuse a core.hooksPath inside the tracked working tree"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("tracked") && stderr.contains("shared"),
        "the refusal must explain that the directory is tracked and shared \
         with every clone, got: {stderr}"
    );
    assert!(
        !repo.join(".husky").join("post-commit").exists(),
        "nothing should have been written to the tracked hooks dir"
    );
}

/// The refusal is specific to a directory inside the tracked working tree: a
/// `core.hooksPath` outside the repo entirely (the common case) must still
/// install normally.
#[test]
fn install_still_succeeds_when_core_hooks_path_is_outside_the_repository() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let custom_hooks = tmp.path().join("outside-hooks");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&custom_hooks).unwrap();
    init_repo(&repo);
    git(
        &repo,
        &["config", "core.hooksPath", custom_hooks.to_str().unwrap()],
    );

    bin(home.path(), &repo)
        .args(["hooks", "install"])
        .assert()
        .success();

    assert!(custom_hooks.join("post-commit").exists());
}

// ── worktrees share the main repo's hooks dir ─────────────────────────────────

/// A linked worktree resolves hooks to the MAIN worktree's shared hooks dir,
/// not a per-worktree location: git itself runs hooks from there for every
/// worktree of a repo.
#[test]
fn hooks_resolve_to_the_shared_main_repo_hooks_dir_from_a_linked_worktree() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    // Canonicalized: git reports `--git-path`/`--show-toplevel` symlink-resolved
    // (macOS `$TMPDIR` is itself a symlink), and the real command always sees a
    // canonical cwd via `std::env::current_dir()`, so comparisons below must
    // use the same resolved form to compare like with like.
    //
    // `spelunk_core::utils::canonicalize` (backed by `dunce`) rather than
    // `Path::canonicalize`: on Windows CI runners the plain std canonicalize
    // returns a `\\?\`-prefixed verbatim path, and passing that straight to
    // `git worktree add <path>` as an argv string fails ("Invalid argument")
    // because this git-for-windows version does not accept the `\\?\` form
    // there. `dunce::canonicalize` still resolves symlinks (so the macOS
    // `$TMPDIR` case above is unaffected) but de-UNCs the Windows result back
    // to a plain `C:\…` path, which `git worktree add` accepts.
    let base = spelunk_core::utils::canonicalize(tmp.path());
    let main_root = base.join("main");
    std::fs::create_dir_all(&main_root).unwrap();
    init_repo(&main_root);

    let linked = base.join("linked");
    git(
        &main_root,
        &[
            "worktree",
            "add",
            "-q",
            linked.to_str().unwrap(),
            "-b",
            "feat",
        ],
    );

    // Setup control: confirm git itself shares the hooks dir across worktrees.
    assert_eq!(
        git_hooks_dir(&linked),
        main_root.join(".git").join("hooks"),
        "setup: git must resolve a linked worktree's hooks to the main repo's"
    );

    bin(home.path(), &linked)
        .args(["hooks", "install"])
        .assert()
        .success();

    assert!(
        main_root
            .join(".git")
            .join("hooks")
            .join("post-commit")
            .exists(),
        "installing from a linked worktree must land the hook in the main \
         repo's shared hooks dir"
    );
}
