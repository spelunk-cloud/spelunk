//! Integration tests for the `spelunk memory harvest` Tier-0 server gate
//! (ADR-002 / issue #260).
//!
//! Harvest now requires `server_url` in config — there is no local-model path.
//! These tests don't need a running server or a real git repo; they only
//! exercise the early server-gate check.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin;

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

// Substring that appears in the Tier-0 error from `harvest_requires_server()`.
const SERVER_REQUIRED: &str = "'spelunk memory harvest' requires spelunk-server";

// ── helpers ───────────────────────────────────────────────────────────────────

/// Write a minimal config file under `dir`.  `extra` is appended verbatim.
fn write_harvest_config(dir: &std::path::Path, extra: &str) -> std::path::PathBuf {
    // ADR-067: harvest fails closed without a local `.spelunk/` project, which
    // would pre-empt the server-gate check under test. Make `dir` a real project.
    fs::create_dir_all(dir.join(".spelunk")).expect("create .spelunk");
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
    let mut cmd = spelunk_bin();
    cmd.current_dir(dir)
        .env_remove("SPELUNK_SERVER_URL")
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
        .stderr(predicate::str::contains(SERVER_REQUIRED))
        // With no `server_url` the guidance must point at the local server and
        // must NOT tell a solo user to configure a team `server_url`.
        .stderr(predicate::str::contains("spelunk server start"))
        .stderr(predicate::str::contains("server_url").not());
}

// ── (b) server_url set → gate passes (fails later for another reason) ─────────

#[test]
fn harvest_check_passes_when_server_url_is_set() {
    let temp = tempdir().unwrap();
    // `Config::load` only honors `server_url`/`project_id` from project-level
    // `.spelunk/config.toml` (or env), never the global `--config` file, so
    // they're written separately from `write_harvest_config`'s `extra`.
    //
    // `mode = "cloud_first"` is required since the 2026-07-23 ADR-004
    // revision: with `SPELUNK_NO_SERVER=1` forcing
    // `Tier::Offline` (no loopback probe at all) and no explicit mode, a
    // bare `server_url` now defaults to `local_first`, which never falls
    // back to `server_url` for inference — so the Tier-0 gate this test
    // means to bypass would (correctly) still fire. This test's intent is
    // "an explicit server_url IS used for inference", which is the
    // `cloud_first` case.
    let config_path = write_harvest_config(temp.path(), "mode = \"cloud_first\"\n");
    plumbing_helpers::write_project_server_config(
        temp.path(),
        "http://127.0.0.1:7777",
        "test/proj",
    );

    // `SPELUNK_NO_SERVER=1` (set by `harvest_cmd` for the other two tests in
    // this file) is a hard offline kill-switch: `resolve_mode()` forces
    // `Offline` under it regardless of the configured `mode`
    // (`resolve_mode_no_server_env_forces_offline`), which would make
    // `resolve_inference_url()` return `None` here even in `cloud_first` and
    // defeat the very case this test means to cover. It must be removed for
    // this test specifically. This is safe: unlike loopback auto-discovery,
    // `cloud_first`'s `server_url` fallback in `resolve_inference_url` does
    // not depend on the URL actually being reachable, so a real local server
    // happening to run on 7777 cannot change whether the Tier-0 gate fires.
    //
    // The command will fail (no live server, no git repo) but NOT with the
    // Tier-0 "requires spelunk-server" message.
    harvest_cmd(&config_path, temp.path())
        .env_remove("SPELUNK_NO_SERVER")
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
