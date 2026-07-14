use anyhow::{Context, Result};
use clap::{Args, Subcommand};

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

const PRE_PUSH_HOOK: &str = r#"#!/bin/sh
# spelunk pre-push hook — installed by `spelunk hooks install --pre-push`
# Publishes spelunk memory (refs/notes/spelunk) to the remote you are pushing to,
# so decisions travel with the code they describe.
# Silently skips if `spelunk` is not in PATH, so teammates without spelunk are unaffected.
#
# This hook never exits non-zero: a failing pre-push hook aborts the branch push
# outright, and sharing memory must never cost you your push.

if ! command -v spelunk >/dev/null 2>&1; then
  exit 0
fi

# Recursion guard. Without it the notes push below re-enters this hook: a version
# lacking one recursed until the process table was exhausted, while every outer
# push still reported success. `--no-verify` on that push is the real guard; this
# sentinel is belt and braces.
if [ -n "${SPELUNK_NOTES_PUSH:-}" ]; then
  exit 0
fi

REMOTE="${1:-origin}"

# Nothing recorded locally, or nothing resolvable to publish to.
git rev-parse --verify --quiet refs/notes/spelunk >/dev/null 2>&1 || exit 0
git remote get-url "$REMOTE" >/dev/null 2>&1 || exit 0

attempt=1
while [ "$attempt" -le 3 ]; do
  # Onto the tracking ref, never over refs/notes/spelunk: fetching straight onto
  # the working ref force-updates it and drops notes you have not pushed yet.
  git fetch --quiet "$REMOTE" "+refs/notes/spelunk:refs/notes/origin/spelunk" >/dev/null 2>&1

  # `-s` is explicit: the notes.mergeStrategy default is `manual`, which exits 1
  # and strands .git/NOTES_MERGE_WORKTREE. Your own setting is never written.
  if git rev-parse --verify --quiet refs/notes/origin/spelunk >/dev/null 2>&1; then
    git notes --ref=spelunk merge -s cat_sort_uniq refs/notes/origin/spelunk >/dev/null 2>&1
  fi

  # Never forced: the union above already carries both sides, so a force-push
  # could only discard a teammate's memory.
  err=$(SPELUNK_NOTES_PUSH=1 git push --no-verify "$REMOTE" \
    refs/notes/spelunk:refs/notes/spelunk 2>&1) && exit 0

  # Only a lost race is retried: a teammate landed notes between our fetch and
  # our push, so re-merging theirs lets the next attempt fast-forward. Anything
  # else (offline, rejected by the remote) would fail identically three times.
  case "$err" in
    *non-fast-forward* | *"fetch first"*) ;;
    *) break ;;
  esac
  attempt=$((attempt + 1))
done

echo "spelunk: could not publish memory notes to '$REMOTE'. Your code push is unaffected." >&2
echo "spelunk: retry with: git push $REMOTE refs/notes/spelunk" >&2
exit 0
"#;

const CI_STEP: &str = r#"# Add to your .github/workflows/ file:
- name: Update spelunk index
  run: |
    if command -v spelunk >/dev/null 2>&1; then
      spelunk index . --detach
      spelunk memory harvest --git-range HEAD~1..HEAD --detach
    fi
"#;

/// An installable hook: git's filename for it, the marker line identifying a
/// spelunk-written copy, and the script body.
struct HookSpec {
    name: &'static str,
    marker: &'static str,
    body: &'static str,
}

const POST_COMMIT: HookSpec = HookSpec {
    name: "post-commit",
    marker: "spelunk post-commit hook",
    body: POST_COMMIT_HOOK,
};

const PRE_PUSH: HookSpec = HookSpec {
    name: "pre-push",
    marker: "spelunk pre-push hook",
    body: PRE_PUSH_HOOK,
};

/// Every hook `uninstall` considers.
const ALL_HOOKS: [&HookSpec; 2] = [&POST_COMMIT, &PRE_PUSH];

/// The command that installs the pre-push hook. `init` names it when it tells
/// the user their memory stays local until they opt in (ADR-069 D3).
pub const PRE_PUSH_INSTALL_CMD: &str = "spelunk hooks install --pre-push";

/// Whether spelunk's own pre-push hook is installed in the repo holding the CWD.
/// False for a foreign pre-push hook: that one publishes nothing.
pub fn pre_push_installed() -> bool {
    let Ok(git_dir) = find_git_dir() else {
        return false;
    };
    std::fs::read_to_string(git_dir.join("hooks").join(PRE_PUSH.name))
        .is_ok_and(|body| body.contains(PRE_PUSH.marker))
}

fn find_git_dir() -> Result<std::path::PathBuf> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo = gix::discover(&cwd).context("Not inside a git repository.")?;
    Ok(repo.git_dir().to_path_buf())
}

/// Whether `write_hook` wrote the hook or found its own copy already there.
enum Installed {
    Wrote(std::path::PathBuf),
    AlreadyPresent(std::path::PathBuf),
}

/// Write `spec`'s body to `<git-dir>/hooks/<name>`, refusing to clobber a hook
/// spelunk did not write.
fn write_hook(spec: &HookSpec) -> Result<Installed> {
    let hooks_dir = find_git_dir()?.join("hooks");
    std::fs::create_dir_all(&hooks_dir)?;
    let hook_path = hooks_dir.join(spec.name);

    if hook_path.exists() {
        let existing = std::fs::read_to_string(&hook_path)?;
        if existing.contains(spec.marker) {
            return Ok(Installed::AlreadyPresent(hook_path));
        }
        anyhow::bail!(
            "A {} hook already exists at {}.\n\
             Inspect it and merge manually, or remove it first.",
            spec.name,
            hook_path.display()
        );
    }

    std::fs::write(&hook_path, spec.body)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms)?;
    }

    Ok(Installed::Wrote(hook_path))
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

fn install_post_commit() -> Result<()> {
    match write_hook(&POST_COMMIT)? {
        Installed::AlreadyPresent(p) => {
            println!("Hook already installed at {}", p.display());
            return Ok(());
        }
        Installed::Wrote(p) => println!("Installed post-commit hook at {}", p.display()),
    }
    println!("After each commit, spelunk will:");
    println!("  - Re-index the project");
    println!("  - Harvest memory from the new commit");
    println!("Teammates without spelunk installed are unaffected.");
    Ok(())
}

fn install_pre_push() -> Result<()> {
    match write_hook(&PRE_PUSH)? {
        Installed::AlreadyPresent(p) => {
            println!("Hook already installed at {}", p.display());
            return Ok(());
        }
        Installed::Wrote(p) => println!("Installed pre-push hook at {}", p.display()),
    }
    println!("On each `git push`, spelunk will publish your memory to that remote:");
    println!("  - Fetch and merge teammates' memory notes (union, nothing dropped)");
    println!("  - Push refs/notes/spelunk alongside the code you are pushing");
    println!("Your push is never blocked: on failure the hook warns and exits 0.");
    println!("Teammates without spelunk installed are unaffected.");
    Ok(())
}

fn hooks_uninstall() -> Result<()> {
    let hooks_dir = find_git_dir()?.join("hooks");
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
