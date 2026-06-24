use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

mod plumbing_helpers;
use plumbing_helpers::{FIXTURE_PROJECT_ID, IndexEmbedResponder, write_config_with_server};

#[test]
fn test_help_output() {
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Usage: spelunk [OPTIONS] <COMMAND>",
        ))
        .stdout(predicate::str::contains("Commands:"));
}

#[test]
fn test_invalid_command() {
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("nonexistent-command")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "error: unrecognized subcommand 'nonexistent-command'",
        ));
}

#[test]
fn test_languages_output() {
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("languages")
        .assert()
        .success()
        .stdout(predicate::str::contains("Supported languages:"))
        .stdout(predicate::str::contains("rust"))
        .stdout(predicate::str::contains("python"))
        .stdout(predicate::str::contains("javascript"));
}

#[test]
fn test_status_empty_project() {
    let temp = tempdir().unwrap();
    let config_path = temp.path().join("config.toml");
    // Pin db_path to a non-existent temp path so the test is machine-independent.
    let db_path = temp.path().join("nonexistent.db");
    fs::write(
        &config_path,
        format!(
            "llm_model = \"test-model\"\ndb_path = {:?}\n",
            db_path.display().to_string()
        ),
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.current_dir(temp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "No index found for the current directory",
        ));
}

use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_index_and_status() {
    let mock_server = MockServer::start().await;
    let project_id = FIXTURE_PROJECT_ID;

    // Health probe — Tier 1 capability set.
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["memory", "index.embed", "search.semantic", "explore", "plan"],
        })))
        .mount(&mock_server)
        .await;

    // Embedding endpoint — handles the index phase.
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock_server)
        .await;

    // Search endpoint (#322) — returns a fake query vector for CLI-side KNN.
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/search$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "mode": "hybrid",
            "query_vector": vec![0.1f32; 896],
        })))
        .mount(&mock_server)
        .await;

    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("my-project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(
        project_dir.join("main.rs"),
        "fn main() { println!(\"hello\"); }",
    )
    .unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("test_index.db");

    fs::write(
        &config_path,
        format!(
            concat!(
                "db_path = {:?}\n",
                "embedding_model = \"test-model\"\n",
                "llm_model = \"test-chat-model\"\n",
                "server_url = {:?}\n",
                "project_id = {:?}\n",
            ),
            db_path,
            mock_server.uri(),
            project_id,
        ),
    )
    .unwrap();

    // 1. Index the project
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // 2. Check status
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Project:"))
        .stdout(predicate::str::contains("my-project"))
        .stdout(predicate::str::contains("Files:      1"))
        .stdout(predicate::str::contains("Chunks:     1"));

    // 3. Search for the function (semantic search via server embedding)
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("search")
        .arg("hello")
        .assert()
        .success()
        .stdout(predicate::str::contains("main.rs"))
        .stdout(predicate::str::contains("fn main()"));
}

/// Regression test for #349 / qa-v080-test-plan.md §Fix 1 (decision #106).
///
/// `derive_project_id` produces slugs containing `/`:
///   - `local/<blake3-hex>`        — repo with no git remote
///   - `github.com/owner/repo`     — repo with a GitHub remote
///
/// Inserted raw into `/v1/projects/{project_id}/index/embed`, the slashes
/// split the path into extra segments and axum's router 404s. PR #349 added
/// `encode_project_id` to percent-encode the whole slug as a single path
/// segment (`/` → `%2F`) before building the URL. This test locks that fix in
/// for both shapes of project_id by asserting on the *raw* request path the
/// mock server actually received — not just that the CLI exits 0 — so a
/// future change that silently reverts to naive `format!` interpolation would
/// fail here even though the mock still matches via `path_regex`.
#[tokio::test]
async fn test_index_encodes_project_id_with_slashes_as_single_segment() {
    for project_id in [
        // No-remote repo: derive_local_fallback() shape.
        "local/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd",
        // Remote repo: normalise_git_url() shape.
        "github.com/owner/repo",
    ] {
        let mock_server = MockServer::start().await;

        // Health probe — Tier 1 capability set.
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "capabilities": ["memory", "index.embed", "search.semantic", "explore", "plan"],
            })))
            .mount(&mock_server)
            .await;

        // Embedding endpoint — match on ANY `/v1/projects/.../index/embed`
        // shape (including one that's been split into extra segments by an
        // unencoded slash) so a regression produces a clear path-shape
        // assertion failure below rather than an opaque 404 from the CLI.
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.*/index/embed$"))
            .respond_with(IndexEmbedResponder)
            .mount(&mock_server)
            .await;

        let temp = tempdir().unwrap();
        let project_dir = temp.path().join("project");
        fs::create_dir(&project_dir).unwrap();
        fs::write(
            project_dir.join("main.rs"),
            "fn main() { println!(\"hello\"); }",
        )
        .unwrap();

        let config_path = temp.path().join("config.toml");
        let db_path = temp.path().join("test_index.db");

        fs::write(
            &config_path,
            format!(
                concat!(
                    "db_path = {:?}\n",
                    "embedding_model = \"test-model\"\n",
                    "llm_model = \"test-chat-model\"\n",
                    "server_url = {:?}\n",
                    "project_id = {:?}\n",
                ),
                db_path,
                mock_server.uri(),
                project_id,
            ),
        )
        .unwrap();

        // Index the project — must reach the embedding phase without a 404.
        Command::cargo_bin("spelunk")
            .unwrap()
            .arg("--config")
            .arg(&config_path)
            .arg("index")
            .arg(&project_dir)
            .assert()
            .success();

        // Inspect the *raw* request the mock server received: the project_id
        // must occupy exactly one path segment, percent-encoded, with no bare
        // `/` from the slug splitting it into extra segments.
        let received = mock_server.received_requests().await.unwrap();
        let embed_reqs: Vec<_> = received
            .iter()
            .filter(|r| r.url.path().ends_with("/index/embed"))
            .collect();
        assert!(
            !embed_reqs.is_empty(),
            "expected at least one /index/embed request for project_id {project_id:?}, got: {:?}",
            received.iter().map(|r| r.url.path()).collect::<Vec<_>>()
        );

        for req in &embed_reqs {
            let raw_path = req.url.path();
            let segments: Vec<&str> = raw_path.trim_start_matches('/').split('/').collect();

            // `v1`, `projects`, `<encoded project_id>`, `index`, `embed` — five
            // segments. If the slug's `/` were left raw, `local/<hex>` would
            // add one extra segment (six total) and `github.com/owner/repo`
            // would add two (seven total).
            assert_eq!(
                segments.len(),
                5,
                "project_id {project_id:?} produced a path with the wrong \
                 number of segments (slug `/` not percent-encoded?): {raw_path:?}"
            );
            assert_eq!(segments[0], "v1");
            assert_eq!(segments[1], "projects");
            assert_eq!(segments[3], "index");
            assert_eq!(segments[4], "embed");

            let encoded_segment = segments[2];
            assert!(
                !encoded_segment.contains('/'),
                "project_id segment must not contain a raw `/`: {encoded_segment:?}"
            );
            assert!(
                encoded_segment.contains("%2F") || encoded_segment.contains("%2f"),
                "project_id {project_id:?} contains `/` and must be percent-encoded \
                 as a single segment (expected `%2F` in {encoded_segment:?})"
            );

            // Round-trip: percent-decoding the segment must recover the
            // original slug exactly (this is what axum does server-side, and
            // what `projects.slug` persistence relies on — decision #106).
            let decoded = percent_encoding::percent_decode_str(encoded_segment)
                .decode_utf8()
                .expect("encoded project_id segment must decode as utf-8");
            assert_eq!(
                decoded, project_id,
                "decoded project_id segment must round-trip to the original slug"
            );
        }
    }
}

// ── Capability tier E2E tests ────────────────────────────────────────────────

#[tokio::test]
async fn test_status_shows_offline_tier() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("index.db");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1234\"\nembedding_model = \"test\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.env("SPELUNK_NO_SERVER", "1") // ensure offline even if a local server is running
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.env("SPELUNK_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Capability tier:"))
        .stdout(predicate::str::contains("Offline"))
        .stdout(predicate::str::contains("ast-grep + text"))
        .stdout(predicate::str::contains("git-notes (local)"))
        .stdout(predicate::str::contains("set server_url to enable"));
}

#[tokio::test]
async fn test_status_shows_server_tier() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory", "index.embed", "search.semantic", "explore", "plan"]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock_server)
        .await;

    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = write_config_with_server(
        temp.path(),
        &db_path,
        &mock_server.uri(),
        &mock_server.uri(),
    );

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Capability tier:"))
        .stdout(predicate::str::contains("Server"))
        .stdout(predicate::str::contains("semantic"))
        .stdout(predicate::str::contains("server sync"));
}

#[tokio::test]
async fn test_status_json_includes_tier_fields() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory", "index.embed", "search.semantic", "plan"]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock_server)
        .await;

    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(
        project_dir.join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }",
    )
    .unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = write_config_with_server(
        temp.path(),
        &db_path,
        &mock_server.uri(),
        &mock_server.uri(),
    );

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let output = Command::cargo_bin("spelunk")
        .unwrap()
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();

    assert!(output.status.success());
    let body: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    assert_eq!(body["tier"], "server");
    assert!(body["server_url"].is_string());
    assert!(body["capabilities"].is_object());
    assert!(body["capabilities"]["search_semantic"].as_bool().unwrap());
    assert!(body["capabilities"]["index_embed"].as_bool().unwrap());
    assert!(body["capabilities"]["plan"].as_bool().unwrap());
    assert!(!body["capabilities"]["explore"].as_bool().unwrap());
}

/// Validate the *stable* JSON schema introduced by issue #269.
///
/// Asserted top-level keys must be present in every future release; their
/// types must remain stable (additive changes only).
#[tokio::test]
async fn test_status_json_stable_schema() {
    // Offline mode — no server URL configured; embed locally.
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "embedding": vec![0.1f64; 896], "index": 0 }],
            "model": "test-model",
            "object": "list",
            "usage": { "prompt_tokens": 5, "total_tokens": 5 }
        })))
        .mount(&mock_server)
        .await;

    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("myproject");
    fs::create_dir(&project_dir).unwrap();
    fs::write(
        project_dir.join("main.rs"),
        "fn main() { println!(\"hello\"); }",
    )
    .unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = temp.path().join("config.toml");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = {:?}\nembedding_model = \"test-model\"\nllm_model = \"test\"\n",
            db_path,
            mock_server.uri()
        ),
    )
    .unwrap();

    // Index the project so there is data to query.
    Command::cargo_bin("spelunk")
        .unwrap()
        .env("SPELUNK_NO_SERVER", "1") // ensure offline even if a local server is running
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let output = Command::cargo_bin("spelunk")
        .unwrap()
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();

    assert!(output.status.success(), "status --format json failed");
    let body: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("output must be valid JSON");

    // ── Stable schema assertions (issue #269) ────────────────────────────────
    assert!(
        body["version"].is_string(),
        "version must be a string, got: {}",
        body["version"]
    );
    // `project` may be null if the project was not registered via `spelunk init`.
    assert!(
        body["project"].is_string() || body["project"].is_null(),
        "project must be string or null"
    );
    assert!(
        body["db_path"].is_string(),
        "db_path must be a string, got: {}",
        body["db_path"]
    );
    assert_eq!(
        body["indexed_files"].as_i64().unwrap(),
        1,
        "expected 1 indexed file"
    );
    assert!(
        body["total_chunks"].as_i64().unwrap() >= 1,
        "expected at least 1 chunk"
    );
    // languages must be an array; Rust file should appear.
    assert!(body["languages"].is_array(), "languages must be an array");
    let langs = body["languages"].as_array().unwrap();
    assert!(!langs.is_empty(), "languages must not be empty");
    // Each language entry must have name (string) and file_count (integer).
    for lang in langs {
        assert!(lang["name"].is_string(), "language name must be string");
        assert!(
            lang["file_count"].as_i64().is_some(),
            "language file_count must be integer"
        );
    }
    // embedding_dim: must be an integer or null (768 when embeddings are stored,
    // null when the local embedding server is not available in CI/test mode).
    assert!(
        body["embedding_dim"].as_u64().is_some() || body["embedding_dim"].is_null(),
        "embedding_dim must be a positive integer or null, got: {}",
        body["embedding_dim"]
    );
    // has_semantic_search: false in offline mode (no server_url).
    assert_eq!(
        body["has_semantic_search"].as_bool(),
        Some(false),
        "has_semantic_search must be false in offline mode"
    );
    // last_indexed_at: ISO-8601 string when files are indexed.
    assert!(
        body["last_indexed_at"].is_string(),
        "last_indexed_at must be a string after indexing"
    );
    let ts = body["last_indexed_at"].as_str().unwrap();
    assert!(
        ts.contains('T') && ts.ends_with('Z'),
        "last_indexed_at must be ISO-8601 UTC, got: {ts}"
    );
    // memory_entries: integer (0 is valid when no entries exist yet).
    assert!(
        body["memory_entries"].as_i64().is_some(),
        "memory_entries must be an integer"
    );
}

#[tokio::test]
async fn test_check_reports_server_reachable() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory", "search.semantic", "explore"]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock_server)
        .await;

    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = write_config_with_server(
        temp.path(),
        &db_path,
        &mock_server.uri(),
        &mock_server.uri(),
    );

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("check")
        .assert()
        .success()
        .stdout(predicate::str::contains("Server:"))
        .stdout(predicate::str::contains("semantic search"))
        .stdout(predicate::str::contains("explore"));
}

#[tokio::test]
async fn test_check_reports_server_unreachable() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = temp.path().join("config.toml");
    let bad_url = "http://127.0.0.1:19999";
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = {:?}\nembedding_model = \"test\"\nllm_model = \"test\"\nserver_url = {:?}\nproject_id = {:?}\n",
            db_path, bad_url, bad_url, FIXTURE_PROJECT_ID
        ),
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("check")
        .assert()
        .success()
        .stdout(predicate::str::contains("Server:"))
        .stdout(predicate::str::contains("unreachable"));
}

#[tokio::test]
async fn test_index_prints_note_when_no_server_configured() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("index.db");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1234\"\nembedding_model = \"test\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "Skipping summaries (no server_url configured)",
        ));
}

#[test]
fn test_status_json_offline_tier() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("lib.rs"), "pub fn answer() -> i32 { 42 }").unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("index.db");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1234\"\nembedding_model = \"test\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.env("SPELUNK_NO_SERVER", "1") // ensure offline even if a local server is running
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let output = Command::cargo_bin("spelunk")
        .unwrap()
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();

    assert!(output.status.success());
    let body: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    assert_eq!(body["tier"], "offline");
    assert!(body["server_url"].is_null());
    assert!(body["capabilities"].is_null());
}

// ── Issue #284: search falls back to ast-grep when no index / no embedder ───

/// When there is no .spelunk/index.db, `spelunk search` in auto mode must
/// succeed (via ast-grep fallback) rather than printing an opaque error.
#[test]
fn test_search_no_index_falls_back_to_ast_grep_or_clean_message() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    // Write a small Rust file so ast-grep has something to scan.
    fs::write(
        project_dir.join("lib.rs"),
        "pub fn greet(name: &str) -> String { format!(\"hello {name}\") }",
    )
    .unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("nonexistent.db"); // deliberately absent
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1234\"\nembedding_model = \"test\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    // With no index, auto mode must not fail with a hard error.
    // It either returns ast-grep results or a clean "No results found." message.
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    let assert = cmd
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("search")
        .arg("greet") // simple pattern ast-grep can match
        .assert()
        .success();

    // Must not print the old opaque error message.
    assert.stdout(predicate::str::contains("Make sure the index has embeddings").not());
}

/// When the index exists but there is no embedder (api_base_url points
/// nowhere), `spelunk search` in auto mode must fall back to ast-grep and
/// succeed, not bail out with a hard error.
#[test]
fn test_search_index_but_no_embedder_falls_back_to_ast_grep() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(
        project_dir.join("lib.rs"),
        "pub fn compute(x: i32) -> i32 { x * 2 }",
    )
    .unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("index.db");
    // Point at an unreachable endpoint so there's no embedder.
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:19999\"\nembedding_model = \"test\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    // Build the index (offline — no embedder needed for parse phase).
    Command::cargo_bin("spelunk")
        .unwrap()
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // Now search in auto mode: embedder is unavailable, so fallback kicks in.
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    let assert = cmd
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("search")
        .arg("compute")
        .assert()
        .success();

    // Must not print the old opaque error message.
    assert.stdout(predicate::str::contains("Make sure the index has embeddings").not());
}

/// Explicit `--mode hybrid` with no reachable server must fall through to FTS
/// text search with a notice on stderr — not fail (#303-F2 / spelunk#323).
#[test]
fn test_search_explicit_hybrid_no_embedder_falls_back_to_text() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("lib.rs"), "pub fn foo() {}").unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("index.db");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:19999\"\nembedding_model = \"test\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    Command::cargo_bin("spelunk")
        .unwrap()
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // Explicit --mode hybrid with no server → succeeds with text search silently
    // (ADR-004: inference-only routing; fallback is resolved at capability detection,
    // no per-query notice is emitted).
    Command::cargo_bin("spelunk")
        .unwrap()
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("search")
        .arg("--mode")
        .arg("hybrid")
        .arg("foo")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

/// Explicit `--mode semantic` with no reachable server must also fall through.
#[test]
fn test_search_explicit_semantic_no_server_falls_back_to_text() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("lib.rs"), "pub fn foo() {}").unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("index.db");
    fs::write(&config_path, format!("db_path = {:?}\n", db_path)).unwrap();

    Command::cargo_bin("spelunk")
        .unwrap()
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // Explicit --mode semantic with no server configured → silent fallback to
    // text search (ADR-004: no explicit server_url = inference-only routing,
    // fallback notice is suppressed; same as the hybrid test above).
    Command::cargo_bin("spelunk")
        .unwrap()
        .env("SPELUNK_NO_SERVER", "1") // prevent accidental loopback auto-discovery
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("search")
        .arg("--mode")
        .arg("semantic")
        .arg("foo")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

// ── spelunk server error-path tests ──────────────────────────────────────────

/// `spelunk server status` prints "not started" when no pid file exists.
#[test]
fn test_server_status_not_running() {
    let tmp = tempdir().unwrap();
    // Point HOME to an empty tmpdir so no real state files interfere.
    Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", tmp.path())
        .arg("server")
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("not started"));
}

/// `spelunk server logs` exits with an error when no log file exists.
#[test]
fn test_server_logs_missing_file() {
    let tmp = tempdir().unwrap();
    Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", tmp.path())
        .arg("server")
        .arg("logs")
        .assert()
        .failure()
        .stderr(predicate::str::contains("No log file"));
}

/// `spelunk server stop` exits with an error when there is no pid file.
#[test]
fn test_server_stop_not_running() {
    let tmp = tempdir().unwrap();
    Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", tmp.path())
        .arg("server")
        .arg("stop")
        .assert()
        .failure()
        .stderr(predicate::str::contains("server.pid"));
}

/// `spelunk server start --bin <missing-path>` exits with a clear error.
///
/// We use `--bin` with a nonexistent path rather than `PATH=""` because in CI
/// both `spelunk` and `spelunk-server` are built to the same `target/debug/`
/// directory, so the sibling-binary lookup would find the real binary even with
/// an empty PATH.
#[test]
fn test_server_start_binary_not_found() {
    let tmp = tempdir().unwrap();
    Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", tmp.path())
        .arg("server")
        .arg("start")
        .arg("--bin")
        .arg("/tmp/spelunk-server-does-not-exist-xyzzy")
        .assert()
        .failure()
        .stderr(predicate::str::contains("spelunk-server binary not found"));
}

/// `spelunk init` in non-TTY mode (piped stdin) prints the server skip notice
/// when no server is reachable. This covers the CI/hook path from issue #318.
#[test]
fn test_init_non_tty_prints_skip_notice() {
    let tmp = tempdir().unwrap();
    // Initialise a git repo so spelunk init finds a project root.
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(tmp.path())
        .status()
        .expect("git init");
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(tmp.path())
        .status()
        .expect("git config email");
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(tmp.path())
        .status()
        .expect("git config name");

    let config_path = tmp.path().join("config.toml");
    fs::write(&config_path, "").unwrap();

    // stdin is piped (not a TTY) when launched via assert_cmd, so
    // is_terminal() returns false — the non-interactive branch runs.
    Command::cargo_bin("spelunk")
        .unwrap()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["init", "--no-index"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "server not running — semantic search skipped",
        ));
}

// ── memory commands against an auto-discovered (loopback) server ─────────────
//
// ADR-004 (unified memory storage): `.spelunk/memory.db` is the single
// canonical store for every CLI memory read and write. An auto-discovered
// loopback server is an INFERENCE backend only (embeddings + LLM); it is never
// a memory store. So `memory add`, `memory search`, and `memory timeline` all
// resolve to the same local `memory.db`, and the server is consulted only to
// embed the query — never to fetch memory rows.
//
// Historical context: IMP-3 / spelunk#316 / PR #349 first taught these commands
// to honour an auto-discovered server (so they no longer errored "requires
// spelunk-server"), but routed BOTH inference and memory storage to the server
// via a synthesised `server_url`. That produced the split-brain Johan flagged
// on PR #386: a note added (to local `memory.db`) was invisible to
// `memory search` (which read the server's `server.db`). ADR-004 fixes this by
// routing inference via `inference_url` while leaving `server_url` unset for
// auto-discovered servers, so `open_memory_backend` keeps memory local.
//
// These tests reproduce the auto-discovery path end-to-end: NO `server_url` in
// config, `SPELUNK_NO_SERVER` unset, and a mock server reachable on loopback —
// discovered via `~/.local/state/spelunk/server.port` (the same file
// `spelunk server start` writes; see `capability.rs` step 3a). We redirect
// `HOME` to an isolated temp dir and pre-write that port file so the probe
// finds our `wiremock` instance deterministically, without depending on the
// real default port 7777 (which may be occupied — or unoccupied — on the test
// host) and without touching the developer's real `~/.local/state`.
//
// Coverage note: `memory harvest` and `explore` route through the same
// `effective_config` bridging code, but harvesting requires mocking `git log`
// plus a streaming `/llm/complete` SSE extraction round-trip, and `explore`
// requires mocking a multi-step tool-calling `Explorer` loop over
// `/llm/complete` SSE — both disproportionately heavy relative to what's under
// test (the auto-discovery → inference-vs-storage split). Left uncovered here;
// flagged honestly rather than thrashing on heavyweight SSE mocks.

/// Write `<home>/.local/state/spelunk/server.port` so `capability::get_tier`'s
/// loopback auto-discovery (step 3a) finds our mock server deterministically.
/// Mirrors the file `spelunk server start` writes (see `cli/cmd/server.rs`).
fn write_server_port_file(home: &std::path::Path, port: u16) {
    let state_dir = home.join(".local").join("state").join("spelunk");
    fs::create_dir_all(&state_dir).expect("create state dir");
    fs::write(state_dir.join("server.port"), format!("{port}\n")).expect("write server.port");
}

/// Extract the TCP port `wiremock` bound to from its `uri()` (`http://127.0.0.1:<port>`).
fn port_from_uri(uri: &str) -> u16 {
    uri.rsplit(':')
        .next()
        .expect("uri has a port")
        .trim_end_matches('/')
        .parse()
        .expect("uri port is numeric")
}

/// Mount the endpoints an INFERENCE-ONLY auto-discovered server needs:
/// - `GET /v1/health` — capability probe (reports `memory` + `search.semantic`
///   so `effective_config` and the inference client build successfully)
/// - `POST /v1/projects/{id}/index/embed` — query/note embedding (`embed_query`
///   / `try_embed_via_server`); returns a constant 768-dim vector so KNN over
///   the LOCAL store is deterministic.
///
/// Deliberately does NOT mount `POST /v1/projects/{id}/memory/search`. Under
/// ADR-004 an auto-discovered server is never a memory backend, so the CLI must
/// not call it for memory rows. The `expect(0)` guard below turns any such call
/// into a test failure, locking in the inference-vs-storage split.
async fn mount_auto_discovery_inference_endpoints(server: &wiremock::MockServer) {
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, ResponseTemplate};

    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory", "index.embed", "search.semantic", "explore", "plan"]
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(server)
        .await;

    // Guard: the server's memory endpoint must NEVER be hit by an auto-discovered
    // server. If it is, the split-brain has regressed. `expect(0)` fails the test
    // on any matching request when the `MockServer` is dropped.
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/memory/search$"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(server)
        .await;
}

/// ADR-004 round-trip: with a loopback server auto-discovered (no `server_url`
/// in config), a note written by `memory add` is found by `memory search` — and
/// the note's content comes from the LOCAL `memory.db`, not the server. The
/// server is consulted ONLY to embed (it has no `/memory/search` mount, and the
/// `expect(0)` guard fails the test if memory rows are ever requested from it).
///
/// This is the exact split-brain the ADR removes: before ADR-004 the
/// auto-discovered server synthesised a `server_url`, so `memory add` wrote
/// `memory.db` while `memory search` read the server's `server.db` and could not
/// see the note.
#[tokio::test]
async fn test_memory_add_then_search_round_trip_on_local_store_with_auto_discovered_server() {
    let mock_server = MockServer::start().await;
    mount_auto_discovery_inference_endpoints(&mock_server).await;

    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    write_server_port_file(&home, port_from_uri(&mock_server.uri()));

    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    // No `server_url` (and no `project_id`) in config — the defining trait of
    // the auto-discovered path. `api_base_url` is unrelated to capability tier
    // probing; it only configures the (offline) embedding/LLM endpoints.
    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("index.db");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1\"\nembedding_model = \"test\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    // Build a local index so memory commands have a DB to resolve `mem_path`
    // from (offline embedding — SPELUNK_NO_SERVER keeps `index` from probing).
    Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", &home)
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // Add a note via the auto-discovery path. No SPELUNK_NO_SERVER, so the
    // loopback server embeds the note (via /index/embed) while the note text +
    // metadata are written to the LOCAL memory.db.
    Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", &home)
        .env_remove("SPELUNK_NO_SERVER")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "Unified memory storage round-trip",
            "--body",
            "Memory lives in memory.db; the loopback server is inference-only.",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [decision]"));

    // Search for it via the same auto-discovery path. The result must be the
    // locally-stored note — proving add and search share one store. The server
    // only embedded the query; the `/memory/search` guard ensures no memory rows
    // were fetched from the server.
    Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", &home)
        .env_remove("SPELUNK_NO_SERVER")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "search", "unified memory storage"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Unified memory storage round-trip",
        ))
        .stdout(predicate::str::contains("[decision]"));

    // Cross-check: `memory list` (which has always read memory.db) sees the same
    // note. Before ADR-004 `search` and `list` could disagree; now they cannot.
    Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", &home)
        .env_remove("SPELUNK_NO_SERVER")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Unified memory storage round-trip",
        ));
}

/// `memory timeline` against an auto-discovered loopback server returns notes
/// from the LOCAL `memory.db` (the server only embeds the query). Companion to
/// the add→search round-trip above; guards that `timeline` does not regress to
/// reading the server's store.
#[tokio::test]
async fn test_memory_timeline_reads_local_store_with_auto_discovered_server() {
    let mock_server = MockServer::start().await;
    mount_auto_discovery_inference_endpoints(&mock_server).await;

    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    write_server_port_file(&home, port_from_uri(&mock_server.uri()));

    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("index.db");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1\"\nembedding_model = \"test\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", &home)
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", &home)
        .env_remove("SPELUNK_NO_SERVER")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "Loopback server is inference-only",
            "--body",
            "Probe 127.0.0.1 when no server_url is configured; memory stays local.",
        ])
        .assert()
        .success();

    Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", &home)
        .env_remove("SPELUNK_NO_SERVER")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("timeline")
        .arg("loopback server")
        .assert()
        .success()
        .stdout(predicate::str::contains("Timeline: loopback server"))
        .stdout(predicate::str::contains(
            "Loopback server is inference-only",
        ));
}
