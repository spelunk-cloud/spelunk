//! Integration tests proving that porcelain commands and their plumbing
//! equivalents produce consistent results (issue #130 — Unix architecture
//! validation).
//!
//! All tests use `index_fixture_project()` which spins up a wiremock mock
//! embedding server returning identical 768-dim vectors for every request.
//! Because all vectors are equidistant, ordering of KNN results is
//! non-deterministic; tests assert structure and non-emptiness only.

mod plumbing_helpers;
use plumbing_helpers::{index_fixture_project, parse_jsonl, spelunk_bin, spelunk_cmd};

use std::path::Path;

// ── Test 1: search --format jsonl vs embed | knn ────────────────────────────
//
// Both pipelines should return valid JSONL with `chunk_id` fields.
// Ordering is non-deterministic (all mock embeddings are identical), so we
// only assert structural validity and non-emptiness.

#[test]
fn porcelain_search_jsonl_returns_valid_chunk_ids() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    // `spelunk search "test" --db <db> --format jsonl --no-stale-check`
    let output = spelunk_bin()
        .arg("--config")
        .arg(&config_path)
        .arg("search")
        .arg("test")
        .arg("--db")
        .arg(&db_path)
        .arg("--format")
        .arg("jsonl")
        .arg("--no-stale-check")
        .arg("--mode")
        .arg("text") // text mode: no embedding call needed
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&output);
    assert!(
        !rows.is_empty(),
        "spelunk search --format jsonl should return at least one result"
    );
    for row in &rows {
        assert!(
            row.get("chunk_id").is_some(),
            "search result missing 'chunk_id': {row}"
        );
        assert!(
            row.get("file_path").is_some(),
            "search result missing 'file_path': {row}"
        );
        assert!(
            row.get("content").is_some(),
            "search result missing 'content': {row}"
        );
    }
}

#[test]
fn plumbing_knn_returns_valid_chunk_ids() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    // Step 1: embed a query string via `spelunk plumbing embed --query`
    // The mock server returns [0.1f32; 768] for every request.
    // `index_fixture_project` writes `server_url` to `<_tmp>/.spelunk/config.toml`
    // (project-level, since `Config::load` never honors it from `--config`);
    // `.current_dir` must match for this second, separate invocation to see it.
    //
    // `SPELUNK_MODE=cloud_first`: `plumbing embed` has no loopback
    // auto-discovery bridging (2026-07-23 ADR-004 revision), so with the
    // default `local_first` mode a bare `server_url` no longer resolves to
    // any inference target.
    let embed_output = spelunk_bin()
        .current_dir(_tmp.path())
        .env("SPELUNK_MODE", "cloud_first")
        .arg("--config")
        .arg(&config_path)
        .arg("plumbing")
        .arg("embed")
        .arg("--query")
        .write_stdin("test\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    // Step 2: feed the embedding JSON into `spelunk plumbing knn`
    let knn_output = spelunk_cmd(&db_path, &config_path)
        .arg("knn")
        .write_stdin(embed_output.as_slice())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&knn_output);
    assert!(
        !rows.is_empty(),
        "plumbing embed | knn should return at least one result"
    );
    for row in &rows {
        assert!(
            row.get("chunk_id").is_some(),
            "knn result missing 'chunk_id': {row}"
        );
        assert!(
            row.get("file_path").is_some(),
            "knn result missing 'file_path': {row}"
        );
        assert!(
            row.get("content").is_some(),
            "knn result missing 'content': {row}"
        );
        assert!(
            row.get("score").is_some(),
            "knn result missing 'score': {row}"
        );
    }
}

// ── Test 2: status --format json file_count matches ls-files line count ───────

#[test]
fn status_json_file_count_matches_ls_files_count() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    // `spelunk status --format json` — uses db_path from config.
    //
    // Run from the temp dir so the registry won't match any registered project
    // via CWD, forcing resolve_project_and_deps to fall back to cfg.db_path.
    let status_output = spelunk_bin()
        .current_dir(_tmp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let status_text = std::str::from_utf8(&status_output).expect("status output is utf-8");
    let status_json: serde_json::Value =
        serde_json::from_str(status_text).expect("status --format json should emit valid JSON");

    let file_count = status_json["file_count"]
        .as_u64()
        .expect("status JSON must have 'file_count'");

    // `spelunk plumbing ls-files` — counts JSONL lines.
    let ls_output = spelunk_cmd(&db_path, &config_path)
        .arg("ls-files")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let ls_rows = parse_jsonl(&ls_output);
    let ls_count = ls_rows.len() as u64;

    assert_eq!(
        file_count, ls_count,
        "status --format json file_count ({file_count}) should equal ls-files line count ({ls_count})"
    );
}

// ── Test 3: parse-file and cat-chunks content overlap ────────────────────────
//
// `parse-file` parses a source file without touching the DB (live AST walk).
// `cat-chunks` fetches the same file's chunks from the index.
// Since indexing uses the same parse-file logic, at least one chunk content
// from parse-file must appear in cat-chunks output.

#[test]
fn parse_file_content_appears_in_cat_chunks() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let fixture_lib =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/simple-project/src/lib.rs");

    // parse-file: parse lib.rs without DB.
    let parse_output = spelunk_bin()
        .arg("--config")
        .arg(&config_path)
        .arg("plumbing")
        .arg("parse-file")
        .arg(&fixture_lib)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let parse_rows = parse_jsonl(&parse_output);
    assert!(
        !parse_rows.is_empty(),
        "parse-file should produce at least one chunk for lib.rs"
    );

    // cat-chunks: fetch indexed chunks for lib.rs (suffix matching).
    let cat_output = spelunk_cmd(&db_path, &config_path)
        .arg("cat-chunks")
        .arg("src/lib.rs")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let cat_rows = parse_jsonl(&cat_output);
    assert!(
        !cat_rows.is_empty(),
        "cat-chunks should return at least one indexed chunk for lib.rs"
    );

    // Collect all content strings from each command.
    let parse_contents: std::collections::HashSet<String> = parse_rows
        .iter()
        .filter_map(|r| r["content"].as_str().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect();

    let cat_contents: std::collections::HashSet<String> = cat_rows
        .iter()
        .filter_map(|r| r["content"].as_str().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect();

    // At least one chunk content must match exactly between parse-file and
    // cat-chunks, since both use the same AST chunker.
    let overlap: std::collections::HashSet<&String> =
        parse_contents.intersection(&cat_contents).collect();

    assert!(
        !overlap.is_empty(),
        "Expected parse-file and cat-chunks to share at least one chunk content.\n\
         parse-file chunks: {parse_contents:?}\n\
         cat-chunks chunks: {cat_contents:?}"
    );
}
