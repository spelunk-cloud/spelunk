//! Regression test for the `spelunk memory harvest` argument-injection guard
//! (security review finding: `--branch`/`--git-range` values starting with
//! `-` must never reach `git log` as an option — see
//! `crates/spelunk-cli/src/cli/cmd/memory/harvest.rs::reject_option_like_ref`).
//!
//! A malicious value like `--branch=--output=/tmp/x` must be rejected with a
//! clear error before any `git` subprocess runs, and must never create or
//! overwrite the target file.

mod plumbing_helpers;
use plumbing_helpers::{init_git_repo, spelunk_bin};

use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

// `server_url`/`project_id` only satisfy harvest's upfront "server
// configured" gate: the injection guard under test fires before any request
// reaches that address, so it deliberately never needs to be reachable.
// `Config::load` only honors those two fields from project-level
// `.spelunk/config.toml` (or env), never the global `--config` file, so they
// land in `dir`'s project config instead of `dir/config.toml`. Every caller
// sets `.current_dir(dir)`.
//
// `mode = "cloud_first"` (global config) is required since the 2026-07-23
// ADR-004 revision: `Config::resolve_inference_url()` no longer falls back to
// an unreachable `server_url` under the default `local_first`, so the Tier-0
// gate this helper means to satisfy unconditionally would (correctly) start
// checking real reachability instead, and nothing listens on the address
// below. Only `cloud_first` keeps the old "a configured server_url always
// satisfies the gate" behavior this test relies on to reach the injection
// guard beneath it.
fn write_harvest_config(dir: &std::path::Path) -> std::path::PathBuf {
    let db_path = dir.join("memory.db");
    let config_path = dir.join("config.toml");
    let content = format!(
        "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1234\"\nmode = \"cloud_first\"\n",
        db_path
    );
    fs::write(&config_path, content).expect("write config.toml");
    plumbing_helpers::write_project_server_config(dir, "http://127.0.0.1:7777", "test/proj");
    config_path
}

/// Initialize a throwaway git repo with a single commit so a real HEAD exists.
fn init_repo(dir: &std::path::Path) {
    init_git_repo(dir);
    // ADR-067: `memory harvest` fails closed without a local `.spelunk/` project,
    // so make this repo a real project — otherwise the guard fires before the
    // ref-injection check under test is reached.
    fs::create_dir_all(dir.join(".spelunk")).expect("create .spelunk");
}

#[test]
fn harvest_rejects_option_like_branch_and_does_not_touch_victim_file() {
    let temp = tempdir().unwrap();
    init_repo(temp.path());

    // Victim file elsewhere on disk that a successful `--output=` injection
    // would create/overwrite via `git log --output=<path>`.
    let victim_dir = tempdir().unwrap();
    let victim_path = victim_dir.path().join("victim.txt");
    assert!(!victim_path.exists());

    let config_path = write_harvest_config(temp.path());

    let malicious_branch_arg = format!("--branch=--output={}", victim_path.display());

    let mut cmd = spelunk_bin();
    cmd.current_dir(temp.path())
        .env_remove("SPELUNK_SERVER_URL")
        .env_remove("SPELUNK_LLM_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("harvest")
        .arg(&malicious_branch_arg);

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("rejected").or(predicate::str::contains("Invalid")));

    assert!(
        !victim_path.exists(),
        "option-injection via --branch must not create the victim file"
    );
}

#[test]
fn harvest_rejects_option_like_git_range() {
    let temp = tempdir().unwrap();
    init_repo(temp.path());

    let victim_dir = tempdir().unwrap();
    let victim_path = victim_dir.path().join("victim2.txt");

    let config_path = write_harvest_config(temp.path());

    let malicious_range_arg = format!("--git-range=--output={}", victim_path.display());

    let mut cmd = spelunk_bin();
    cmd.current_dir(temp.path())
        .env_remove("SPELUNK_SERVER_URL")
        .env_remove("SPELUNK_LLM_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("harvest")
        .arg(&malicious_range_arg);

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("rejected").or(predicate::str::contains("Invalid")));

    assert!(!victim_path.exists());
}

/// Short option-shaped refs (e.g. `-1`, mimicking `git log -1`) must be
/// rejected too, not just long `--flag=value` forms. Regression coverage
/// for the "short option that looks like a legitimate-ish ref" edge case.
#[test]
fn harvest_rejects_short_option_like_branch() {
    let temp = tempdir().unwrap();
    init_repo(temp.path());

    let config_path = write_harvest_config(temp.path());

    let mut cmd = spelunk_bin();
    cmd.current_dir(temp.path())
        .env_remove("SPELUNK_SERVER_URL")
        .env_remove("SPELUNK_LLM_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("harvest")
        .arg("--branch=-1");

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("rejected").or(predicate::str::contains("Invalid")));
}

/// A ref value that is exactly the `--` end-of-options marker must be
/// rejected (not silently accepted or treated as a no-op separator).
#[test]
fn harvest_rejects_bare_double_dash_branch() {
    let temp = tempdir().unwrap();
    init_repo(temp.path());

    let config_path = write_harvest_config(temp.path());

    let mut cmd = spelunk_bin();
    cmd.current_dir(temp.path())
        .env_remove("SPELUNK_SERVER_URL")
        .env_remove("SPELUNK_LLM_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("harvest")
        .arg("--branch=--");

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("rejected").or(predicate::str::contains("Invalid")));
}

/// A ref value starting with `-` but containing shell metacharacters must
/// still be rejected by the option-like guard (belt-and-braces: no shell is
/// ever involved since git is spawned via argv, but the leading `-` alone is
/// grounds for rejection, and this pins that no metacharacter-based bypass
/// exists).
#[test]
fn harvest_rejects_option_like_branch_with_shell_metacharacters() {
    let temp = tempdir().unwrap();
    init_repo(temp.path());

    let victim_dir = tempdir().unwrap();
    let victim_path = victim_dir.path().join("victim3.txt");

    let config_path = write_harvest_config(temp.path());

    let malicious_branch_arg = format!(
        "--branch=--output={};touch /tmp/oss61-pwned",
        victim_path.display()
    );

    let mut cmd = spelunk_bin();
    cmd.current_dir(temp.path())
        .env_remove("SPELUNK_SERVER_URL")
        .env_remove("SPELUNK_LLM_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("harvest")
        .arg(&malicious_branch_arg);

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("rejected").or(predicate::str::contains("Invalid")));

    assert!(!victim_path.exists());
    assert!(!std::path::Path::new("/tmp/oss61-pwned").exists());
}
