//! Component tests for `spelunk plumbing embed`.
//!
//! Tests use a `wiremock::MockServer` that responds to
//! `POST /v1/projects/{id}/index/embed` (the spelunk-server endpoint) with
//! a fixed 768-dimensional vector, so no real server is needed.

mod plumbing_helpers;
use plumbing_helpers::{FIXTURE_PROJECT_ID, IndexEmbedResponder, spelunk_bin};

use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Build a config.toml with `embedding_model`, and separately point
// `server_url`/`project_id` at `<dir>/.spelunk/config.toml`: `Config::load`
// only honors those two fields from a project-level config (or env), never
// the global `--config` file. The caller's `Command` must set
// `.current_dir(dir.path())`.
//
// `mode = "cloud_first"` in the global config is required since the
// 2026-07-23 ADR-004 revision: `plumbing embed` has no
// loopback-auto-discovery bridging (unlike `memory add`/`search`/etc, it
// calls `require_server_client` directly on the loaded `Config`, with no
// `effective_config` step), so a bare `server_url` with the default
// `local_first` mode no longer resolves to any inference target at all — by
// design, `local_first` never falls back to `server_url` for inference. This
// test's whole premise (a mocked `server_url` that IS used for embedding, no
// local server involved) is exactly the `cloud_first` case.
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

// ── error path: no server configured ─────────────────────────────────────────

#[test]
fn embed_exits_nonzero_when_no_server_configured() {
    let tmp = TempDir::new().unwrap();
    let config = tmp.path().join("config.toml");
    std::fs::write(&config, "embedding_model = \"test-model\"\n").unwrap();

    spelunk_bin()
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("embed")
        .write_stdin("some text\n")
        .assert()
        .failure();
}
