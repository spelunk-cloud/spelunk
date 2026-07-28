//! Regression tests for the secret-scanner bypass fix.
//!
//! Covers:
//! - a secret in a doc-comment causes the whole chunk to be dropped, so it
//!   never lands in `chunks.content`, `chunks.metadata`, or the embedding
//!   accumulator;
//! - a secret that only appears in an LLM-generated summary is not persisted
//!   (the summary is replaced with an empty string before it can be embedded);
//! - sensitive filenames are excluded from indexing regardless of case on a
//!   case-preserving filesystem (macOS/Windows).

mod plumbing_helpers;
use plumbing_helpers::{index_project_dir, spelunk_cmd};

use predicates::prelude::*;
use tempfile::TempDir;

/// A syntactically valid AWS secret access key value (fake, for test purposes).
const FAKE_AWS_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY1";

// ── docstring secret → chunk dropped ───────────────────────────────────────────

#[test]
fn docstring_secret_drops_whole_chunk() {
    let tmp = TempDir::new().expect("create temp project dir");
    let src_dir = tmp.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    // The function body itself is clean; the secret lives only in the
    // preceding doc-comment. Before the fix, `store_chunks` only scanned
    // `chunk.content`, so this chunk was indexed, stored (docstring in
    // `metadata`), and embedded.
    let source = format!(
        "/// aws_secret_access_key = \"{FAKE_AWS_SECRET}\"\npub fn clean_fn(x: i32) -> i32 {{\n    x + 1\n}}\n"
    );
    std::fs::write(src_dir.join("lib.rs"), &source).unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"secret-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    let (_tmp_idx, db_path, config_path) = index_project_dir(tmp.path());

    // The chunk store must not contain the dropped chunk at all.
    let output = spelunk_cmd(&db_path, &config_path)
        .arg("cat-chunks")
        .arg("src/lib.rs")
        .assert()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    assert!(
        !text.contains("clean_fn"),
        "chunk with a secret in its docstring must be dropped entirely, got: {text}"
    );
    assert!(
        !text.contains(FAKE_AWS_SECRET),
        "the secret must never appear in cat-chunks output"
    );

    // Directly inspect the DB: no row in `chunks` may contain the secret in
    // either `content` or `metadata` (which holds the docstring JSON), and no
    // row in `embeddings` may exist that used to hold this chunk's vector.
    let conn = rusqlite::Connection::open(&db_path).expect("open db");
    let mut stmt = conn
        .prepare("SELECT content, metadata FROM chunks")
        .unwrap();
    let rows: Vec<(String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    for (content, metadata) in &rows {
        assert!(
            !content.contains(FAKE_AWS_SECRET),
            "secret leaked into chunks.content: {content}"
        );
        if let Some(m) = metadata {
            assert!(
                !m.contains(FAKE_AWS_SECRET),
                "secret leaked into chunks.metadata (docstring): {m}"
            );
        }
    }

    // The chunk store must not have an embeddings row referencing the file at
    // all beyond what's expected — i.e. there is no chunk for this file, so
    // there is nothing in the embedding accumulator for it either.
    let chunk_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chunks c JOIN files f ON c.file_id = f.id WHERE f.path LIKE '%lib.rs'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        chunk_count, 0,
        "the only chunk in this file contained a secret and must have been dropped"
    );
}

// ── summary secret → summary not persisted/embedded ────────────────────────────

/// Build the SSE body `ServerInferenceClient::llm_complete` expects: one
/// `data: {"kind":"token",...}` event carrying the whole payload, followed by
/// a `data: {"kind":"done"}` terminator. See
/// `crates/spelunk-cli/src/server_client.rs`'s `llm_complete` for the parser
/// this must satisfy (event boundary = `\n\n`, `data: ` prefix per line).
fn sse_token_response(content: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\n",
        serde_json::json!({"kind": "token", "content": content}),
        serde_json::json!({"kind": "done"}),
    )
}

/// Exercise the *real* `generate_summaries` wiring end-to-end: run `spelunk
/// index` against a fixture project with `server_url` configured (so
/// summaries are generated, matching `index_fixture_project`'s mock-server
/// convention from `plumbing_helpers.rs`/`e2e_cli.rs`/`embed.rs`), but with the
/// `/llm/complete` endpoint additionally mocked to return a summary containing
/// a secret. This proves the guard in
/// `crates/spelunk-cli/src/cli/cmd/index/summaries.rs` — which is `pub(super)`
/// and otherwise unreachable from this external test file — actually runs and
/// actually strips the secret, rather than re-testing a hand-written
/// reimplementation of the same logic.
#[test]
fn summary_secret_is_not_persisted() {
    let tmp = TempDir::new().expect("create temp project dir");
    let src_dir = tmp.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        src_dir.join("lib.rs"),
        "pub fn clean_fn(x: i32) -> i32 {\n    x + 1\n}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"summary-secret-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    let db_tmp = TempDir::new().expect("create temp db dir");
    let db_path = db_tmp.path().join("spelunk.db");

    let secret_summary = format!("Uses aws_secret_access_key = \"{FAKE_AWS_SECRET}\" internally");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock_server = rt.block_on(async {
        use wiremock::matchers::{method, path, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "version": "test",
                "capabilities": ["memory", "index.embed", "search.semantic", "explore", "plan"],
            })))
            .mount(&server)
            .await;

        // New Tier 1 index/embed — echoes back constant vectors so parsing/
        // embedding succeeds and the summary pass is reached.
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(plumbing_helpers::IndexEmbedResponder)
            .mount(&server)
            .await;

        // `/llm/complete` — the real endpoint `generate_summaries` calls via
        // `ServerLlmAdapter`/`ServerInferenceClient::llm_complete`. Returns a
        // one-chunk JSON array whose summary contains a fake AWS secret, in
        // the exact SSE wire format the client parses.
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/llm/complete$"))
            .respond_with(move |_: &wiremock::Request| {
                let body = serde_json::json!([{"id": 1, "summary": secret_summary}]).to_string();
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_token_response(&body))
            })
            .mount(&server)
            .await;

        server
    });

    let mock_url = mock_server.uri();
    let config_path = plumbing_helpers::write_config_with_server(
        tmp.path(),
        &db_path,
        &mock_url,
        &mock_url,
        tmp.path(),
    );

    // Run the real `spelunk index` (no `--no-summaries`), same as production:
    // parse → embed → summary generation, all through `generate_summaries`.
    //
    // `SPELUNK_MODE=cloud_first`: `generate_summaries` calls
    // `ServerInferenceClient::from_config` directly on the loaded `Config`
    // with no loopback auto-discovery bridging (2026-07-23 ADR-004 revision),
    // so under the default `local_first` mode a bare
    // `server_url` no longer resolves to any inference target.
    plumbing_helpers::spelunk_bin_in(tmp.path())
        .current_dir(tmp.path())
        .env("SPELUNK_MODE", "cloud_first")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg("--db")
        .arg(&db_path)
        .arg(tmp.path())
        .assert()
        .success();

    // Inspect the DB directly, same rigor as `docstring_secret_drops_whole_chunk`:
    // the secret must never have landed in `chunks.summary`.
    let conn = rusqlite::Connection::open(&db_path).expect("open db");
    let mut stmt = conn.prepare("SELECT summary FROM chunks").unwrap();
    let summaries: Vec<Option<String>> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(
        !summaries.is_empty(),
        "expected at least one chunk to have been indexed"
    );
    for s in summaries.iter().flatten() {
        assert!(
            !s.contains(FAKE_AWS_SECRET),
            "secret leaked into chunks.summary via generate_summaries: {s}"
        );
    }
    // The chunk that received the secret-bearing summary must have been
    // stored as "" (matching the guard's substitution), not left NULL/unset —
    // proving the real `contains_secret` → `update_chunk_summary("")` path ran.
    let empty_summary_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks WHERE summary = ''", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        empty_summary_count, 1,
        "expected exactly one chunk with its secret-bearing summary replaced by \"\""
    );
}

// ── case-insensitive exclusion globs ───────────────────────────────────────────

#[test]
fn case_variant_sensitive_filenames_are_excluded() {
    let tmp = TempDir::new().expect("create temp project dir");

    // Uppercase / mixed-case variants of patterns that are already excluded in
    // lowercase form (parse_phase.rs `sensitive_patterns`).
    std::fs::write(tmp.path().join("ID_RSA"), "fake private key material\n").unwrap();
    std::fs::write(tmp.path().join(".ENV"), "SECRET=fake\n").unwrap();
    std::fs::write(
        tmp.path().join("Config.PEM"),
        "-----BEGIN CERTIFICATE-----\n",
    )
    .unwrap();
    // Control: an ordinary source file that must still be indexed.
    std::fs::write(
        tmp.path().join("main.rs"),
        "pub fn main() { println!(\"hi\"); }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"case-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    let (_tmp_idx, db_path, config_path) = index_project_dir(tmp.path());

    let output = spelunk_cmd(&db_path, &config_path)
        .arg("ls-files")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();

    for excluded in ["ID_RSA", ".ENV", "Config.PEM"] {
        assert!(
            !text.to_lowercase().contains(&excluded.to_lowercase()),
            "expected '{excluded}' to be excluded from indexing regardless of case, \
             ls-files output: {text}"
        );
    }
    assert!(
        text.contains("main.rs"),
        "expected the ordinary source file to still be indexed, ls-files output: {text}"
    );
}

/// Sanity check that the exclusion also holds for canonical lowercase names
/// (guards against a regression where `case_insensitive(true)` accidentally
/// disabled the globs entirely instead of making them case-insensitive).
#[test]
fn lowercase_sensitive_filenames_still_excluded() {
    let tmp = TempDir::new().expect("create temp project dir");
    std::fs::write(tmp.path().join("id_rsa"), "fake private key material\n").unwrap();
    std::fs::write(
        tmp.path().join("main.rs"),
        "pub fn main() { println!(\"hi\"); }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"case-fixture-2\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    let (_tmp_idx, db_path, config_path) = index_project_dir(tmp.path());

    spelunk_cmd(&db_path, &config_path)
        .arg("ls-files")
        .assert()
        .success()
        .stdout(predicate::str::contains("id_rsa").not());
}
