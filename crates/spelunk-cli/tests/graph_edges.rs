// Component tests for `spelunk plumbing graph-edges`.
//
// Paths here are the paths the index stores, which are relative to the indexed
// project root (`src/main.rs`), not to the fixture directory
// (`simple-project/src/main.rs`). Filtering on the latter matches nothing and
// exits 1, so a test written as "exit 0 or exit 1" over such a path never runs
// its assertions at all.

mod plumbing_helpers;
use plumbing_helpers::{index_fixture_project, parse_jsonl, spelunk_bin, spelunk_cmd};

use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;

fn has_edge(rows: &[Value], source: &str, target: &str, kind: &str) -> bool {
    rows.iter().any(|row| {
        row["source_name"] == *source && row["target_name"] == *target && row["kind"] == *kind
    })
}

fn assert_edge_fields(rows: &[Value]) {
    assert!(!rows.is_empty(), "expected at least one edge");
    for row in rows {
        for field in ["source_file", "source_name", "target_name", "kind", "line"] {
            assert!(row.get(field).is_some(), "missing {field:?}: {row}");
        }
    }
}

// ── happy path: file filter ───────────────────────────────────────────────────

#[test]
fn graph_edges_file_filter_emits_the_files_call_edges() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let result = spelunk_cmd(&db_path, &config_path)
        .arg("graph-edges")
        .arg("--file")
        .arg("src/utils.rs")
        .output()
        .unwrap();

    assert_eq!(
        result.status.code(),
        Some(0),
        "utils.rs has call edges, so an empty result is a regression\nstderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows = parse_jsonl(&result.stdout);
    assert_edge_fields(&rows);
    assert!(
        has_edge(&rows, "sum_slice", "sum", "calls"),
        "expected the `sum_slice -> sum` call edge: {rows:?}"
    );
    assert!(
        rows.iter().all(|row| row["source_file"] == "src/utils.rs"),
        "a --file filter must not leak edges from other files: {rows:?}"
    );
}

#[test]
fn graph_edges_main_file_emits_both_call_and_import_edges() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let result = spelunk_cmd(&db_path, &config_path)
        .arg("graph-edges")
        .arg("--file")
        .arg("src/main.rs")
        .output()
        .unwrap();

    assert_eq!(
        result.status.code(),
        Some(0),
        "main.rs calls `greet` and imports it, so it has edges\nstderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows = parse_jsonl(&result.stdout);
    assert_edge_fields(&rows);
    assert!(
        has_edge(&rows, "main", "greet", "calls"),
        "expected the `main -> greet` call edge: {rows:?}"
    );
    // An import edge has no enclosing symbol, which is why `source_name` is
    // nullable in the JSONL contract rather than always a string.
    let import = rows
        .iter()
        .find(|row| row["kind"] == "imports")
        .unwrap_or_else(|| panic!("expected an import edge: {rows:?}"));
    assert!(
        import["source_name"].is_null(),
        "an import edge is not attributed to a symbol: {import}"
    );
}

// ── symbol filter ─────────────────────────────────────────────────────────────

#[test]
fn graph_edges_symbol_filter_finds_edges_across_files() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let result = spelunk_cmd(&db_path, &config_path)
        .arg("graph-edges")
        .arg("--symbol")
        .arg("greet")
        .output()
        .unwrap();

    assert_eq!(
        result.status.code(),
        Some(0),
        "`greet` is defined in lib.rs and called from main.rs\nstderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows = parse_jsonl(&result.stdout);
    assert_edge_fields(&rows);
    assert!(
        has_edge(&rows, "main", "greet", "calls"),
        "the symbol filter must reach the caller in another file: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row["source_file"] == "src/lib.rs" && row["source_name"] == "greet"),
        "the symbol filter must also reach edges out of the definition: {rows:?}"
    );
}

// ── a path the index does not store ───────────────────────────────────────────

#[test]
fn graph_edges_exits_1_for_a_path_the_index_does_not_store() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    // Stored paths are relative to the indexed root, so a fixture-relative path
    // matches nothing. Pinned because tolerating this exit silently is what
    // made the earlier file-filter tests unfalsifiable.
    spelunk_cmd(&db_path, &config_path)
        .arg("graph-edges")
        .arg("--file")
        .arg("simple-project/src/main.rs")
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty());
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

    spelunk_bin()
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
