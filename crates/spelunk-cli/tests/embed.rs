//! Component tests for `spelunk plumbing embed`.
//!
//! Tests use a `wiremock::MockServer` that responds to
//! `POST /v1/projects/{id}/index/embed` (the spelunk-server endpoint) with
//! a fixed 768-dimensional vector, so no real server is needed.

mod plumbing_helpers;
use plumbing_helpers::{FIXTURE_PROJECT_ID, IndexEmbedResponder};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a config.toml that points `server_url` at the given mock server URI.
fn write_server_config(dir: &TempDir, server_uri: &str) -> std::path::PathBuf {
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            concat!(
                "server_url = \"{server_uri}\"\n",
                "project_id = \"{project_id}\"\n",
                "embedding_model = \"test-model\"\n",
            ),
            server_uri = server_uri,
            project_id = FIXTURE_PROJECT_ID,
        ),
    )
    .unwrap();
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
    Command::cargo_bin("spelunk")
        .unwrap()
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

    let output = Command::cargo_bin("spelunk")
        .unwrap()
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
    assert!(row.get("model").is_some(), "missing 'model'");
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

    let output = Command::cargo_bin("spelunk")
        .unwrap()
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

    let output = Command::cargo_bin("spelunk")
        .unwrap()
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

    Command::cargo_bin("spelunk")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("embed")
        .write_stdin("some text\n")
        .assert()
        .failure();
}
