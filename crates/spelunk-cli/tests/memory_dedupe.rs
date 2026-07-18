//! Integration tests for `spelunk memory dedupe`.
//!
//! Covers the CLI-facing acceptance criteria (see ADR-068's third amendment):
//! - `--dry-run` reports counts and makes no writes (AC9).
//! - Without `--dry-run`, duplicate groups collapse and row count drops by
//!   exactly `rows_collapsed` (AC10).
//! - Zero duplicate groups: all-zero counts, no writes, either mode (AC22).
//! - `--format json` emits one JSON summary object (AC23).
//!
//! Storage-layer mechanics (survivor selection, tag/file union, archived
//! sticks, supersede adoption/rewrite/self-edge-guard, embedding cleanup,
//! transactional rollback) are covered directly against `MemoryStore` in
//! `spelunk_core::storage::memory::dedupe`; these tests exercise the command
//! surface end to end instead of re-proving that mechanics.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin;

use assert_cmd::Command;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tempfile::TempDir;

fn ensure_sqlite_vec() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        #[allow(clippy::missing_transmute_annotations)]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

/// Write a minimal spelunk config and make `dir` a real project, mirroring
/// `memory_reconcile.rs`'s `write_config`. Returns `(config_path, mem_path)`.
fn write_config(dir: &Path) -> (PathBuf, PathBuf) {
    let spelunk_dir = dir.join(".spelunk");
    std::fs::create_dir_all(&spelunk_dir).expect("create .spelunk");
    let index_db = spelunk_dir.join("index.db");
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "db_path = {:?}\nllm_model = \"test-model\"\n",
            index_db.display().to_string()
        ),
    )
    .expect("write config");
    let mem_path = index_db.with_file_name("memory.db");
    (config_path, mem_path)
}

/// Seed `mem_path` with two rows sharing `{kind, title, body}` (a duplicate
/// entity_id group) via a schema-only connection, bypassing the CLI's own
/// `MemoryStore::open` pipeline so the seed isn't rejected: a fresh,
/// zero-duplicate store would otherwise already have promoted the index to
/// UNIQUE by the time we try to insert the second row.
fn seed_duplicate_group(mem_path: &Path) {
    ensure_sqlite_vec();
    std::fs::create_dir_all(mem_path.parent().unwrap()).expect("create .spelunk dir");
    let conn = Connection::open(mem_path).expect("open memory.db");
    conn.execute_batch(include_str!("../../spelunk-core/migrations/004_memory.sql"))
        .expect("base memory schema");
    conn.execute_batch(
        "ALTER TABLE notes ADD COLUMN status TEXT NOT NULL DEFAULT 'active';
         ALTER TABLE notes ADD COLUMN superseded_by INTEGER REFERENCES notes(id);
         ALTER TABLE notes ADD COLUMN source_ref TEXT;
         ALTER TABLE notes ADD COLUMN valid_at INTEGER;
         ALTER TABLE notes ADD COLUMN invalid_at INTEGER;
         ALTER TABLE notes ADD COLUMN uuid TEXT;
         ALTER TABLE notes ADD COLUMN remote_id TEXT;
         ALTER TABLE notes ADD COLUMN entity_id TEXT;
         CREATE INDEX IF NOT EXISTS idx_notes_entity_id ON notes(entity_id) WHERE entity_id IS NOT NULL;",
    )
    .expect("lifecycle/uuid/entity_id columns");

    for created_at in [1_700_000_001_i64, 1_700_000_002_i64] {
        conn.execute(
            "INSERT INTO notes (kind, title, body, created_at) VALUES ('decision', 'dup', 'body', ?1)",
            rusqlite::params![created_at],
        )
        .expect("seed duplicate row");
    }
}

fn count_memory_notes(mem_path: &Path) -> i64 {
    ensure_sqlite_vec();
    let conn = Connection::open(mem_path).expect("open memory.db");
    conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap_or(0)
}

fn dedupe_cmd(config_path: &Path) -> Command {
    let tmp_dir = config_path
        .parent()
        .expect("config_path must have a parent");
    let mut cmd = spelunk_bin();
    cmd.current_dir(tmp_dir)
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(config_path)
        .arg("memory")
        .arg("dedupe");
    cmd
}

// ── AC9: --dry-run reports counts, makes no writes ───────────────────────────

#[test]
fn dry_run_reports_counts_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let (config_path, mem_path) = write_config(tmp.path());
    seed_duplicate_group(&mem_path);
    let before = count_memory_notes(&mem_path);
    assert_eq!(before, 2, "precondition: two duplicate rows seeded");

    let output = dedupe_cmd(&config_path)
        .arg("--dry-run")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).expect("stdout should be valid JSON");
    assert_eq!(value["duplicate_groups"].as_i64(), Some(1));
    assert_eq!(value["rows_collapsed"].as_i64(), Some(1));

    assert_eq!(
        count_memory_notes(&mem_path),
        before,
        "dry-run must write nothing"
    );
}

// ── AC10: without --dry-run, duplicates collapse and row count drops exactly ─

#[test]
fn real_run_collapses_and_row_count_drops_by_rows_collapsed() {
    let tmp = TempDir::new().unwrap();
    let (config_path, mem_path) = write_config(tmp.path());
    seed_duplicate_group(&mem_path);
    let before = count_memory_notes(&mem_path);

    let output = dedupe_cmd(&config_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).expect("stdout should be valid JSON");
    let rows_collapsed = value["rows_collapsed"].as_i64().expect("rows_collapsed");
    assert_eq!(rows_collapsed, 1);

    assert_eq!(count_memory_notes(&mem_path), before - rows_collapsed);
}

// ── AC22: zero duplicate groups -> all-zero counts, no writes ───────────────

#[test]
fn zero_duplicates_reports_all_zero_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let (config_path, mem_path) = write_config(tmp.path());

    // A single `memory add` seeds one unique row and, on its own `open()`,
    // the empty-store Step B promotes the index: no duplicates ever exist.
    spelunk_bin()
        .current_dir(tmp.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("add")
        .arg("--title")
        .arg("solo")
        .arg("--body")
        .arg("only entry")
        .assert()
        .success();

    let before = count_memory_notes(&mem_path);
    assert_eq!(before, 1);

    let output = dedupe_cmd(&config_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(value["duplicate_groups"].as_i64(), Some(0));
    assert_eq!(value["rows_collapsed"].as_i64(), Some(0));
    assert_eq!(value["tags_merged"].as_i64(), Some(0));
    assert_eq!(value["linked_files_merged"].as_i64(), Some(0));
    assert_eq!(value["supersede_edges_repointed"].as_i64(), Some(0));
    assert_eq!(value["supersede_self_edges_dropped"].as_i64(), Some(0));

    assert_eq!(
        count_memory_notes(&mem_path),
        before,
        "no writes on zero groups"
    );
}

// ── AC23: default/text format emits a human-readable line, not JSON ─────────

#[test]
fn default_format_emits_human_readable_line() {
    let tmp = TempDir::new().unwrap();
    let (config_path, mem_path) = write_config(tmp.path());
    seed_duplicate_group(&mem_path);

    let assert = dedupe_cmd(&config_path).arg("--dry-run").assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.trim().is_empty(),
        "text-format summary goes to stderr, not stdout: {stdout}"
    );
    assert!(
        stderr.contains("dedupe") && stderr.contains("duplicate_groups=1"),
        "expected a human-readable dedupe summary line, got: {stderr}"
    );
}
