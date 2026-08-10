// Integration tests for `spelunk memory add --kind` validation.
//
// The bug: `memory add --kind <anything>` silently accepted any string as the
// kind, printed `Stored [<anything>]`, and exited 0 — so a typo'd kind (e.g.
// `decisions`) stored an entry that no retrieval path (`memory list --kind
// decision`, `spelunk context`, `memory failures`) could ever surface, yet the
// command reported success.
//
// Acceptance covered here:
// - each of the nine canonical kinds is accepted; omitting --kind defaults to note
// - an unknown kind (bogus, and realistic typos) is rejected with a non-zero
//   exit and a message that names the offending value and lists the valid
//   kinds, and stores NO entry.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin;

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// The nine canonical kinds. Mirrors `spelunk_core::storage::NOTE_KINDS` but is
// kept as a literal here so this end-to-end test pins the user-visible contract
// independently of the library constant.
const VALID_KINDS: [&str; 9] = [
    "decision",
    "context",
    "requirement",
    "note",
    "question",
    "answer",
    "handoff",
    "intent",
    "antipattern",
];

// store_in_git_notes = false so no git repo is needed and the only store is the
// SQLite memory.db that `--db` points at.
fn write_config(dir: &Path, mem_db: &Path) -> PathBuf {
    let content = format!(
        "db_path = {:?}\nllm_model = \"x\"\nstore_in_git_notes = false\n",
        mem_db,
    );
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, content).expect("write config.toml");
    cfg
}

// Build `spelunk --config <cfg> memory --db <mem_db> add …`. Callers append the
// `--kind`/`--title`/`--body` args. SPELUNK_NO_SERVER keeps the embed phase
// offline and deterministic (a note is still stored, just without a vector).
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
        .arg("add");
    cmd
}

// Count memory rows in `mem_db`. Returns 0 if the DB doesn't exist yet.
fn row_count(mem_db: &Path) -> i64 {
    if !mem_db.exists() {
        return 0;
    }
    let conn = rusqlite::Connection::open(mem_db).expect("open memory db");
    conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0)
}

// ── each canonical kind is accepted and stored ────────────────────────────────

#[test]
fn each_canonical_kind_is_accepted_and_stored() {
    for kind in VALID_KINDS {
        let tmp = TempDir::new().unwrap();
        let mem_db = tmp.path().join("memory.db");
        let cfg = write_config(tmp.path(), &mem_db);

        memory_add_cmd(tmp.path(), &cfg, &mem_db)
            .arg("--kind")
            .arg(kind)
            .arg("--title")
            .arg("a title")
            .arg("--body")
            .arg("a body")
            .assert()
            .success()
            .stdout(predicate::str::contains(format!("Stored [{kind}]")));

        assert_eq!(
            row_count(&mem_db),
            1,
            "kind {kind} should store exactly one row"
        );
    }
}

// ── omitting --kind still defaults to note ────────────────────────────────────

#[test]
fn omitting_kind_defaults_to_note() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db);

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("a title")
        .arg("--body")
        .arg("a body")
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    assert_eq!(row_count(&mem_db), 1);
}

// ── an unknown kind is rejected, names the value, lists valid kinds, stores 0 ──

#[test]
fn unknown_kind_is_rejected_and_stores_nothing() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db);

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--kind")
        .arg("bogus")
        .arg("--title")
        .arg("a title")
        .arg("--body")
        .arg("a body")
        .assert()
        .failure()
        // Names the offending value …
        .stderr(predicate::str::contains("bogus"))
        // … and lists the valid kinds so the user can correct it.
        .stderr(predicate::str::contains("decision"))
        .stderr(predicate::str::contains("note"))
        .stderr(predicate::str::contains("antipattern"));

    assert_eq!(row_count(&mem_db), 0, "an unknown kind must store no row");
}

// ── realistic typos are rejected (the exact silent-drop the bug caused) ────────

#[test]
fn realistic_typo_kinds_are_rejected_and_store_nothing() {
    // `decisions` (plural) and `desicion` (misspelling) are the exact typos that
    // silently dropped a decision out of every retrieval path before the fix.
    for typo in ["decisions", "desicion"] {
        let tmp = TempDir::new().unwrap();
        let mem_db = tmp.path().join("memory.db");
        let cfg = write_config(tmp.path(), &mem_db);

        memory_add_cmd(tmp.path(), &cfg, &mem_db)
            .arg("--kind")
            .arg(typo)
            .arg("--title")
            .arg("a title")
            .arg("--body")
            .arg("a body")
            .assert()
            .failure()
            .stderr(predicate::str::contains(typo))
            .stderr(predicate::str::contains("decision"));

        assert_eq!(row_count(&mem_db), 0, "typo kind {typo} must store no row");
    }
}
