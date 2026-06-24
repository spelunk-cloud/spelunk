//! Integration tests for the `spelunk memory harvest` Tier-0 server gate
//! (ADR-002 / issue #260).
//!
//! Harvest now requires `server_url` in config — there is no local-model path.
//! These tests don't need a running server or a real git repo; they only
//! exercise the early server-gate check.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

// Substring that appears in the Tier-0 error from `harvest_requires_server()`.
const SERVER_REQUIRED: &str = "'spelunk memory harvest' requires spelunk-server";

// ── helpers ───────────────────────────────────────────────────────────────────

/// Write a minimal config file under `dir`.  `extra` is appended verbatim.
fn write_harvest_config(dir: &std::path::Path, extra: &str) -> std::path::PathBuf {
    let db_path = dir.join("memory.db");
    let config_path = dir.join("config.toml");
    let content = format!(
        "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1234\"\n{extra}",
        db_path
    );
    fs::write(&config_path, content).expect("write config.toml");
    config_path
}

/// Build a `spelunk --config <cfg> memory harvest --git-range HEAD~1..HEAD` command.
fn harvest_cmd(config_path: &std::path::Path, dir: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.current_dir(dir)
        .env_remove("SPELUNK_SERVER_URL")
        .env_remove("SPELUNK_MEMORY_SERVER_URL")
        .env_remove("SPELUNK_LLM_URL")
        // Disable loopback auto-discovery so the server gate fires before the
        // git-log step even when a local spelunk-server happens to be running.
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(config_path)
        .arg("memory")
        .arg("harvest")
        .arg("--git-range")
        .arg("HEAD~1..HEAD");
    cmd
}

// ── (a) no server_url → server-required error ─────────────────────────────────

#[test]
fn harvest_fails_with_actionable_error_when_no_server_and_no_model() {
    let temp = tempdir().unwrap();
    // Config has no server_url.
    let config_path = write_harvest_config(temp.path(), "");

    harvest_cmd(&config_path, temp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(SERVER_REQUIRED));
}

// ── (b) server_url set → gate passes (fails later for another reason) ─────────

#[test]
fn harvest_check_passes_when_server_url_is_set() {
    let temp = tempdir().unwrap();
    // server_url is set; project_id is required alongside it.
    let config_path = write_harvest_config(
        temp.path(),
        "server_url = \"http://127.0.0.1:7777\"\nproject_id = \"test/proj\"\n",
    );

    // The command will fail (no live server, no git repo) but NOT with the
    // Tier-0 "requires spelunk-server" message.
    harvest_cmd(&config_path, temp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(SERVER_REQUIRED).not());
}

// ── (c) llm_model set without server_url → still fails (no local-model path) ──

#[test]
fn harvest_fails_when_llm_model_set_but_no_server_url() {
    let temp = tempdir().unwrap();
    // llm_model is set but no server_url — local LLM no longer supported for harvest.
    let config_path = write_harvest_config(temp.path(), "llm_model = \"local-chat-model\"\n");

    harvest_cmd(&config_path, temp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(SERVER_REQUIRED));
}
