use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use std::path::Path;

#[derive(Args, Debug)]
pub struct HooksArgs {
    #[command(subcommand)]
    pub command: HooksCommand,
}

#[derive(Subcommand, Debug)]
pub enum HooksCommand {
    /// Install a post-commit hook that auto-indexes and harvests memory, or a
    /// pre-push hook that publishes memory to the remote (`--pre-push`)
    Install(HooksInstallArgs),
    /// Remove every git hook spelunk installed
    Uninstall,
}

#[derive(Args, Debug)]
pub struct HooksInstallArgs {
    /// Install the pre-push hook that publishes memory notes on `git push`
    #[arg(long, conflicts_with = "ci")]
    pub pre_push: bool,

    /// Print a GitHub Actions workflow step instead of writing a git hook
    #[arg(long)]
    pub ci: bool,
}

pub fn hooks(args: HooksArgs) -> Result<()> {
    match args.command {
        HooksCommand::Install(a) => hooks_install(a),
        HooksCommand::Uninstall => hooks_uninstall(),
    }
}

const POST_COMMIT_HOOK: &str = r#"#!/bin/sh
# spelunk post-commit hook — installed by `spelunk hooks install`
# Keeps the spelunk index in sync and harvests memory from new commits.
# Silently skips if `spelunk` is not in PATH, so teammates without spelunk are unaffected.

if ! command -v spelunk >/dev/null 2>&1; then
  exit 0
fi

PROJECT_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0

spelunk index "$PROJECT_ROOT" --detach
spelunk memory harvest --git-range HEAD~1..HEAD --detach
"#;

/// The pre-push shim. `{spelunk}` is substituted with the shell-quoted absolute
/// path of this binary by [`pre_push_hook_body`].
///
/// Every decision lives in the command, not here: a hook body is a string a user
/// already has on disk, so anything encoded in it cannot be changed by a release.
const PRE_PUSH_HOOK_TEMPLATE: &str = r#"#!/bin/sh
# spelunk pre-push hook (installed by `spelunk hooks install --pre-push`)
# Publishes spelunk memory (refs/notes/spelunk) to the remote you are pushing to,
# so decisions travel with the code they describe.
#
# The path below is absolute rather than a PATH lookup: GUI git clients on macOS
# inherit their environment from launchd, not from your shell profile. If spelunk
# is no longer there this exits 127 and stops the push, which is the intended
# loud failure; re-run `spelunk hooks install --pre-push` to re-resolve it.
#
# `exec` makes this hook's status the command's, and --best-effort makes a failed
# publish exit 0, so publishing can never cost you your push.
# stdout is dropped: the command emits JSONL, which a `git push` should not print.

exec {spelunk} plumbing publish-notes --best-effort "$@" >/dev/null
"#;

const CI_STEP: &str = r#"# Add to your .github/workflows/ file:
- name: Update spelunk index
  run: |
    if command -v spelunk >/dev/null 2>&1; then
      spelunk index . --detach
      spelunk memory harvest --git-range HEAD~1..HEAD --detach
    fi
"#;

/// An installable hook: git's filename for it, and the marker line identifying a
/// spelunk-written copy.
struct HookSpec {
    name: &'static str,
    marker: &'static str,
}

const POST_COMMIT: HookSpec = HookSpec {
    name: "post-commit",
    marker: "spelunk post-commit hook",
};

const PRE_PUSH: HookSpec = HookSpec {
    name: "pre-push",
    marker: "spelunk pre-push hook",
};

/// Every hook `uninstall` considers.
const ALL_HOOKS: [&HookSpec; 2] = [&POST_COMMIT, &PRE_PUSH];

/// The command that installs the pre-push hook. `init` names it when it tells
/// the user their memory stays local until they opt in (ADR-069 D3).
pub const PRE_PUSH_INSTALL_CMD: &str = "spelunk hooks install --pre-push";

/// Quote `path` for a POSIX shell. The shim runs under Git for Windows' `sh`,
/// where single quotes keep backslashes intact, so a Windows path has to arrive
/// forward-slashed.
fn sh_quoted(path: &Path) -> String {
    let forward = path.display().to_string().replace('\\', "/");
    format!("'{}'", forward.replace('\'', r"'\''"))
}

/// The pre-push shim, with this binary's resolved absolute path embedded.
fn pre_push_hook_body() -> Result<String> {
    let exe = std::env::current_exe().context("resolving the path of the spelunk binary")?;
    Ok(PRE_PUSH_HOOK_TEMPLATE.replace("{spelunk}", &sh_quoted(&exe)))
}

/// Whether spelunk's own pre-push hook is installed in the repo holding `dir`.
/// False for a foreign pre-push hook: that one publishes nothing.
pub fn pre_push_installed(dir: &Path) -> bool {
    let Ok(hooks_dir) = resolve_hooks_dir(dir) else {
        return false;
    };
    std::fs::read_to_string(hooks_dir.join(PRE_PUSH.name))
        .is_ok_and(|body| body.contains(PRE_PUSH.marker))
}

/// Run `git <args>` in `dir` and return trimmed stdout, erroring on a non-zero
/// exit.
fn git_output(dir: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !output.status.success() {
        anyhow::bail!("Not inside a git repository.");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Resolve the hooks directory the way git itself would run hooks from:
/// `git rev-parse --git-path hooks`, run from `dir`. This is the only correct
/// resolution because it honors `core.hooksPath` (set by husky, lefthook, and
/// the pre-commit framework) and follows a linked worktree back to its shared
/// hooks directory. Reading `$GIT_DIR/hooks` directly, as this used to, agrees
/// with git only when `core.hooksPath` is unset.
///
/// The result is canonicalized (via [`spelunk_core::utils::canonicalize`], so
/// symlinks are resolved and, on Windows, the `\\?\` prefix is stripped)
/// before it is returned. `hooks_dir_is_tracked` compares this value against
/// git's own `--show-toplevel` / `--git-common-dir` output with a plain
/// `PathBuf::starts_with`, which is a component-wise comparison with no
/// tolerance for two paths naming the same directory in different forms -
/// resolved vs. un-resolved symlinks, a differently-cased Windows drive
/// letter, or a `\\?\`-prefixed path next to a plain one. Canonicalizing both
/// sides is what makes that comparison meaningful.
fn resolve_hooks_dir(dir: &Path) -> Result<std::path::PathBuf> {
    let raw = git_output(dir, &["rev-parse", "--git-path", "hooks"])?;
    let path = std::path::PathBuf::from(raw);
    // A relative result is relative to `dir`, the cwd git was invoked from
    // (matches how git itself resolves a relative core.hooksPath). The
    // target itself (e.g. a `core.hooksPath` that has never been created)
    // may not exist yet, so canonicalize the base - which always exists -
    // before joining, rather than the full result.
    Ok(if path.is_absolute() {
        spelunk_core::utils::canonicalize(&path)
    } else {
        spelunk_core::utils::canonicalize(dir).join(path)
    })
}

/// Whether `hooks_dir` sits inside the repository's tracked working tree
/// rather than under its git directory. True for the husky/lefthook pattern:
/// `core.hooksPath` pointing at a directory (e.g. `.husky/`) that is itself
/// committed and shared with every clone. False for the default `.git/hooks`
/// and for a `core.hooksPath` pointing outside the repo entirely.
fn hooks_dir_is_tracked(dir: &Path, hooks_dir: &Path) -> Result<bool> {
    let Ok(toplevel) = git_output(dir, &["rev-parse", "--show-toplevel"]) else {
        // No working tree (bare repo): nothing to be "inside".
        return Ok(false);
    };
    // `--show-toplevel` is always absolute and always names a directory that
    // exists, so canonicalizing it is safe unconditionally. `hooks_dir`
    // (from `resolve_hooks_dir`) is canonicalized the same way, so this
    // `starts_with` compares two paths in the same normalized form rather
    // than risking a resolved-vs-unresolved-symlink or Windows case/`\\?\`
    // mismatch between git's notion of the path and ours.
    let toplevel = spelunk_core::utils::canonicalize(&std::path::PathBuf::from(toplevel));

    let common_dir = git_output(dir, &["rev-parse", "--git-common-dir"])?;
    let common_dir = std::path::PathBuf::from(common_dir);
    let common_dir = if common_dir.is_absolute() {
        common_dir
    } else {
        dir.join(common_dir)
    };
    // The `.git` directory always exists, so this is always safe too.
    let common_dir = spelunk_core::utils::canonicalize(&common_dir);

    Ok(hooks_dir.starts_with(&toplevel) && !hooks_dir.starts_with(&common_dir))
}

/// What [`write_hook`] did.
pub(crate) enum Installed {
    Wrote(std::path::PathBuf),
    /// Ours, but the body changed: a moved binary re-resolves through here.
    Updated(std::path::PathBuf),
    AlreadyPresent(std::path::PathBuf),
}

/// Write `body` to the git-resolved hooks directory for the repo at `dir`,
/// refusing to clobber a hook spelunk did not write.
fn write_hook(dir: &Path, spec: &HookSpec, body: &str) -> Result<Installed> {
    let hooks_dir = resolve_hooks_dir(dir)?;

    // A tracked hooks directory is shared with every teammate on clone: writing
    // into it is committing spelunk's hook to the team, not to this machine, so
    // it needs the user's own commit rather than a silent write on their behalf.
    if hooks_dir_is_tracked(dir, &hooks_dir)? {
        anyhow::bail!(
            "core.hooksPath resolves to {}, which is inside this repository's tracked \
             working tree, so it is shared with every clone. spelunk will not write a hook \
             there on your behalf; add it to that directory yourself, or point \
             core.hooksPath at an untracked location and re-run this command.",
            hooks_dir.display()
        );
    }

    std::fs::create_dir_all(&hooks_dir)?;
    let hook_path = hooks_dir.join(spec.name);

    let mut replacing = false;
    if hook_path.exists() {
        let existing = std::fs::read_to_string(&hook_path)?;
        if !existing.contains(spec.marker) {
            anyhow::bail!(
                "A {} hook already exists at {}.\n\
                 Inspect it and merge manually, or remove it first.",
                spec.name,
                hook_path.display()
            );
        }
        if existing == body {
            return Ok(Installed::AlreadyPresent(hook_path));
        }
        replacing = true;
    }

    std::fs::write(&hook_path, body)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms)?;
    }

    Ok(if replacing {
        Installed::Updated(hook_path)
    } else {
        Installed::Wrote(hook_path)
    })
}

fn hooks_install(args: HooksInstallArgs) -> Result<()> {
    if args.ci {
        print!("{CI_STEP}");
        return Ok(());
    }

    if args.pre_push {
        return install_pre_push();
    }
    install_post_commit()
}

/// Install the post-commit hook in the repo at `dir`. Exposed so `spelunk
/// init --hook` shares this resolution logic rather than re-implementing it
/// against a hardcoded `$GIT_DIR/hooks`.
pub(crate) fn install_post_commit_hook(dir: &Path) -> Result<Installed> {
    write_hook(dir, &POST_COMMIT, POST_COMMIT_HOOK)
}

fn cwd() -> Result<std::path::PathBuf> {
    std::env::current_dir().context("getting current directory")
}

fn install_post_commit() -> Result<()> {
    match install_post_commit_hook(&cwd()?)? {
        Installed::AlreadyPresent(p) => {
            println!("Hook already installed at {}", p.display());
            return Ok(());
        }
        Installed::Updated(p) => println!("Updated post-commit hook at {}", p.display()),
        Installed::Wrote(p) => println!("Installed post-commit hook at {}", p.display()),
    }
    println!("After each commit, spelunk will:");
    println!("  - Re-index the project");
    println!("  - Harvest memory from the new commit");
    println!("Teammates without spelunk installed are unaffected.");
    Ok(())
}

fn install_pre_push() -> Result<()> {
    match write_hook(&cwd()?, &PRE_PUSH, &pre_push_hook_body()?)? {
        Installed::AlreadyPresent(p) => {
            println!("Hook already installed at {}", p.display());
            return Ok(());
        }
        Installed::Updated(p) => println!("Updated pre-push hook at {}", p.display()),
        Installed::Wrote(p) => println!("Installed pre-push hook at {}", p.display()),
    }
    println!("On each `git push`, spelunk will publish your memory to that remote:");
    println!("  - Fetch and merge teammates' memory notes (union, nothing dropped)");
    println!("  - Push refs/notes/spelunk alongside the code you are pushing");
    println!("Your push is never blocked: on failure the hook warns and exits 0.");
    println!("Teammates never receive this hook: git does not clone .git/hooks.");
    Ok(())
}

fn hooks_uninstall() -> Result<()> {
    let hooks_dir = resolve_hooks_dir(&cwd()?)?;
    let mut removed = 0usize;
    let mut foreign: Vec<std::path::PathBuf> = Vec::new();

    for spec in ALL_HOOKS {
        let hook_path = hooks_dir.join(spec.name);
        if !hook_path.exists() {
            continue;
        }
        if !std::fs::read_to_string(&hook_path)?.contains(spec.marker) {
            foreign.push(hook_path);
            continue;
        }
        std::fs::remove_file(&hook_path)?;
        println!("Removed {} hook.", spec.name);
        removed += 1;
    }

    // Only a wholly ineffective uninstall is an error: with a spelunk hook
    // removed, leaving someone else's hook alone is the correct outcome.
    if removed == 0 {
        if let Some(p) = foreign.first() {
            anyhow::bail!(
                "The hook at {} was not installed by spelunk. Remove it manually.",
                p.display()
            );
        }
        println!("No spelunk hooks found.");
        return Ok(());
    }

    for p in &foreign {
        println!(
            "Left {} alone: it was not installed by spelunk.",
            p.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backslash reaches Git Bash intact through single quotes, so a Windows
    /// path embedded raw would resolve to nothing and every push would fail.
    #[test]
    fn a_windows_path_is_forward_slashed() {
        assert_eq!(
            sh_quoted(Path::new(r"C:\Program Files\spelunk\spelunk.exe")),
            "'C:/Program Files/spelunk/spelunk.exe'"
        );
    }

    /// A space in the path is why it is quoted at all.
    #[test]
    fn a_path_with_spaces_stays_one_word() {
        assert_eq!(
            sh_quoted(Path::new("/Users/a b/.local/bin/spelunk")),
            "'/Users/a b/.local/bin/spelunk'"
        );
    }

    /// A quote in the path would otherwise close the string and let the rest of
    /// the path parse as shell words.
    #[test]
    fn a_quote_in_the_path_cannot_escape_the_string() {
        assert_eq!(
            sh_quoted(Path::new("/home/o'brien/bin/spelunk")),
            r"'/home/o'\''brien/bin/spelunk'"
        );
    }

    /// The shim must carry a real path, never the literal placeholder: a hook
    /// reading `exec '{spelunk}'` would fail on every push.
    #[test]
    fn the_shim_embeds_a_resolved_absolute_path() {
        let body = pre_push_hook_body().expect("resolve current_exe");
        let exec = body
            .lines()
            .find(|l| l.starts_with("exec "))
            .expect("the shim execs the command");

        assert!(
            !body.contains("{spelunk}"),
            "placeholder left unsubstituted"
        );
        assert!(
            exec.contains("plumbing publish-notes --best-effort \"$@\""),
            "the shim must delegate every decision to the command: {exec}"
        );
        // `command -v` is withdrawn: it cannot occur (hooks are never cloned)
        // and it broke GUI clients, whose PATH comes from launchd.
        assert!(
            !body.contains("command -v"),
            "the shim must not look spelunk up on PATH: {body}"
        );

        let quoted = sh_quoted(&std::env::current_exe().unwrap());
        assert!(exec.contains(&quoted), "expected {quoted} in: {exec}");
        assert!(
            Path::new(quoted.trim_matches('\'')).is_absolute(),
            "the embedded path must be absolute: {quoted}"
        );
    }

    /// A repo with a real identity and one commit, isolated from the
    /// developer's ambient git config.
    ///
    /// The isolation has to be process-wide (see
    /// `cli::cmd::test_support::isolate_git_config`), not just set on the
    /// setup `Command`s here: `resolve_hooks_dir` (the function under test)
    /// spawns its own git via `git_output`, uninstrumented, and inherits
    /// whatever the process environment holds at that point.
    fn init_repo(dir: &Path) {
        crate::cli::cmd::test_support::isolate_git_config();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
    }

    fn set_hooks_path(dir: &Path, path: &str) {
        let status = std::process::Command::new("git")
            .args(["config", "core.hooksPath", path])
            .current_dir(dir)
            .status()
            .expect("set core.hooksPath");
        assert!(status.success());
    }

    /// A fresh temp dir, canonicalized. `resolve_hooks_dir`/`hooks_dir_is_tracked`
    /// compare their `dir` argument against git's own output (`--show-toplevel`,
    /// `--git-common-dir`), which git always reports symlink-resolved; every real
    /// call site gets a `dir` the same way, via `std::env::current_dir()`, which
    /// resolves symlinks for the same reason. `tempfile`'s raw path does not (on
    /// macOS `$TMPDIR` is itself a symlink), so tests comparing paths must
    /// canonicalize to match what these functions actually receive in practice.
    ///
    /// This must go through [`spelunk_core::utils::canonicalize`] (the `dunce`
    /// wrapper), not `Path::canonicalize`/`std::fs::canonicalize` directly: on
    /// Windows the std version returns the verbatim `\\?\`-prefixed form, while
    /// `resolve_hooks_dir` and `hooks_dir_is_tracked` canonicalize through the
    /// `dunce` wrapper and so never produce that prefix. Building an expected
    /// path from the verbatim form and comparing it against the non-verbatim
    /// form those functions actually return compares two different spellings
    /// of the same real directory and fails `assert_eq!`/`PathBuf` equality
    /// even though nothing is wrong - the fix is to canonicalize both sides
    /// through the identical helper, not to chase the `\\?\` prefix itself.
    fn canonical_tmp_dir(tmp: &tempfile::TempDir) -> std::path::PathBuf {
        spelunk_core::utils::canonicalize(tmp.path())
    }

    #[test]
    fn resolve_hooks_dir_defaults_to_dot_git_hooks_when_core_hooks_path_is_unset() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = canonical_tmp_dir(&tmp);
        init_repo(&dir);

        assert_eq!(
            resolve_hooks_dir(&dir).unwrap(),
            dir.join(".git").join("hooks")
        );
    }

    #[test]
    fn resolve_hooks_dir_honors_a_relative_core_hooks_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = canonical_tmp_dir(&tmp);
        init_repo(&dir);
        set_hooks_path(&dir, ".githooks-custom");

        assert_eq!(
            resolve_hooks_dir(&dir).unwrap(),
            dir.join(".githooks-custom")
        );
    }

    #[test]
    fn hooks_dir_is_tracked_false_for_the_default_git_hooks_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = canonical_tmp_dir(&tmp);
        init_repo(&dir);
        let hooks_dir = resolve_hooks_dir(&dir).unwrap();

        assert!(!hooks_dir_is_tracked(&dir, &hooks_dir).unwrap());
    }

    #[test]
    fn hooks_dir_is_tracked_true_for_a_directory_inside_the_working_tree() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = canonical_tmp_dir(&tmp);
        init_repo(&dir);
        set_hooks_path(&dir, ".husky");
        let hooks_dir = resolve_hooks_dir(&dir).unwrap();

        assert!(hooks_dir_is_tracked(&dir, &hooks_dir).unwrap());
    }

    #[test]
    fn hooks_dir_is_tracked_false_for_a_hooks_path_outside_the_repository() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = canonical_tmp_dir(&tmp);
        let repo = base.join("repo");
        let outside = base.join("outside-hooks");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        init_repo(&repo);
        set_hooks_path(&repo, outside.to_str().unwrap());
        let hooks_dir = resolve_hooks_dir(&repo).unwrap();

        assert!(!hooks_dir_is_tracked(&repo, &hooks_dir).unwrap());
    }

    /// `dir` reaches these functions as `std::env::current_dir()` in real use,
    /// which does not resolve symlinks. On a machine where the OS temp dir has
    /// a symlinked component (e.g. macOS, where `$TMPDIR` sits under `/var`,
    /// itself a symlink to `/private/var`), git resolves that away when it
    /// prints `--show-toplevel` / `--git-common-dir`, while a hooks_dir built
    /// by joining onto the raw, un-resolved `dir` does not. A component-wise
    /// `starts_with` between the two then fails even though both name the
    /// same real directory - the same class of bug as a Windows drive-letter
    /// case or `\\?\`-prefix mismatch, reproduced here without needing Windows.
    #[test]
    fn hooks_dir_is_tracked_true_with_an_unresolved_symlinked_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf(); // deliberately NOT canonicalized
        init_repo(&dir);
        set_hooks_path(&dir, ".husky");
        let hooks_dir = resolve_hooks_dir(&dir).unwrap();

        assert!(
            hooks_dir_is_tracked(&dir, &hooks_dir).unwrap(),
            "must detect the tracked hooks dir even when `dir` itself was \
             never canonicalized by the caller"
        );
    }
}
