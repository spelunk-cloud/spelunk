use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

mod plumbing_helpers;
use plumbing_helpers::{
    FIXTURE_PROJECT_ID, IndexEmbedResponder, spelunk_bin, spelunk_bin_in, write_config_with_server,
};

#[test]
fn test_help_output() {
    let mut cmd = spelunk_bin();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            // On Windows clap includes the `.exe` extension: "spelunk.exe [OPTIONS]…"
            // Match only the stable prefix so the assertion holds on all platforms.
            "Usage: spelunk",
        ))
        .stdout(predicate::str::contains("Commands:"));
}

/// Guard the help-text corrections from PR fix(cli): correct stale and inaccurate --help text.
///
/// Checks that:
/// - `memory add --kind` lists `antipattern` (was missing before the fix)
/// - `memory harvest --source` lists `failures` (was missing before the fix)
/// - `memory harvest --help` does not contain an `ADR-` internal reference (removed)
/// - `sync --help` says "shorthand" not "alias" (was inaccurate before the fix)
///
/// These assertions are deliberately non-brittle: they check for the *presence* of
/// a corrected token or the *absence* of a stale one, not for exact prose alignment,
/// so ordinary copy edits won't break them.
#[test]
fn test_help_text_accuracy_guards() {
    // `memory add --help` must list `antipattern` as a valid kind.
    spelunk_bin()
        .args(["memory", "add", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("antipattern"));

    // `memory harvest --help` must list `failures` as a valid --source value.
    spelunk_bin()
        .args(["memory", "harvest", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("failures"))
        // Must not embed internal ADR references in user-facing help.
        .stdout(predicate::str::contains("ADR-").not());

    // Top-level `sync --help` must say "shorthand", not "alias"
    // (sync dispatches directly, it is not a clap alias).
    spelunk_bin()
        .args(["sync", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("shorthand"))
        .stdout(predicate::str::contains("alias").not());
}

/// regression: `spelunk search --as-of <sha>` (snapshot search) was removed
/// outright — the flag no longer exists on the top-level `search` command.
/// `spelunk search --help` must not mention `--as-of`.
///
/// This is deliberately scoped to top-level `search --help` only. It must NOT
/// be confused with the unrelated, still-live `--as-of <date>` flag on
/// `memory list` / `memory search` / `memory failures` (point-in-time memory
/// queries, untouched by the snapshot removal) — asserting its absence there
/// would be wrong.
#[test]
fn test_search_help_does_not_list_as_of() {
    spelunk_bin()
        .args(["search", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--as-of").not());

    // Sanity check the disambiguation itself: the sibling `memory search
    // --as-of` flag is untouched and must still be listed, so this test can't
    // pass by accident (e.g. if `--help` output were empty/broken).
    spelunk_bin()
        .args(["memory", "search", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--as-of"));
}

#[test]
fn test_invalid_command() {
    let mut cmd = spelunk_bin();
    cmd.arg("nonexistent-command")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "error: unrecognized subcommand 'nonexistent-command'",
        ));
}

#[test]
fn test_languages_output() {
    let mut cmd = spelunk_bin();
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

    let mut cmd = spelunk_bin();
    cmd.current_dir(temp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        // ADR-067: an un-init'd dir fails closed and reports no project rather
        // than describing the global store.
        .stdout(predicate::str::contains("No spelunk project here"));
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
    let mut cmd = spelunk_bin();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // 2. Check status
    let mut cmd = spelunk_bin();
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
    let mut cmd = spelunk_bin();
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
        spelunk_bin()
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

    let mut cmd = spelunk_bin();
    cmd.env("SPELUNK_NO_SERVER", "1") // ensure offline even if a local server is running
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let mut cmd = spelunk_bin();
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
        // ADR-067 D3: the memory line reflects the resolved backend (sqlite by
        // default), not a tier-derived git-notes label.
        .stdout(predicate::str::contains("sqlite (local)"))
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

    let mut cmd = spelunk_bin();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let mut cmd = spelunk_bin();
    cmd.current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Capability tier:"))
        .stdout(predicate::str::contains("Server"))
        .stdout(predicate::str::contains("semantic"))
        // ADR-067 D3: memory line reflects the resolved backend. With an explicit
        // team server_url the mode is local_first, so the store is local sqlite
        // (converged by `spelunk sync`), not a tier-inferred "server sync" label.
        .stdout(predicate::str::contains("sqlite (local)"));
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

    let mut cmd = spelunk_bin();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let output = spelunk_bin()
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
    // `plan` is a reserved protocol field (ADR-002) with no `spelunk plan`
    // command yet: even though this mock server advertises "plan", it must
    // never surface in user-facing status JSON.
    assert!(body["capabilities"]["plan"].is_null());
    assert!(!body["capabilities"]["explore"].as_bool().unwrap());
    // With an explicit server_url and no `mode` override, the default is
    // local_first even though the tier probe found the server
    // reachable: tier and sync mode are independent axes.
    assert_eq!(body["mode"], "local_first", "got: {body}");
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
    spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1") // ensure offline even if a local server is running
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let output = spelunk_bin()
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
    // mode: additive field (no server_url configured -> resolve_mode() is
    // offline, the same default as pre-existing behaviour).
    assert_eq!(body["mode"], "offline", "got: {body}");
}

/// Locks the top-level key set of `status --format json` so a future change
/// cannot silently rename, drop, or add a field outside the documented
/// "additive extensions only" contract (issue #269 doc comment above
/// `status()`). `mode` (this story) is the newest addition.
#[tokio::test]
async fn test_status_json_top_level_keys_are_exactly_the_documented_set() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = temp.path().join("config.toml");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1\"\nembedding_model = \"test\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let output = spelunk_bin()
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
    let mut got: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    got.sort_unstable();

    let mut want = vec![
        "version",
        "project",
        "db_path",
        "indexed_files",
        "file_count",
        "total_chunks",
        "languages",
        "embedding_dim",
        "has_semantic_search",
        "last_indexed_at",
        "memory_entries",
        "memory_backend",
        "tier",
        "mode",
        "server_url",
        "capabilities",
        "embedder_state",
        "embedding_count",
        "embedding_pending",
        "embed_worker_alive",
        "embed_tokens",
        "drift_candidates",
        "usage_7d",
    ];
    want.sort_unstable();
    assert_eq!(
        got, want,
        "status --format json top-level key set changed; if this is an \
         intentional additive field, add it to `want` here and to the doc \
         comment on `status()`"
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

    let mut cmd = spelunk_bin();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let mut cmd = spelunk_bin();
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

    let mut cmd = spelunk_bin();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let mut cmd = spelunk_bin();
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

    let mut cmd = spelunk_bin();
    cmd.env("SPELUNK_NO_SERVER", "1") // ensure offline even if a local server is running
        .arg("--config")
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

    let mut cmd = spelunk_bin();
    cmd.env("SPELUNK_NO_SERVER", "1") // ensure offline even if a local server is running
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let output = spelunk_bin()
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

// ── Issue #284: search falls back to structural matching when no index / no embedder ───

/// When there is no .spelunk/index.db, `spelunk search` in auto mode must
/// succeed (via the in-process structural fallback) rather than printing an
/// opaque error. Runs on every platform: the fallback is now compiled into the
/// `spelunk` binary (ast-grep-core), so it no longer requires `ast-grep` on PATH.
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
    let mut cmd = spelunk_bin();
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
/// nowhere), `spelunk search` in auto mode must fall back to structural search
/// and succeed, not bail out with a hard error.
/// Runs on every platform: the in-process fallback (ast-grep-core) is compiled
/// into the `spelunk` binary and no longer requires `ast-grep` on PATH.
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
    // SPELUNK_NO_SERVER=1 keeps the embed phase from auto-discovering a
    // loopback spelunk-server on 127.0.0.1:7777.
    spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // Now search in auto mode: embedder is unavailable, so fallback kicks in.
    let mut cmd = spelunk_bin();
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

    spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1") // prevent accidental loopback auto-discovery
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // Explicit --mode hybrid with no server → succeeds with text search silently
    // (ADR-004: inference-only routing; fallback is resolved at capability detection,
    // no per-query notice is emitted).
    spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1") // prevent accidental loopback auto-discovery
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

    spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1") // prevent accidental loopback auto-discovery
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // Explicit --mode semantic with no server configured → silent fallback to
    // text search (ADR-004: no explicit server_url = inference-only routing,
    // fallback notice is suppressed; same as the hybrid test above).
    spelunk_bin()
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
    spelunk_bin_in(tmp.path())
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
    spelunk_bin()
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
    spelunk_bin()
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
    // Use a path that does not exist on any platform. On Windows, an absolute
    // Unix-style path like /tmp/... is interpreted as a relative path and will
    // also not exist, so any clearly non-existent path works here.
    let nonexistent = tmp.path().join("spelunk-server-does-not-exist-xyzzy");
    spelunk_bin()
        .env("HOME", tmp.path())
        .arg("server")
        .arg("start")
        .arg("--bin")
        .arg(&nonexistent)
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
    spelunk_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["init", "--no-index"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "server not running - semantic search skipped",
        ));
}

/// Init a git repo at `dir` with a committer identity so `spelunk init` finds a
/// project root. (spelunk#141 init tests only need the repo, not any commits.)
fn git_init_repo(dir: &std::path::Path) {
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "test@test.com"][..],
        &["config", "user.name", "Test"][..],
    ] {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git setup");
    }
}

/// `spelunk init` must NOT create an uninvited `CLAUDE.md` in the user's repo,
/// and must not claim to have written one.
#[test]
fn test_init_does_not_write_claude_md() {
    let tmp = tempdir().unwrap();
    git_init_repo(tmp.path());

    let config_path = tmp.path().join("config.toml");
    fs::write(&config_path, "").unwrap();

    spelunk_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["init", "--no-index"])
        .assert()
        .success()
        // The uninvited-write log line must be gone.
        .stdout(predicate::str::contains("CLAUDE.md written").not());

    assert!(
        !tmp.path().join("CLAUDE.md").exists(),
        "init must not create a CLAUDE.md in the project root"
    );
}

/// A pre-existing `CLAUDE.md` must be left byte-for-byte untouched — init must
/// never overwrite a user's own file.
#[test]
fn test_init_leaves_existing_claude_md_untouched() {
    let tmp = tempdir().unwrap();
    git_init_repo(tmp.path());

    let claude_md = tmp.path().join("CLAUDE.md");
    let sentinel = b"# my own CLAUDE.md\n\ndo not touch\n";
    fs::write(&claude_md, sentinel).unwrap();

    let config_path = tmp.path().join("config.toml");
    fs::write(&config_path, "").unwrap();

    spelunk_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["init", "--no-index"])
        .assert()
        .success();

    assert_eq!(
        fs::read(&claude_md).unwrap(),
        sentinel,
        "init must not modify a pre-existing CLAUDE.md"
    );
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
///
/// Returns the state dir path so callers can pass it as `SPELUNK_STATE_DIR`
/// to child processes. `dirs::home_dir()` 6.x on Windows calls the Win32
/// `SHGetKnownFolderPath` API (a Registry lookup) instead of reading
/// `USERPROFILE`, so setting `HOME`/`USERPROFILE` in the child env is not
/// enough — `SPELUNK_STATE_DIR` bypasses that entirely.
fn write_server_port_file(home: &std::path::Path, port: u16) -> std::path::PathBuf {
    let state_dir = home.join(".local").join("state").join("spelunk");
    fs::create_dir_all(&state_dir).expect("create state dir");
    fs::write(state_dir.join("server.port"), format!("{port}\n")).expect("write server.port");
    state_dir
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
    let state_dir = write_server_port_file(&home, port_from_uri(&mock_server.uri()));

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
    spelunk_bin()
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
    spelunk_bin()
        .env("HOME", &home)
        .env("SPELUNK_STATE_DIR", &state_dir)
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
    spelunk_bin()
        .env("HOME", &home)
        .env("SPELUNK_STATE_DIR", &state_dir)
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
    spelunk_bin()
        .env("HOME", &home)
        .env("SPELUNK_STATE_DIR", &state_dir)
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
    let state_dir = write_server_port_file(&home, port_from_uri(&mock_server.uri()));

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

    spelunk_bin()
        .env("HOME", &home)
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    spelunk_bin()
        .env("HOME", &home)
        .env("SPELUNK_STATE_DIR", &state_dir)
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

    spelunk_bin()
        .env("HOME", &home)
        .env("SPELUNK_STATE_DIR", &state_dir)
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

// ── init imports git-notes memory into memory.db ─────────────────────────────
//
// During `spelunk init`, after the project memory.db is created, every entry on
// the enclosing repo's `refs/notes/spelunk` that is not already present is
// imported into memory.db (no embeddings). The summary line
// `Memory:  imported N entries from git notes` prints only when N > 0, and a
// re-run imports nothing (dedup by the same content key as `memory reconcile`).

/// Init a git repo at `dir` with a committer identity AND one commit, so
/// `refs/notes/spelunk` can be attached - git notes hang off a commit object,
/// so the no-commit `git_init_repo` helper above is not enough here.
fn git_init_repo_with_commit(dir: &std::path::Path) {
    plumbing_helpers::isolate_git_config();
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "test@test.com"][..],
        &["config", "user.name", "Test"][..],
    ] {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git setup");
    }
    fs::write(dir.join("README.md"), "seed\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(dir)
        .status()
        .expect("git add");
    std::process::Command::new("git")
        .args(["commit", "-q", "--no-gpg-sign", "-m", "seed"])
        .current_dir(dir)
        .status()
        .expect("git commit");
}

/// One JSON-Lines `NoteRecord` as the git-notes backend serializes it. Built as
/// a `serde_json::Value` rather than the (crate-private) `NoteRecord` type so
/// this test needs no library dependency on spelunk-cli.
fn git_note_record_line(id: i64, kind: &str, title: &str, body: &str) -> String {
    serde_json::json!({
        "schema_version": 1,
        "id": id,
        "kind": kind,
        "title": title,
        "body": body,
        "tags": [],
        "linked_files": [],
        // Fixed timestamps → a stable content key, so a re-run dedups exactly.
        "created_at": 1_700_000_000_i64 + id,
        "status": "active",
    })
    .to_string()
}

/// Attach `jsonl` (one or more record lines) to HEAD's `refs/notes/spelunk`.
fn seed_git_notes(dir: &std::path::Path, jsonl: &str) {
    let notes_file = tempfile::NamedTempFile::new().expect("notes tempfile");
    fs::write(notes_file.path(), jsonl).unwrap();
    let status = std::process::Command::new("git")
        .args(["notes", "--ref=spelunk", "add", "-f", "-F"])
        .arg(notes_file.path())
        .args(["--", "HEAD"])
        .current_dir(dir)
        .status()
        .expect("git notes add");
    assert!(status.success(), "seeding git notes must succeed");
}

/// End-to-end: `spelunk init` over a real repo that already has git-notes
/// memory imports those entries, `memory list` surfaces them, the summary line
/// reports the right count, and a second init is a no-op (no re-import, no
/// duplicate rows). Covers the import-on-init and idempotency guarantees.
#[test]
fn test_init_imports_git_notes_memory_and_is_idempotent() {
    let tmp = tempdir().unwrap();
    git_init_repo_with_commit(tmp.path());

    let l1 = git_note_record_line(
        1,
        "decision",
        "Adopt sqlite for memory",
        "portable, no server",
    );
    let l2 = git_note_record_line(
        2,
        "requirement",
        "Notes survive a clone",
        "git-notes travel",
    );
    seed_git_notes(tmp.path(), &format!("{l1}\n{l2}\n"));

    let config_path = tmp.path().join("config.toml");
    fs::write(&config_path, "").unwrap();

    // First init: both pre-existing git-notes entries import, and the summary
    // line reports the exact count.
    spelunk_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["init", "--no-index"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "imported 2 entries from git notes",
        ));

    // `memory list` (default sqlite backend, reads memory.db) surfaces both.
    spelunk_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Adopt sqlite for memory"))
        .stdout(predicate::str::contains("Notes survive a clone"));

    // Second init: everything dedups, so nothing imports and the Memory summary
    // line is suppressed (printed only when N > 0).
    spelunk_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["init", "--no-index"])
        .assert()
        .success()
        .stdout(predicate::str::contains("from git notes").not());

    // The key idempotency guarantee: row count is stable — no duplicate rows.
    let output = spelunk_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "list", "--format", "json", "--limit", "100"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let notes: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("memory list --format json");
    assert_eq!(
        notes.as_array().map(Vec::len),
        Some(2),
        "re-running init must not duplicate imported rows"
    );
}

/// `spelunk init` outside any git repo skips the git-notes import entirely:
/// there is no enclosing repo to read notes from, so no import runs, the Memory
/// summary line is absent, and init still succeeds.
#[test]
fn test_init_without_git_repo_skips_notes_import() {
    let tmp = tempdir().unwrap();
    // Deliberately NOT a git repo — no `.git`, no notes ref.
    let config_path = tmp.path().join("config.toml");
    fs::write(&config_path, "").unwrap();

    spelunk_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["init", "--no-index"])
        .assert()
        .success()
        .stdout(predicate::str::contains("from git notes").not());
}

// ── ADR-070 D3/D4: warmup contract + status honesty (adversarial pass) ────────

/// Build an offline-indexed project (chunks stored, zero embeddings, no
/// recorded worker) under `home`, returning `(project_dir, config_path)`.
/// The index DB lands at `<project_dir>/.spelunk/index.db` - the same path
/// `status`/`search` resolve via the project walk, and the one the embed
/// worker's state files are keyed on.
fn offline_indexed_project(home: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let project_dir = home.join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(
        project_dir.join("lib.rs"),
        "pub fn compute(x: i32) -> i32 { x * 2 }\npub fn helper() -> i32 { 7 }\n",
    )
    .unwrap();
    let config_path = home.join("config.toml");
    fs::write(
        &config_path,
        "api_base_url = \"http://127.0.0.1:19999\"\nembedding_model = \"test\"\nllm_model = \"test\"\n",
    )
    .unwrap();
    spelunk_bin_in(home)
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();
    (project_dir, config_path)
}

/// Path of the embed worker's pid state file for `db_path` under a given
/// state directory, replicating the worker's own keying (blake3 of the
/// canonicalised index path, first 16 hex chars). Deliberately duplicated
/// here: if the writer's keying ever drifts from this, the reader/writer
/// pair drifts too, and this test fails loudly.
fn embed_worker_pid_file_in(
    state_dir: &std::path::Path,
    db_path: &std::path::Path,
) -> std::path::PathBuf {
    let canonical = spelunk_core::utils::canonicalize(db_path);
    let key = blake3::hash(canonical.to_string_lossy().as_bytes())
        .to_hex()
        .to_string();
    state_dir.join(format!("embed-worker-{}.pid", &key[..16]))
}

/// Same as [`embed_worker_pid_file_in`], for the default (no
/// `SPELUNK_STATE_DIR`) state dir derived from `home`.
fn embed_worker_pid_file(home: &std::path::Path, db_path: &std::path::Path) -> std::path::PathBuf {
    embed_worker_pid_file_in(&home.join(".local").join("state").join("spelunk"), db_path)
}

/// ADR-070 D4: the `status --format json` embed-state extensions are additive
/// and truthful. On an offline-built index (pending work, no worker) the new
/// fields must report pending counts, a non-alive worker, and token sums with
/// their own denominators - while the stable #269 schema keys survive intact.
#[test]
fn test_status_json_embed_state_extensions_when_pending() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());

    let output = spelunk_bin_in(home.path())
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["status", "--format", "json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");

    // Stable schema keys must survive the extension (additive-only contract).
    for key in [
        "version",
        "db_path",
        "indexed_files",
        "total_chunks",
        "languages",
        "embedding_dim",
        "has_semantic_search",
        "memory_entries",
        "memory_backend",
        "tier",
        "embedding_count",
    ] {
        assert!(
            body.get(key).is_some(),
            "stable/extension key `{key}` missing from status JSON"
        );
    }

    let total_chunks = body["total_chunks"].as_i64().unwrap();
    assert!(total_chunks > 0, "fixture must produce chunks");
    assert_eq!(body["embedding_count"].as_i64(), Some(0));
    assert_eq!(
        body["embedding_pending"].as_i64(),
        Some(total_chunks),
        "everything is pending on an offline-built index"
    );
    assert_eq!(
        body["embed_worker_alive"].as_bool(),
        Some(false),
        "no recorded worker must read as alive=false, never a guess"
    );
    let tokens = &body["embed_tokens"];
    assert!(
        tokens.is_object(),
        "embed_tokens must be an object: {tokens}"
    );
    let total_tokens = tokens["total_tokens"].as_i64().unwrap();
    let pending_tokens = tokens["pending_tokens"].as_i64().unwrap();
    assert!(total_tokens > 0, "token counts are written at parse time");
    assert_eq!(
        pending_tokens, total_tokens,
        "zero embeddings means every token is pending"
    );
}

/// ADR-070 D4: with pending work and no recorded worker, text `status` says
/// `Embedding incomplete` plus the resume command - never `in progress`, and
/// the deleted hedging parenthetical must not resurface.
#[test]
fn test_status_reports_incomplete_when_no_worker_is_recorded() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());

    spelunk_bin_in(home.path())
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Embedding incomplete"))
        .stdout(predicate::str::contains("resume with `spelunk index .`"))
        .stdout(predicate::str::contains("Embedding in progress").not())
        .stdout(predicate::str::contains("may be running").not());
}

/// ADR-070 D4: a worker that crashed without cleanup leaves a pid file behind;
/// the next `status` must classify the dead pid as not-running (never
/// `in progress`) and remove the stale record so it cannot be re-read later.
#[cfg(unix)]
#[test]
fn test_status_cleans_stale_dead_worker_pid_and_reports_incomplete() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());
    let db_path = project_dir.join(".spelunk").join("index.db");
    assert!(db_path.exists(), "offline index must exist");

    // A pid that was real and is now certainly dead: spawn and reap a child.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = child.id();
    child.wait().unwrap();

    let pid_file = embed_worker_pid_file(home.path(), &db_path);
    fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
    fs::write(&pid_file, format!("{dead_pid}\n")).unwrap();
    fs::write(pid_file.with_extension("baseline"), "0 1000\n").unwrap();

    spelunk_bin_in(home.path())
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Embedding incomplete"))
        .stdout(predicate::str::contains("Embedding in progress").not());

    assert!(
        !pid_file.exists(),
        "a dead worker's stale pid record must be cleaned up on read"
    );
}

/// ADR-070 D4: a pid recycled by an unrelated live process (here: this test
/// process itself - alive, but its command line is not a spelunk index run)
/// must never be reported as a live embed worker, and the foreign record is
/// cleaned up like a dead one.
#[cfg(unix)]
#[test]
fn test_status_foreign_pid_reuse_never_reads_as_live_worker() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());
    let db_path = project_dir.join(".spelunk").join("index.db");

    // This test process is definitely alive, and its `ps` command line (the
    // e2e test binary plus a test-name filter) is not a spelunk index run.
    let foreign_pid = std::process::id();

    let pid_file = embed_worker_pid_file(home.path(), &db_path);
    fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
    fs::write(&pid_file, format!("{foreign_pid}\n")).unwrap();

    spelunk_bin_in(home.path())
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Embedding in progress").not())
        .stdout(predicate::str::contains("Embedding incomplete"));

    assert!(
        !pid_file.exists(),
        "a foreign (recycled) pid record must be cleaned up on read"
    );
}

/// Regression: writer and reader of runtime state must agree on
/// `SPELUNK_STATE_DIR`. `HOME` and `SPELUNK_STATE_DIR` are pointed at two
/// *different* directories; the embed worker's pid file is written only into
/// the override directory (as the writer does once it honours the override),
/// never under `HOME`. `status` - the reader - must resolve the same
/// override to find and clean it up. Before the fix, `status`'s read path
/// (`cli/cmd/embed_worker.rs` -> `cli/cmd/server.rs::spelunk_state_dir()`)
/// ignored `SPELUNK_STATE_DIR` and only ever looked under `HOME`, so a file
/// written to the override would never be found.
#[cfg(unix)]
#[test]
fn test_status_honors_state_dir_override_for_embed_worker_pid() {
    let home = tempfile::TempDir::new().unwrap();
    let state_override = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());
    let db_path = project_dir.join(".spelunk").join("index.db");
    assert!(db_path.exists(), "offline index must exist");

    // A pid that was real and is now certainly dead.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = child.id();
    child.wait().unwrap();

    // Write directly into the override dir - NOT `<home>/.local/state/spelunk`.
    let pid_file = embed_worker_pid_file_in(state_override.path(), &db_path);
    fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
    fs::write(&pid_file, format!("{dead_pid}\n")).unwrap();
    fs::write(pid_file.with_extension("baseline"), "0 1000\n").unwrap();

    // Sanity: nothing was written under the HOME-derived default location.
    let home_pid_file = embed_worker_pid_file(home.path(), &db_path);
    assert!(
        !home_pid_file.exists(),
        "fixture bug: pid file must only exist under the override"
    );

    spelunk_bin_in(home.path())
        .env("SPELUNK_NO_SERVER", "1")
        .env("SPELUNK_STATE_DIR", state_override.path())
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Embedding incomplete"))
        .stdout(predicate::str::contains("Embedding in progress").not());

    assert!(
        !pid_file.exists(),
        "the reader must resolve SPELUNK_STATE_DIR (not HOME) to find and clean up the stale pid record"
    );
}

/// ADR-070 D3, zero-coverage auto cell, end to end: an offline-built index has
/// chunks but no embeddings; `search` in auto mode must fall back to the live
/// search with a stderr notice naming warmup - never a bare `No results
/// found.` over a corpus KNN never saw.
#[test]
fn test_search_auto_zero_coverage_falls_back_with_warmup_notice() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());

    spelunk_bin_in(home.path())
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["search", "compute"])
        .assert()
        .success()
        .stderr(predicate::str::contains("semantic search is warming up"))
        .stderr(predicate::str::contains("0/"))
        .stderr(predicate::str::contains("ast-grep"));
}

/// ADR-070 D3, zero-coverage explicit cell, end to end: with a reachable
/// server but an index whose embeddings have not been built yet, an explicit
/// `--mode semantic` search must be an actionable error naming warmup and the
/// resume command - never `No results found.`.
#[tokio::test]
async fn test_search_explicit_semantic_zero_coverage_is_actionable_error() {
    let mock = MockServer::start().await;
    plumbing_helpers::mount_health(&mock).await;

    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, _config_path) = offline_indexed_project(home.path());

    // Same project, but a config that names the (mock) server, so the search
    // runs at Tier 1 and reaches the coverage gate instead of the Tier-0
    // text fallback.
    let db_ignored = home.path().join("unused.db");
    let server_config =
        write_config_with_server(home.path(), &db_ignored, &mock.uri(), &mock.uri());

    let assert = spelunk_bin_in(home.path())
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&server_config)
        .args(["search", "--mode", "semantic", "compute"])
        .assert()
        .failure();
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stderr.contains("warming up"),
        "error must name warmup, got stderr: {stderr}"
    );
    assert!(
        stderr.contains("spelunk index ."),
        "error must name the resume command, got stderr: {stderr}"
    );
    assert!(
        !stdout.contains("No results found"),
        "never the empty-result claim over an unsearched corpus, got stdout: {stdout}"
    );
}

/// ADR-070 D3, partial-coverage cell, end to end: embed everything, then add a
/// file and re-index offline so coverage is partial. An auto search must emit
/// the one-line stderr warmup notice carrying the coverage AND its
/// front-loaded shape, while `--format json` stdout stays machine-clean.
#[tokio::test]
async fn test_search_auto_partial_coverage_emits_warmup_notice_on_stderr() {
    let mock = MockServer::start().await;
    plumbing_helpers::mount_health(&mock).await;
    plumbing_helpers::mount_index_embed(&mock).await;

    let home = tempfile::TempDir::new().unwrap();
    let project_dir = home.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(
        project_dir.join("lib.rs"),
        "pub fn compute(x: i32) -> i32 { x * 2 }\n",
    )
    .unwrap();
    let db_ignored = home.path().join("unused.db");
    let config_path = write_config_with_server(home.path(), &db_ignored, &mock.uri(), &mock.uri());

    // Pass 1: embed everything via the mock server (full coverage).
    spelunk_bin_in(home.path())
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // Pass 2: add a file and re-index offline - its chunks are stored but not
    // embedded, so coverage drops below 100%.
    fs::write(
        project_dir.join("extra.rs"),
        "pub fn extra_helper() -> i32 { 41 }\npub fn another_helper() -> i32 { 42 }\n",
    )
    .unwrap();
    spelunk_bin_in(home.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // Auto search with no reachable embedder: the partial-coverage warmup
    // notice must land on stderr (percentage + shape + pointer at status),
    // and the JSON on stdout must stay parseable.
    let output = spelunk_bin_in(home.path())
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["search", "compute", "--format", "json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("warmup: searchable"),
        "partial coverage must emit the warmup notice, got stderr: {stderr}"
    );
    assert!(
        stderr.contains("front-loaded by importance and recency"),
        "the notice must name the prefix shape, got stderr: {stderr}"
    );
    assert!(
        stderr.contains("spelunk status"),
        "the notice must be actionable, got stderr: {stderr}"
    );
    let _: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("stdout must stay machine-clean JSON with all notices on stderr");
}
