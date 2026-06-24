//! Component tests for `spelunk plumbing parse-file`.
//!
//! `parse-file` does not require an indexed DB — it just parses a file
//! on disk and emits JSONL chunks.

mod plumbing_helpers;
use plumbing_helpers::parse_jsonl;

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

/// Absolute path to the fixture Rust source file.
fn fixture_main() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/simple-project/src/main.rs")
}

fn fixture_lib() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/simple-project/src/lib.rs")
}

/// Write a minimal config file (parse-file doesn't use DB but the binary
/// requires the global --config path to exist when certain env vars are absent).
fn dummy_config(tmp: &TempDir) -> std::path::PathBuf {
    let cfg = tmp.path().join("config.toml");
    std::fs::write(&cfg, "llm_model = \"x\"\n").unwrap();
    cfg
}

// ── happy path ────────────────────────────────────────────────────────────────

#[test]
fn parse_file_emits_jsonl_for_rust_file() {
    let tmp = TempDir::new().unwrap();
    let config = dummy_config(&tmp);

    let output = Command::cargo_bin("spelunk")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("parse-file")
        .arg(fixture_main())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&output);
    assert!(!rows.is_empty(), "expected at least one parsed chunk");

    // Every row must have the required fields.
    for row in &rows {
        assert!(row.get("kind").is_some(), "missing 'kind': {row}");
        assert!(
            row.get("start_line").is_some(),
            "missing 'start_line': {row}"
        );
        assert!(row.get("end_line").is_some(), "missing 'end_line': {row}");
        assert!(row.get("content").is_some(), "missing 'content': {row}");
        assert!(row.get("language").is_some(), "missing 'language': {row}");
        assert_eq!(
            row["language"].as_str().unwrap(),
            "rust",
            "language should be rust"
        );
    }
}

#[test]
fn parse_file_finds_function_and_struct_chunks() {
    let tmp = TempDir::new().unwrap();
    let config = dummy_config(&tmp);

    let output = Command::cargo_bin("spelunk")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("parse-file")
        .arg(fixture_lib())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&output);

    // lib.rs has functions AND a struct; both should appear.
    let kinds: Vec<&str> = rows.iter().filter_map(|r| r["kind"].as_str()).collect();
    assert!(
        kinds.contains(&"function"),
        "expected a 'function' chunk; got {kinds:?}"
    );
    assert!(
        kinds.contains(&"struct"),
        "expected a 'struct' chunk; got {kinds:?}"
    );
}

// ── no results (exit 1) ───────────────────────────────────────────────────────

#[test]
fn parse_file_exits_1_for_unsupported_file_type() {
    let tmp = TempDir::new().unwrap();
    let config = dummy_config(&tmp);

    // Write a file with an extension spelunk doesn't recognise.
    let unknown = tmp.path().join("file.xyz123");
    std::fs::write(&unknown, "some content").unwrap();

    Command::cargo_bin("spelunk")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("parse-file")
        .arg(&unknown)
        .assert()
        .code(1);
}

// ── error path (exit 2) ───────────────────────────────────────────────────────

#[test]
fn parse_file_exits_nonzero_for_missing_file() {
    let tmp = TempDir::new().unwrap();
    let config = dummy_config(&tmp);

    Command::cargo_bin("spelunk")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("parse-file")
        .arg("/nonexistent/path/file.rs")
        .assert()
        .failure()
        .stderr(predicate::str::contains("reading"));
}

#[test]
fn parse_file_exits_nonzero_missing_argument() {
    let tmp = TempDir::new().unwrap();
    let config = dummy_config(&tmp);

    // Missing required positional argument → clap error.
    Command::cargo_bin("spelunk")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("parse-file")
        .assert()
        .failure()
        .stderr(predicate::str::contains("required"));
}
