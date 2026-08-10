//! Component tests for `spelunk plumbing embed`.
//!
//! Tests use a `wiremock::MockServer` that responds to
//! `POST /v1/projects/{id}/index/embed` (the spelunk-server endpoint) with
//! a fixed 768-dimensional vector, so no real server is needed.

mod plumbing_helpers;
use plumbing_helpers::{
    FIXTURE_PROJECT_ID, IndexEmbedResponder, mount_health, mount_index_embed, spelunk_bin,
    spelunk_bin_in,
};

use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Point loopback auto-discovery (`SPELUNK_STATE_DIR`/`server.port`, step 3a of
// `capability::probe`) at `url`, so a mock `MockServer` on a random port stands
// in for a locally-running `spelunk-server` the CLI discovers on its own —
// exactly as `index_embed_tier_routing.rs` does.
fn write_loopback_state(state_dir: &Path, url: &str) {
    std::fs::create_dir_all(state_dir).expect("create state dir");
    let port: u16 = url
        .rsplit(':')
        .next()
        .expect("uri has a port")
        .trim_end_matches('/')
        .parse()
        .expect("uri port is numeric");
    std::fs::write(state_dir.join("server.port"), format!("{port}\n")).expect("write server.port");
}

// Build a `spelunk plumbing embed` command that auto-discovers the loopback
// server via `SPELUNK_STATE_DIR`, with every ambient `SPELUNK_*` var these
// tests isolate scrubbed so a developer/CI shell value can't change which tier
// is probed.
fn embed_loopback_cmd(
    home: &Path,
    project: &Path,
    state_dir: &Path,
    config: &Path,
) -> assert_cmd::Command {
    let mut cmd = spelunk_bin_in(home);
    cmd.current_dir(project)
        .env_remove("SPELUNK_SERVER_URL")
        .env_remove("SPELUNK_MODE")
        .env_remove("SPELUNK_PROJECT_ID")
        .env_remove("SPELUNK_NO_SERVER")
        .env("SPELUNK_STATE_DIR", state_dir)
        .arg("--config")
        .arg(config)
        .arg("plumbing")
        .arg("embed");
    cmd
}

// Build a config.toml with `embedding_model`, and separately point
// `server_url`/`project_id` at `<dir>/.spelunk/config.toml`: `Config::load`
// only honors those two fields from a project-level config (or env), never
// the global `--config` file. The caller's `Command` must set
// `.current_dir(dir.path())`.
//
// `mode = "cloud_first"` in the global config makes the explicit `server_url`
// the inference target: since the 2026-07-23 ADR-004 revision, a bare
// `server_url` under the default `local_first` mode is a memory sync replica
// only and never serves inference. These tests exercise that explicit-remote
// path (a mocked `server_url` that IS used for embedding, no local server
// involved), which is exactly the `cloud_first` case. `plumbing embed` now
// bridges loopback auto-discovery the same way `search`/`memory search` do —
// covered separately by `embed_finds_auto_discovered_loopback_server` below,
// which needs no `server_url` at all.
fn write_server_config(dir: &TempDir, server_uri: &str) -> std::path::PathBuf {
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "embedding_model = \"test-model\"\nmode = \"cloud_first\"\n",
    )
    .unwrap();
    plumbing_helpers::write_project_server_config(dir.path(), server_uri, FIXTURE_PROJECT_ID);
    config
}

// ── exit 0: no stdin piped (empty pipe) ──────────────────────────────────────

#[tokio::test]
async fn embed_exits_0_with_empty_piped_stdin() {
    let mock = MockServer::start().await;
    // Health probe for tier detection.
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["index.embed", "search.semantic"],
        })))
        .mount(&mock)
        .await;

    let tmp = TempDir::new().unwrap();
    let config = write_server_config(&tmp, &mock.uri());

    // Pipe empty stdin — command should succeed (no lines to embed).
    spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("embed")
        .write_stdin("")
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

// ── happy path: single line → one JSON embedding ──────────────────────────────

#[tokio::test]
async fn embed_document_mode_produces_jsonl_vector() {
    let mock = MockServer::start().await;

    // Health probe.
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["index.embed", "search.semantic"],
        })))
        .mount(&mock)
        .await;

    // index/embed endpoint — echoes chunk_ids with constant 768-d vectors.
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock)
        .await;

    let tmp = TempDir::new().unwrap();
    let config = write_server_config(&tmp, &mock.uri());

    let output = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("embed")
        .write_stdin("fn greet(name: &str) -> String { format!(\"Hello, {}!\", name) }\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = plumbing_helpers::parse_jsonl(&output);
    assert_eq!(rows.len(), 1, "one stdin line → one embedding");

    let row = &rows[0];
    // The config.toml written by `write_server_config` sets a *different*
    // `embedding_model` value ("test-model"): the reported model must be the
    // pinned constant regardless, never that config default.
    assert_eq!(
        row.get("model").and_then(|v| v.as_str()),
        Some(spelunk_core::embeddings::MODEL_ID),
        "'model' must report the pinned model id, not a config value"
    );
    assert!(row.get("dimensions").is_some(), "missing 'dimensions'");
    assert!(row.get("vector").is_some(), "missing 'vector'");

    let dims = row["dimensions"].as_u64().unwrap_or(0);
    assert!(dims > 0, "dimensions should be positive");

    let vec_len = row["vector"].as_array().map(|a| a.len()).unwrap_or(0);
    assert_eq!(
        vec_len, dims as usize,
        "vector length must match dimensions"
    );
}

#[tokio::test]
async fn embed_query_mode_produces_jsonl_vector() {
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["index.embed", "search.semantic"],
        })))
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock)
        .await;

    let tmp = TempDir::new().unwrap();
    let config = write_server_config(&tmp, &mock.uri());

    let output = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("embed")
        .arg("--query")
        .write_stdin("how does greet work?\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = plumbing_helpers::parse_jsonl(&output);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].get("vector").is_some(), "missing 'vector'");
}

#[tokio::test]
async fn embed_multiple_lines_produce_multiple_vectors() {
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["index.embed", "search.semantic"],
        })))
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock)
        .await;

    let tmp = TempDir::new().unwrap();
    let config = write_server_config(&tmp, &mock.uri());

    let output = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("embed")
        .write_stdin("first line\nsecond line\nthird line\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = plumbing_helpers::parse_jsonl(&output);
    assert_eq!(rows.len(), 3, "three stdin lines → three embeddings");
}

// ── happy path: auto-discovered loopback server (no server_url configured) ────

// The reported bug: a healthy local `spelunk-server` discovered via loopback
// auto-discovery (`SPELUNK_STATE_DIR`/`server.port`) — with NO explicit
// `server_url` and the default `local_first` mode — must be found by `plumbing
// embed`, exactly as `search --mode semantic` / `memory search` already find
// it. Before the fix, `embed` skipped the capability-tier / `effective_config`
// bridge those commands use and reported `requires spelunk-server` here, even
// while every other server-backed command found the same server.
#[tokio::test]
async fn embed_finds_auto_discovered_loopback_server() {
    let mock = MockServer::start().await;
    mount_health(&mock).await;
    mount_index_embed(&mock).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    // No `.spelunk/config.toml` at all: no server_url, no project_id — pure
    // loopback auto-discovery, the default no-team-server case.
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &mock.uri());

    let config = project.path().join("config.toml");
    std::fs::write(&config, "embedding_model = \"test-model\"\n").unwrap();

    let output = embed_loopback_cmd(home.path(), project.path(), &state_dir, &config)
        .write_stdin("hello world\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = plumbing_helpers::parse_jsonl(&output);
    assert_eq!(rows.len(), 1, "one stdin line → one embedding");
    assert!(rows[0].get("vector").is_some(), "missing 'vector'");
    assert_eq!(
        rows[0].get("model").and_then(|v| v.as_str()),
        Some(spelunk_core::embeddings::MODEL_ID),
    );
}

// The `--query` prefix path must reach the same auto-discovered loopback
// server (it routes through `embed_query_vec`, a distinct code path from the
// document branch).
#[tokio::test]
async fn embed_query_finds_auto_discovered_loopback_server() {
    let mock = MockServer::start().await;
    mount_health(&mock).await;
    mount_index_embed(&mock).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &mock.uri());

    let config = project.path().join("config.toml");
    std::fs::write(&config, "embedding_model = \"test-model\"\n").unwrap();

    let output = embed_loopback_cmd(home.path(), project.path(), &state_dir, &config)
        .arg("--query")
        .write_stdin("how does greet work?\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = plumbing_helpers::parse_jsonl(&output);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].get("vector").is_some(), "missing 'vector'");
}

// ── error path: no server reachable (gate preserved) ─────────────────────────

// The locked-feature gate must survive the fix: with no server reachable
// (here forced with `SPELUNK_NO_SERVER=1` so the result is deterministic
// regardless of any real server on the default loopback port), `plumbing
// embed` still fails with the actionable `requires spelunk-server` error.
#[test]
fn embed_exits_nonzero_when_no_server_configured() {
    let tmp = TempDir::new().unwrap();
    let config = tmp.path().join("config.toml");
    std::fs::write(&config, "embedding_model = \"test-model\"\n").unwrap();

    spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("embed")
        .write_stdin("some text\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("requires spelunk-server"));
}
