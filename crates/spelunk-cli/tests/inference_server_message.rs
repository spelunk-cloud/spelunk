//! End-to-end coverage for the inference-server-required guidance.
//!
//! The bug: inference-only commands (`memory search --mode semantic|hybrid`,
//! `memory timeline`, `plumbing embed`) told a solo user with no server to set a
//! team `server_url` — when all they needed was `spelunk server start`. The fix
//! routes every such caller's *effective* `server_url` (which is `None` for a
//! solo/no-server user, never the auto-discovered loopback URL) into
//! `capability::inference_server_required_message`, whose no-`server_url` branch
//! must point at the local server and must NOT mention `server_url`.
//!
//! The engineer's unit tests pin the pure function and `embed_text` body
//! surfacing. These tests close the highest-value gap: a real CLI invocation, so
//! a caller that passed the *wrong* argument (e.g. the loopback inference URL, or
//! a hard-coded `None` that suppresses a legitimately-configured URL) would be
//! caught — a pure-fn test cannot see the wiring.
//!
//! All cases drive the no-`server_url` branch, because that is the branch the
//! bug regressed and the only branch these callers can reach: each gates on
//! `from_config` / `resolve_inference_url()` being `None`, which is only true
//! when `server_url` is also `None` (`resolve_inference_url` falls back to
//! `server_url`). See the report accompanying this change for the reachability
//! analysis of the `Some(url)` branch.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin;

use predicates::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

/// The substring that reintroducing the bug would add back to the message. Its
/// ABSENCE is the core regression guard: the no-server message must never tell a
/// solo user to configure `server_url`.
const REGRESSION_SUBSTR: &str = "server_url";

/// Write a minimal config with NO `server_url` (solo / no-server user).
fn write_no_server_config(dir: &Path) -> PathBuf {
    let db_path = dir.join("index.db");
    let config_path = dir.join("config.toml");
    fs::write(&config_path, format!("db_path = {db_path:?}\n")).expect("write config.toml");
    config_path
}

/// Shared assertion: the no-server message points at the local server and never
/// mentions `server_url`.
fn assert_local_start_no_server_url(stderr: &str) {
    assert!(
        stderr.contains("requires spelunk-server"),
        "must state the feature requires the server; got: {stderr}"
    );
    assert!(
        stderr.contains("spelunk server start"),
        "must point at the local auto-server; got: {stderr}"
    );
    assert!(
        !stderr.contains(REGRESSION_SUBSTR),
        "no-server message must NOT mention `server_url`; got: {stderr}"
    );
}

/// `memory search --mode semantic` with no server: the exact bug report.
#[test]
fn memory_search_semantic_no_server_points_at_local_start() {
    let temp = tempdir().unwrap();
    let config_path = write_no_server_config(temp.path());
    let mem_db = temp.path().join("memory.db");

    let assert = spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(temp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_db)
        .args(["search", "--mode", "semantic", "why did we pick candle"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert_local_start_no_server_url(&stderr);
    assert!(
        stderr.contains("memory search"),
        "message must name the invoked feature; got: {stderr}"
    );
}

/// `memory search --mode hybrid` (the default mode) also embeds the query, so it
/// flows through the same gate and must produce the same guidance.
#[test]
fn memory_search_hybrid_no_server_points_at_local_start() {
    let temp = tempdir().unwrap();
    let config_path = write_no_server_config(temp.path());
    let mem_db = temp.path().join("memory.db");

    let assert = spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(temp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_db)
        .args(["search", "--mode", "hybrid", "anything"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert_local_start_no_server_url(&stderr);
}

/// `memory timeline` is a distinct caller of `require_server_client` (it reaches
/// the gate before opening the memory backend), so cover it independently: a
/// wiring regression there would not be caught by the search tests.
#[test]
fn memory_timeline_no_server_points_at_local_start() {
    let temp = tempdir().unwrap();
    let config_path = write_no_server_config(temp.path());
    let mem_db = temp.path().join("memory.db");

    let assert = spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(temp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_db)
        .args(["timeline", "some topic"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert_local_start_no_server_url(&stderr);
    assert!(
        stderr.contains("memory timeline"),
        "message must name the invoked feature; got: {stderr}"
    );
}

/// `plumbing embed` is the low-level embedding path (a third `require_server_client`
/// caller). It reads stdin; with no server the gate fires before any line is read.
#[test]
fn plumbing_embed_no_server_points_at_local_start() {
    let temp = tempdir().unwrap();
    let config_path = write_no_server_config(temp.path());
    let db = temp.path().join("index.db");

    let assert = spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(temp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("plumbing")
        .arg("--db")
        .arg(&db)
        .arg("embed")
        .write_stdin("some text\n")
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert_local_start_no_server_url(&stderr);
    assert!(
        stderr.contains("plumbing embed"),
        "message must name the invoked feature; got: {stderr}"
    );
}

/// Guard the `predicates` negative-match idiom used elsewhere is not silently
/// vacuous: the positive substring must actually be present (not merely "absent
/// `server_url`" passing because the command produced no output at all).
#[test]
fn memory_search_semantic_message_is_nonempty() {
    let temp = tempdir().unwrap();
    let config_path = write_no_server_config(temp.path());
    let mem_db = temp.path().join("memory.db");

    spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(temp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_db)
        .args(["search", "--mode", "semantic", "q"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("spelunk server start"));
}
