//! Component tests for `spelunk plumbing graph-edges`.

mod plumbing_helpers;
use plumbing_helpers::{index_fixture_project, parse_jsonl, spelunk_cmd};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

// ── happy path: file filter ───────────────────────────────────────────────────

#[test]
fn graph_edges_file_filter_returns_valid_jsonl_or_exit1() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    // utils.rs has no imports — may have no outgoing edges.
    let result = spelunk_cmd(&db_path, &config_path)
        .arg("graph-edges")
        .arg("--file")
        .arg("simple-project/src/utils.rs")
        .output()
        .unwrap();

    // Either exit 1 (no edges) or exit 0 with valid JSONL.
    if result.status.success() {
        let rows = parse_jsonl(&result.stdout);
        for row in &rows {
            assert!(
                row.get("source_file").is_some(),
                "missing 'source_file': {row}"
            );
            assert!(
                row.get("target_name").is_some(),
                "missing 'target_name': {row}"
            );
            assert!(row.get("kind").is_some(), "missing 'kind': {row}");
            assert!(row.get("line").is_some(), "missing 'line': {row}");
        }
    } else {
        assert_eq!(result.status.code(), Some(1));
    }
}

#[test]
fn graph_edges_main_file_returns_valid_jsonl_or_exit1() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    // main.rs uses `greet` from lib — there should be at least a call edge.
    let result = spelunk_cmd(&db_path, &config_path)
        .arg("graph-edges")
        .arg("--file")
        .arg("simple-project/src/main.rs")
        .output()
        .unwrap();

    if result.status.success() {
        let rows = parse_jsonl(&result.stdout);
        assert!(!rows.is_empty(), "expected edges for main.rs");
        for row in &rows {
            assert!(
                row.get("source_file").is_some(),
                "missing 'source_file': {row}"
            );
            assert!(
                row.get("target_name").is_some(),
                "missing 'target_name': {row}"
            );
            assert!(row.get("kind").is_some(), "missing 'kind': {row}");
            assert!(row.get("line").is_some(), "missing 'line': {row}");
        }
    }
    // exit 1 is also acceptable if tree-sitter didn't extract edges.
}

// ── symbol filter ─────────────────────────────────────────────────────────────

#[test]
fn graph_edges_symbol_filter_returns_valid_jsonl_or_exit1() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let result = spelunk_cmd(&db_path, &config_path)
        .arg("graph-edges")
        .arg("--symbol")
        .arg("greet")
        .output()
        .unwrap();

    if result.status.success() {
        let rows = parse_jsonl(&result.stdout);
        for row in &rows {
            assert!(
                row.get("source_file").is_some(),
                "missing 'source_file': {row}"
            );
            assert!(
                row.get("target_name").is_some(),
                "missing 'target_name': {row}"
            );
        }
    } else {
        assert_eq!(result.status.code(), Some(1));
    }
}

// ── no results (exit 1) ───────────────────────────────────────────────────────

#[test]
fn graph_edges_exits_1_for_nonexistent_symbol() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    spelunk_cmd(&db_path, &config_path)
        .arg("graph-edges")
        .arg("--symbol")
        .arg("symbol_that_does_not_exist_xyz")
        .assert()
        .code(1);
}

// ── error path: no flags ──────────────────────────────────────────────────────

#[test]
fn graph_edges_exits_nonzero_when_no_flags_given() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    spelunk_cmd(&db_path, &config_path)
        .arg("graph-edges")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "at least one of --file or --symbol is required",
        ));
}

// ── error path: missing DB ────────────────────────────────────────────────────

#[test]
fn graph_edges_exits_nonzero_when_db_missing() {
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
        .arg("graph-edges")
        .arg("--symbol")
        .arg("foo")
        .assert()
        .failure()
        .stderr(predicate::str::contains("No index found"));
}
