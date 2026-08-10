//! Re-export of the shared git-isolation fixture for `cli::cmd`'s
//! `#[cfg(test)]` unit tests.
//!
//! `tests/plumbing_helpers.rs` re-exports the same underlying
//! `spelunk_core::test_support::isolate_git_config` for the `tests/`
//! integration binaries; a unit test compiled into `src/` cannot reach a
//! file under `tests/`, so this is the `src/`-side path to the one
//! definition.

pub(crate) use spelunk_core::test_support::isolate_git_config;
