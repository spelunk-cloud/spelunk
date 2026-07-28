// End-to-end regression tests for `spelunk memory add`'s insert-then-recover
// behavior once `idx_notes_entity_id` has been promoted to UNIQUE (ADR-068's
// fourth amendment, criteria 25-30, 33, 34).
//
// Every existing test proving this behavior (`entity_id_migration.rs`'s
// `add_note_after_promotion_*` tests) drives `MemoryStore::add_note`
// directly, the storage layer, one level below the actual regression QA
// reproduced: "`spelunk memory add` for a second time with identical
// kind/title/body prints 'Error: UNIQUE constraint failed: notes.entity_id'
// and exits 1", run against the *built CLI binary*. Nothing in the existing
// suite drives the real `spelunk` binary through this path end to end, so
// this file closes that gap: it proves the CLI's own output branch
// (criterion 33: "Stored" vs "Already recorded as") and the git-notes
// write-through carrier's behavior on a reuse (criterion 34) against the
// actual process, not just the library call it wraps.

mod plumbing_helpers;
use plumbing_helpers::{init_git_repo, spelunk_bin};

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn write_config(dir: &Path, mem_db: &Path) -> PathBuf {
    let cfg = dir.join("config.toml");
    std::fs::write(
        &cfg,
        format!(
            "db_path = {:?}\nllm_model = \"test-model\"\nstore_in_git_notes = true\n",
            mem_db.display().to_string()
        ),
    )
    .expect("write config.toml");
    cfg
}

fn memory_add_cmd(dir: &Path, cfg: &Path, mem_db: &Path) -> Command {
    let mut cmd = spelunk_bin();
    cmd.current_dir(dir)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(cfg)
        .arg("memory")
        .arg("--db")
        .arg(mem_db)
        .arg("add")
        .arg("--kind")
        .arg("decision");
    cmd
}

fn row_count(mem_db: &Path) -> i64 {
    let conn = Connection::open(mem_db).expect("open memory.db");
    conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap_or(0)
}

fn note_tags(mem_db: &Path, id: i64) -> String {
    let conn = Connection::open(mem_db).expect("open memory.db");
    conn.query_row(
        "SELECT COALESCE(tags, '') FROM notes WHERE id = ?1",
        rusqlite::params![id],
        |r| r.get(0),
    )
    .unwrap_or_default()
}

// Parse every `{"id": ..., ...}` JSONL record out of `git notes --ref=spelunk
// show HEAD`, returning each record's `id` field in file order.
fn git_note_record_ids(dir: &Path) -> Vec<i64> {
    let out = std::process::Command::new("git")
        .args(["notes", "--ref=spelunk", "show", "HEAD"])
        .current_dir(dir)
        .output()
        .expect("git notes show HEAD");
    assert!(
        out.status.success(),
        "git notes show HEAD failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("git note line is not valid JSON: {l:?}: {e}"));
            v["id"].as_i64().expect("record has an id field")
        })
        .collect()
}

// ── Criteria 25/29/30/33: a fresh insert prints "Stored", a colliding second
// insert prints "Already recorded as", and the row count stays at 1 ─────────

#[test]
fn second_identical_add_reuses_the_row_and_prints_already_recorded() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db);

    // First add: a fresh store with zero rows promotes idx_notes_entity_id to
    // UNIQUE on this very `open()` (zero duplicate groups trivially), so the
    // *second* call below hits the promoted index.
    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("dup entry")
        .arg("--body")
        .arg("same content")
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [decision]"));

    assert_eq!(row_count(&mem_db), 1, "first add creates one row");

    // Second add: byte-identical kind/title/body. Pre-fix this hard-crashed
    // with a raw "UNIQUE constraint failed: notes.entity_id" SQLite error and
    // a non-zero exit, reproduced live against the built binary during the
    // original QA review this story fixes.
    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("dup entry")
        .arg("--body")
        .arg("same content")
        .assert()
        .success()
        .stdout(predicate::str::contains("Already recorded as [decision]"))
        .stdout(predicate::str::contains("Stored [decision]").not());

    assert_eq!(
        row_count(&mem_db),
        1,
        "criterion 26/30: a collision must reuse the existing row, not create a second one"
    );
}

// ── Criterion 26: tags supplied on the colliding call merge into the
// existing row (add-wins) rather than being silently dropped ────────────────

#[test]
fn second_identical_add_merges_tags_into_the_existing_row() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db);

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("dup entry")
        .arg("--body")
        .arg("same content")
        .arg("--tags")
        .arg("alpha")
        .assert()
        .success();

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("dup entry")
        .arg("--body")
        .arg("same content")
        .arg("--tags")
        .arg("beta")
        .assert()
        .success()
        .stdout(predicate::str::contains("Already recorded as"));

    let conn = Connection::open(&mem_db).unwrap();
    let id: i64 = conn
        .query_row("SELECT id FROM notes LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        note_tags(&mem_db, id),
        "alpha,beta",
        "criterion 26: tags must union add-wins, neither dropped"
    );
}

// ── Criterion 34: the git-notes write-through carrier is unconditional:
// it appends on a reused row exactly as on a fresh one, using the SAME id ───

#[test]
fn second_identical_add_still_writes_through_to_git_notes_with_the_same_id() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db);

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("dup entry")
        .arg("--body")
        .arg("same content")
        .assert()
        .success();

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("dup entry")
        .arg("--body")
        .arg("same content")
        .assert()
        .success()
        .stdout(predicate::str::contains("Already recorded as"));

    let ids = git_note_record_ids(tmp.path());
    assert_eq!(
        ids.len(),
        2,
        "criterion 34: the carrier must write on BOTH calls, reuse or not, \
         got records: {ids:?}"
    );
    assert_eq!(
        ids[0], ids[1],
        "criterion 34: both records must carry the SAME id, the reused \
         row's, not a fresh one, so a later reader can't see two different \
         ids for what SQLite considers a single entry"
    );

    // SQLite itself agrees there is exactly one row.
    assert_eq!(row_count(&mem_db), 1);
}
