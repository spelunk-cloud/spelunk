//! Shared test helpers.
#![allow(dead_code)]
//!
//! Import with `mod common;` or `use crate::common::*;` inside integration tests.

use std::sync::OnceLock;

/// Register the sqlite-vec extension exactly once for the test process.
///
/// sqlite3_auto_extension is process-global; calling it more than once per
/// address is a no-op but calling it from multiple threads without
/// synchronisation is UB.  `OnceLock` guarantees single initialisation.
///
/// Tests that open a `Database` **must** call this first.
/// Annotate those tests with `#[serial_test::serial]` so the global
/// registration happens before any connection is opened.
pub fn register_sqlite_vec() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        #[allow(clippy::missing_transmute_annotations)]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

/// Open an in-memory `spelunk_core::storage::Database` for tests.
///
/// Calls `register_sqlite_vec()` automatically.
pub fn open_test_db() -> spelunk_core::storage::Database {
    register_sqlite_vec();
    spelunk_core::storage::Database::open(std::path::Path::new(":memory:"))
        .expect("failed to open in-memory database")
}

// Drop the ambient global/system git config for every git this process
// spawns, including one the code under test spawns itself: process-wide via
// `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM=/dev/null`, guarded by `Once`. Also
// clears `GIT_AUTHOR_*`/`GIT_COMMITTER_*`/`EMAIL`, which git resolves before
// consulting config at all, so an ambient value there would otherwise
// override a test's own explicit `user.name`/`user.email`.
//
// This is `spelunk-core`'s `tests/`-side copy of
// `spelunk_core::test_support::isolate_git_config`. An integration test
// binary links the crate externally, so it can't reach that `#[cfg(test)]`-
// reachable definition directly without a self-referencing dev-dependency:
// tried, and it breaks this repo's shared-`CARGO_TARGET_DIR`-across-
// worktrees pre-commit hook (fails with `unresolved import` against a target
// dir last built from a different Cargo.lock). This duplicate is the actual
// floor, not the self-dependency trick.
pub fn isolate_git_config() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: every git-touching helper here calls this first and
        // `Once` blocks the rest until it returns, so no thread can be
        // spawning git (reading environ) while these run.
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
            for var in [
                "GIT_AUTHOR_NAME",
                "GIT_AUTHOR_EMAIL",
                "GIT_AUTHOR_DATE",
                "GIT_COMMITTER_NAME",
                "GIT_COMMITTER_EMAIL",
                "GIT_COMMITTER_DATE",
                "EMAIL",
            ] {
                std::env::remove_var(var);
            }
        }
    });
}

// Build a `git` `Command` rooted at `cwd`, isolated via `isolate_git_config`
// first, so a caller cannot construct an un-isolated one by forgetting a
// separate setup step. `scripts/check-git-isolation.sh` enforces that a test
// file spawning `git` wires this in.
pub fn git_command(cwd: &std::path::Path) -> std::process::Command {
    isolate_git_config();
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(cwd);
    cmd
}
