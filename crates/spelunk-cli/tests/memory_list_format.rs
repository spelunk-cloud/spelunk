//! Component tests for `spelunk memory list --format` output shapes.
//!
//! Regression coverage for the bug where `--format jsonl` fell through to the
//! colored text summary instead of emitting one JSON object per line.

mod plumbing_helpers;
use plumbing_helpers::{parse_jsonl, write_config};

use assert_cmd::Command;
use tempfile::TempDir;

/// Create a temp project with a single memory note and return
/// `(TempDir, mem_path, config_path)`.  The `TempDir` must be kept alive for
/// the duration of the test.
fn project_with_memory_note() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let mem_path = db_path.with_file_name("memory.db");

    // No server needed for `memory add`/`memory list` on the local backend.
    let config_path = write_config(tmp.path(), &db_path, "http://127.0.0.1:1");

    Command::cargo_bin("spelunk")
        .unwrap()
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_path)
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("jsonl format test note")
        .arg("--body")
        .arg("body content here")
        .assert()
        .success();

    (tmp, mem_path, config_path)
}

/// Build a `spelunk --config <cfg> memory --db <mem> list` Command.
fn memory_list_cmd(mem_path: &std::path::Path, config_path: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--config")
        .arg(config_path)
        .arg("memory")
        .arg("--db")
        .arg(mem_path)
        .arg("list");
    cmd
}

// ── --format jsonl emits one JSON object per line ───────────────────────────────

#[test]
fn memory_list_jsonl_emits_one_object_per_line() {
    let (_tmp, mem_path, config_path) = project_with_memory_note();

    let output = memory_list_cmd(&mem_path, &config_path)
        .arg("--format")
        .arg("jsonl")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    // Every non-empty line must be a standalone JSON object with the note fields.
    let rows = parse_jsonl(&output);
    assert!(
        !rows.is_empty(),
        "expected at least one memory note as JSONL"
    );
    for row in &rows {
        assert!(row.get("id").is_some(), "missing 'id': {row}");
        assert!(row.get("kind").is_some(), "missing 'kind': {row}");
        assert!(row.get("title").is_some(), "missing 'title': {row}");
        assert!(row.get("body").is_some(), "missing 'body': {row}");
    }

    // Must NOT fall back to the colored text summary, which carries ANSI escape
    // codes and is not valid JSON on a per-line basis.
    let text = std::str::from_utf8(&output).expect("stdout is utf-8");
    assert!(
        !text.contains('\u{1b}'),
        "jsonl output must not contain ANSI escapes (text-summary fallback): {text:?}"
    );
}

// ── --format json still emits a single pretty-printed array ─────────────────────

#[test]
fn memory_list_json_emits_pretty_array() {
    let (_tmp, mem_path, config_path) = project_with_memory_note();

    let output = memory_list_cmd(&mem_path, &config_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = std::str::from_utf8(&output).expect("stdout is utf-8");
    let parsed: serde_json::Value =
        serde_json::from_str(text.trim()).expect("json format should be a single JSON document");
    assert!(parsed.is_array(), "json format should be a JSON array");
    assert!(
        !parsed.as_array().unwrap().is_empty(),
        "expected at least one note in the json array"
    );
}
