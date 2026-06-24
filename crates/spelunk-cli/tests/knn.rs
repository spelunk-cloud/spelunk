//! Component tests for `spelunk plumbing knn`.
//!
//! `knn` reads an embedding vector from stdin (in the JSON format produced
//! by `plumbing embed`) and returns the K nearest neighbours from the index.
//!
//! All tests that exercise real KNN search require an indexed DB with real
//! embeddings — those need the embedding server and are marked `#[ignore]`.
//! Error-path tests (bad JSON on stdin, missing DB) run without a server.

mod plumbing_helpers;
use plumbing_helpers::{index_fixture_project, parse_ndjson, spelunk_cmd};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

// ── error path: malformed JSON on stdin ───────────────────────────────────────

#[test]
fn knn_exits_nonzero_for_bad_json_stdin() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    spelunk_cmd(&db_path, &config_path)
        .arg("knn")
        .write_stdin("not json at all")
        .assert()
        .failure()
        .stderr(predicate::str::contains("parsing stdin as JSON"));
}

#[test]
fn knn_exits_nonzero_for_json_missing_vector_field() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    spelunk_cmd(&db_path, &config_path)
        .arg("knn")
        .write_stdin(r#"{"model":"test","dimensions":3}"#)
        .assert()
        .failure()
        .stderr(predicate::str::contains("vector"));
}

// ── error path: missing DB ────────────────────────────────────────────────────

#[test]
fn knn_exits_nonzero_when_db_missing() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    let db_path = tmp.path().join("nonexistent.db");

    std::fs::write(
        &config_path,
        format!("db_path = {:?}\nllm_model = \"x\"\n", db_path),
    )
    .unwrap();

    Command::cargo_bin("spelunk")
        .unwrap()
        .arg("--config")
        .arg(&config_path)
        .arg("plumbing")
        .arg("--db")
        .arg(&db_path)
        .arg("knn")
        .write_stdin(r#"{"model":"m","dimensions":2,"vector":[0.1,0.2]}"#)
        .assert()
        .failure()
        .stderr(predicate::str::contains("No index found"));
}

// ── dimension mismatch: wrong-dim vector against 768-dim index ────────────────

#[test]
fn knn_exits_1_or_error_for_wrong_dimension_vector() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    // The index uses 768-dim vectors. A 3-dim vector should produce an error
    // or no results (sqlite-vec may reject the dimension mismatch).
    let result = spelunk_cmd(&db_path, &config_path)
        .arg("knn")
        .write_stdin(r#"{"model":"test","dimensions":3,"vector":[0.1,0.2,0.3]}"#)
        .output()
        .unwrap();

    // Either failure (dimension mismatch error) or exit 1 (no results) is acceptable.
    assert!(
        !result.status.success() || result.status.code() == Some(1),
        "expected non-zero exit for wrong dimension; got: {:?}",
        result.status
    );
}

// ── happy path (needs embedding server + indexed embeddings) ──────────────────

#[test]
#[ignore] // requires embedding server at 127.0.0.1:1234 AND an indexed project with real embeddings
fn knn_returns_ndjson_results_for_valid_vector() {
    // Run manually: cargo test knn_returns_ndjson -- --ignored
    // Expected: a spelunk.db in the current directory with 768-dim embeddings.

    let tmp = TempDir::new().unwrap();
    let config = tmp.path().join("config.toml");
    let db_path = std::path::PathBuf::from("spelunk.db");

    std::fs::write(
        &config,
        "llm_model = \"x\"\napi_base_url = \"http://127.0.0.1:1234\"\nembedding_model = \"text-embedding-nomic-embed-text-v1.5\"\n",
    )
    .unwrap();

    // Construct a unit vector of 768 dimensions.
    let vec: Vec<f32> = {
        let mut v = vec![0.0f32; 896];
        v[0] = 1.0;
        v
    };
    let payload = serde_json::json!({
        "model": "test-model",
        "dimensions": 896,
        "vector": vec,
    });

    let output = Command::cargo_bin("spelunk")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("--db")
        .arg(&db_path)
        .arg("knn")
        .write_stdin(payload.to_string())
        .assert()
        .get_output()
        .stdout
        .clone();

    let rows = parse_ndjson(&output);
    for row in &rows {
        assert!(row.get("chunk_id").is_some(), "missing 'chunk_id': {row}");
        assert!(row.get("file_path").is_some(), "missing 'file_path': {row}");
        assert!(row.get("content").is_some(), "missing 'content': {row}");
        assert!(row.get("score").is_some(), "missing 'score': {row}");
        let score = row["score"].as_f64().unwrap_or(-1.0);
        assert!((0.0..=1.0).contains(&score), "score out of range: {score}");
    }
}

#[test]
#[ignore] // requires embedding server at 127.0.0.1:1234
fn knn_lang_filter_restricts_to_language() {
    let tmp = TempDir::new().unwrap();
    let config = tmp.path().join("config.toml");
    let db_path = std::path::PathBuf::from("spelunk.db");

    std::fs::write(
        &config,
        "llm_model = \"x\"\napi_base_url = \"http://127.0.0.1:1234\"\nembedding_model = \"text-embedding-nomic-embed-text-v1.5\"\n",
    )
    .unwrap();

    let vec: Vec<f32> = {
        let mut v = vec![0.0f32; 896];
        v[0] = 1.0;
        v
    };
    let payload = serde_json::json!({
        "model": "test-model",
        "dimensions": 896,
        "vector": vec,
    });

    let output = Command::cargo_bin("spelunk")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("--db")
        .arg(&db_path)
        .arg("knn")
        .arg("--lang")
        .arg("rust")
        .write_stdin(payload.to_string())
        .output()
        .unwrap();

    if output.status.success() {
        let rows = parse_ndjson(&output.stdout);
        for row in &rows {
            assert_eq!(
                row["language"].as_str().unwrap_or(""),
                "rust",
                "--lang rust filter violated: {row}"
            );
        }
    }
    // exit 1 (no results after lang filter) is also acceptable.
}

#[test]
#[ignore] // requires embedding server at 127.0.0.1:1234
fn knn_min_score_filter_respects_threshold() {
    let tmp = TempDir::new().unwrap();
    let config = tmp.path().join("config.toml");
    let db_path = std::path::PathBuf::from("spelunk.db");

    std::fs::write(
        &config,
        "llm_model = \"x\"\napi_base_url = \"http://127.0.0.1:1234\"\nembedding_model = \"text-embedding-nomic-embed-text-v1.5\"\n",
    )
    .unwrap();

    let vec: Vec<f32> = {
        let mut v = vec![0.0f32; 896];
        v[0] = 1.0;
        v
    };
    let payload = serde_json::json!({
        "model": "test-model",
        "dimensions": 896,
        "vector": vec,
    });

    let output = Command::cargo_bin("spelunk")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("--db")
        .arg(&db_path)
        .arg("knn")
        .arg("--min-score")
        .arg("0.99") // very high threshold → likely exit 1
        .write_stdin(payload.to_string())
        .output()
        .unwrap();

    if output.status.success() {
        let rows = parse_ndjson(&output.stdout);
        for row in &rows {
            let score = row["score"].as_f64().unwrap_or(0.0);
            assert!(score >= 0.99, "score {score} below --min-score 0.99");
        }
    }
}
