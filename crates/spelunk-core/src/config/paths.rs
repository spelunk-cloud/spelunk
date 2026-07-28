use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Returns `~/.config/spelunk/`, or `SPELUNK_CONFIG_DIR` when set.
///
/// On all platforms we use `~/.config` rather than the OS-native config dir
/// (e.g. `~/Library/Application Support` on macOS) so that the path matches
/// what the CLI documentation and error messages say, and so that config files
/// work the same way across Linux and macOS.
///
/// `SPELUNK_CONFIG_DIR` is a supported override of the entire path, not
/// dev-only cruft: it is load-bearing on Windows, where `dirs::home_dir()` 6.x
/// calls `SHGetKnownFolderPath` (a Registry lookup) rather than reading
/// `HOME`/`USERPROFILE`, making a per-process environment override of `HOME`
/// ineffective (the identical portability gap documented on
/// `spelunk_state_dir` in the CLI's `capability/probe.rs` and on
/// `web_to_md_script_path` in `memory/add.rs`). Tests that need an isolated
/// config/secret-store location (this crate's own `config::mod::tests`, and
/// the CLI integration tests via `spelunk_bin_in`) set this instead of relying
/// on `HOME` alone.
pub(in crate::config) fn spelunk_config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SPELUNK_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("spelunk")
}

/// Walk up from `start` looking for `.spelunk/index.db`.
/// Returns the first match found, or `None` if the filesystem root is reached.
///
/// If `start` is inside a git linked worktree, the walk begins from the main
/// worktree root so that linked worktrees share the same index without a symlink.
pub fn find_project_db(start: &Path) -> Option<PathBuf> {
    let resolved = crate::utils::resolve_main_worktree_root(start);
    let mut dir = resolved;
    loop {
        let candidate = dir.join(".spelunk").join("index.db");
        if candidate.exists() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Walk up from `start` for a `.spelunk/` **directory** (worktree-aware).
/// Returns the `.spelunk` dir path, or `None` if none is found before the root.
///
/// Keys on the directory, not `index.db`: `spelunk init --no-index` writes a
/// `.spelunk/config.toml` with no index, and memory needs no index. Linked
/// worktrees resolve to the main worktree's `.spelunk/` (mirrors
/// [`find_project_db`]).
pub fn find_project_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = crate::utils::resolve_main_worktree_root(start);
    loop {
        let candidate = dir.join(".spelunk");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// ADR-067: resolve the local project's `.spelunk/index.db` base path anchored at
/// `start`, failing closed when `start` has no `.spelunk/` project instead of
/// silently falling back to the global `~/.config/spelunk/` store.
///
/// Explicit `--db` / index-path callers bypass this (an explicit store is always
/// honored). Memory callers apply `.with_file_name("memory.db")` to the result.
/// `allow_global` restores the legacy global fallback and is reserved for a
/// future `--global` flag (ADR-067 D2); no caller sets it today.
pub fn require_project_db_at(
    start: &Path,
    cfg_default: &Path,
    allow_global: bool,
) -> Result<PathBuf> {
    if let Some(spelunk_dir) = find_project_dir(start) {
        return Ok(spelunk_dir.join("index.db"));
    }
    if allow_global {
        return Ok(cfg_default.to_path_buf());
    }
    anyhow::bail!("no spelunk project here. Run 'spelunk init' first")
}

/// [`require_project_db_at`] anchored at the current working directory.
pub fn require_project_db(cfg_default: &Path, allow_global: bool) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("resolving current directory")?;
    require_project_db_at(&cwd, cfg_default, allow_global)
}

/// Walk up from `start` looking for `.spelunk/config.toml` (project-level config).
/// Stops at the filesystem root. Returns the path if found.
pub(in crate::config) fn find_project_config(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let candidate = dir.join(".spelunk").join("config.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Resolve the database path.
///
/// Priority: explicit `--db` arg > project DB (walk up from CWD) > `cfg_default`.
pub fn resolve_db(explicit: Option<&Path>, cfg_default: &Path) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(cwd) = std::env::current_dir()
        && let Some(p) = find_project_db(&cwd)
    {
        return p;
    }
    cfg_default.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── spelunk_config_dir / SPELUNK_CONFIG_DIR override ─────────────────────

    /// `SPELUNK_CONFIG_DIR` wins over `dirs::home_dir()`-derived resolution.
    /// This is the override that makes per-test isolation possible on
    /// Windows, where `dirs::home_dir()` does not read `HOME`.
    #[test]
    #[serial_test::serial(spelunk_config_dir_env)]
    fn spelunk_config_dir_honors_env_override() {
        let tmp = TempDir::new().unwrap();
        let override_dir = tmp.path().join("custom-config-dir");
        unsafe { std::env::set_var("SPELUNK_CONFIG_DIR", &override_dir) };
        let got = spelunk_config_dir();
        unsafe { std::env::remove_var("SPELUNK_CONFIG_DIR") };
        assert_eq!(got, override_dir);
    }

    #[test]
    #[serial_test::serial(spelunk_config_dir_env)]
    fn spelunk_config_dir_falls_back_to_home_when_unset() {
        unsafe { std::env::remove_var("SPELUNK_CONFIG_DIR") };
        let got = spelunk_config_dir();
        assert!(
            got.ends_with(Path::new(".config").join("spelunk")),
            "got: {}",
            got.display()
        );
    }

    // ── find_project_dir / require_project_db_at (ADR-067) ───────────────────

    /// Exact fail-closed error text (ADR-067; em dash restructured out per the
    /// no-em-dash house rule for user-facing copy).
    const NO_PROJECT_ERR: &str = "no spelunk project here. Run 'spelunk init' first";

    #[test]
    fn find_project_dir_finds_dot_spelunk_at_start() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".spelunk")).unwrap();
        assert_eq!(
            find_project_dir(tmp.path()),
            Some(tmp.path().join(".spelunk"))
        );
    }

    #[test]
    fn find_project_dir_walks_up_from_subdir() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".spelunk")).unwrap();
        let sub = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(find_project_dir(&sub), Some(tmp.path().join(".spelunk")));
    }

    #[test]
    fn find_project_dir_none_when_absent() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(find_project_dir(tmp.path()), None);
    }

    #[test]
    fn find_project_dir_ignores_dot_spelunk_file() {
        // A regular file named `.spelunk` is not a project dir.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".spelunk"), "not a dir").unwrap();
        assert_eq!(find_project_dir(tmp.path()), None);
    }

    #[test]
    fn require_project_db_at_returns_scoped_index_db_with_project() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".spelunk")).unwrap();
        let global = PathBuf::from("/nonexistent/global/index.db");
        let got = require_project_db_at(tmp.path(), &global, false).unwrap();
        assert_eq!(got, tmp.path().join(".spelunk").join("index.db"));
    }

    #[test]
    fn require_project_db_at_walks_up_to_project() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".spelunk")).unwrap();
        let sub = tmp.path().join("crates").join("x");
        std::fs::create_dir_all(&sub).unwrap();
        let global = PathBuf::from("/nonexistent/global/index.db");
        let got = require_project_db_at(&sub, &global, false).unwrap();
        assert_eq!(got, tmp.path().join(".spelunk").join("index.db"));
    }

    #[test]
    fn require_project_db_at_errors_without_project() {
        let tmp = TempDir::new().unwrap();
        let global = PathBuf::from("/nonexistent/global/index.db");
        let err = require_project_db_at(tmp.path(), &global, false).unwrap_err();
        assert_eq!(err.to_string(), NO_PROJECT_ERR);
    }

    #[test]
    fn require_project_db_at_allow_global_returns_default_when_no_project() {
        // The reserved --global opt-in path (ADR-067 D2) restores the legacy
        // global fallback instead of failing closed.
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("global-index.db");
        let got = require_project_db_at(tmp.path(), &global, true).unwrap();
        assert_eq!(got, global);
    }

    /// Build a linked-worktree fixture: `<main>/.spelunk/index.db` exists and
    /// `<wt>` is a linked worktree with NO local `.spelunk/`. Returns
    /// `(main_root, wt_root, index_db)`. Mirrors the fixture in `utils::tests`.
    fn linked_worktree_fixture(tmp: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
        let main_root = tmp.path().join("main");
        let wt_root = tmp.path().join("feat-branch");
        std::fs::create_dir_all(&main_root).unwrap();
        std::fs::create_dir_all(&wt_root).unwrap();

        // Main worktree owns the shared index.
        std::fs::create_dir_all(main_root.join(".spelunk")).unwrap();
        let index_db = main_root.join(".spelunk").join("index.db");
        std::fs::write(&index_db, b"").unwrap();

        // wt_root/.git is a file pointing at <main>/.git/worktrees/<name>.
        let gitdir_entry = main_root.join(".git").join("worktrees").join("feat-branch");
        std::fs::create_dir_all(&gitdir_entry).unwrap();
        std::fs::write(
            wt_root.join(".git"),
            format!("gitdir: {}\n", gitdir_entry.display()),
        )
        .unwrap();

        (main_root, wt_root, index_db)
    }

    #[test]
    fn find_project_db_resolves_worktree_to_main_index() {
        // A linked worktree with no local `.spelunk/` resolves reads to the
        // main worktree's shared index, with no setup step.
        let tmp = TempDir::new().unwrap();
        let (_main_root, wt_root, index_db) = linked_worktree_fixture(&tmp);
        assert!(
            !wt_root.join(".spelunk").exists(),
            "worktree must have no local .spelunk/"
        );
        assert_eq!(find_project_db(&wt_root), Some(index_db));
    }

    #[test]
    fn require_project_db_at_resolves_worktree_to_main_index() {
        let tmp = TempDir::new().unwrap();
        let (main_root, wt_root, _index_db) = linked_worktree_fixture(&tmp);
        let global = PathBuf::from("/nonexistent/global/index.db");
        let got = require_project_db_at(&wt_root, &global, false).unwrap();
        assert_eq!(got, main_root.join(".spelunk").join("index.db"));
    }
}
