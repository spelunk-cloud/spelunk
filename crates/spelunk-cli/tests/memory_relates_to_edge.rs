// Integration tests for `spelunk memory add --relates-to <id>`.
//
// Regression: `--relates-to` was accepted and the entry stored (`Stored
// [...]` printed), but the flag was never wired to the edge API, so NO
// `relates_to` edge was recorded on either side — `memory graph`/`memory
// show` showed no relationship from either entry. `--supersedes` in the same
// command writes its edge correctly; these tests pin that `--relates-to` now
// writes a `relates_to` edge too, visible from BOTH endpoints, while
// archiving neither entry (a relates_to link is non-superseding).
//
// The edge lives in the local SQLite graph (`memory_edges`), so these tests
// use `store_in_git_notes = false` and need no git repo.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

// Minimal config pointing at a fresh memory DB, git-notes carry disabled.
fn write_config(dir: &Path, mem_db: &Path) -> PathBuf {
    let content = format!(
        "db_path = {:?}\nllm_model = \"x\"\nstore_in_git_notes = false\n",
        mem_db,
    );
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, content).expect("write config.toml");
    cfg
}

// `spelunk --config <cfg> memory --db <mem_db> …`
fn memory_cmd(dir: &Path, cfg: &Path, mem_db: &Path) -> Command {
    let mut cmd = spelunk_bin();
    cmd.current_dir(dir)
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(cfg)
        .arg("memory")
        .arg("--db")
        .arg(mem_db);
    cmd
}

// Run `memory add --kind note --title <title> --body … <extra…>`; assert
// success and return the id printed in `Stored [<kind>] #<id>: <title>`.
fn add_note(dir: &Path, cfg: &Path, mem_db: &Path, title: &str, extra: &[&str]) -> i64 {
    let mut cmd = memory_cmd(dir, cfg, mem_db);
    cmd.arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg(title)
        .arg("--body")
        .arg("body text with no secrets");
    for a in extra {
        cmd.arg(a);
    }
    let out = cmd.output().expect("run memory add");
    assert!(
        out.status.success(),
        "memory add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_stored_id(&stdout)
}

// Extract the integer id from a `Stored [note] #<id>: <title>` line.
fn parse_stored_id(stdout: &str) -> i64 {
    let hash = stdout
        .find('#')
        .unwrap_or_else(|| panic!("no id marker in stored output: {stdout:?}"));
    let rest = &stdout[hash + 1..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end]
        .parse()
        .unwrap_or_else(|_| panic!("could not parse id from stored output: {stdout:?}"))
}

// Count notes rows; 0 if the DB doesn't exist yet.
fn note_row_count(mem_db: &Path) -> i64 {
    if !mem_db.exists() {
        return 0;
    }
    let conn = rusqlite::Connection::open(mem_db).expect("open memory db");
    conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0)
}

fn note_status(mem_db: &Path, id: i64) -> String {
    let conn = rusqlite::Connection::open(mem_db).expect("open memory db");
    conn.query_row("SELECT status FROM notes WHERE id = ?1", [id], |r| {
        r.get::<_, String>(0)
    })
    .expect("note status")
}

fn superseded_by(mem_db: &Path, id: i64) -> Option<i64> {
    let conn = rusqlite::Connection::open(mem_db).expect("open memory db");
    conn.query_row("SELECT superseded_by FROM notes WHERE id = ?1", [id], |r| {
        r.get::<_, Option<i64>>(0)
    })
    .expect("superseded_by")
}

fn edge_count(mem_db: &Path, from_id: i64, to_id: i64, kind: &str) -> i64 {
    let conn = rusqlite::Connection::open(mem_db).expect("open memory db");
    conn.query_row(
        "SELECT COUNT(*) FROM memory_edges WHERE from_id = ?1 AND to_id = ?2 AND kind = ?3",
        rusqlite::params![from_id, to_id, kind],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

fn total_edges(mem_db: &Path) -> i64 {
    if !mem_db.exists() {
        return 0;
    }
    let conn = rusqlite::Connection::open(mem_db).expect("open memory db");
    conn.query_row("SELECT COUNT(*) FROM memory_edges", [], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap_or(0)
}

// `memory graph <id> --format json` parsed into a serde_json Value.
fn graph_json(dir: &Path, cfg: &Path, mem_db: &Path, id: i64) -> Value {
    let out = memory_cmd(dir, cfg, mem_db)
        .arg("graph")
        .arg(id.to_string())
        .arg("--format")
        .arg("json")
        .output()
        .expect("run memory graph");
    assert!(
        out.status.success(),
        "memory graph failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("parse memory graph json")
}

fn has_edge(edges: &Value, endpoint_field: &str, other: i64, kind: &str) -> bool {
    edges
        .as_array()
        .map(|arr| {
            arr.iter()
                .any(|e| e[endpoint_field].as_i64() == Some(other) && e["kind"] == kind)
        })
        .unwrap_or(false)
}

// ── (1) relates_to writes a bidirectional edge and archives nothing ───────────

#[test]
fn relates_to_writes_a_bidirectional_edge_and_archives_neither_entry() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db);

    let target = add_note(tmp.path(), &cfg, &mem_db, "Original observation", &[]);
    let linker = add_note(
        tmp.path(),
        &cfg,
        &mem_db,
        "Contradicting observation",
        &["--relates-to", &target.to_string()],
    );

    // Exactly one edge: directed linker -> target, kind relates_to.
    assert_eq!(
        total_edges(&mem_db),
        1,
        "expected exactly one edge after --relates-to"
    );
    assert_eq!(
        edge_count(&mem_db, linker, target, "relates_to"),
        1,
        "expected a relates_to edge #{linker} -> #{target}"
    );

    // Non-superseding: neither entry archived, neither superseded_by set.
    assert_eq!(
        note_status(&mem_db, target),
        "active",
        "target must stay active"
    );
    assert_eq!(
        note_status(&mem_db, linker),
        "active",
        "linker must stay active"
    );
    assert_eq!(superseded_by(&mem_db, target), None);
    assert_eq!(superseded_by(&mem_db, linker), None);

    // Visible from the linker: an outgoing relates_to -> target.
    let from_linker = graph_json(tmp.path(), &cfg, &mem_db, linker);
    assert!(
        has_edge(&from_linker["outgoing"], "to_id", target, "relates_to"),
        "graph from #{linker} must show outgoing relates_to -> #{target}: {from_linker}"
    );

    // Visible from the target: an incoming relates_to from the linker.
    let from_target = graph_json(tmp.path(), &cfg, &mem_db, target);
    assert!(
        has_edge(&from_target["incoming"], "from_id", linker, "relates_to"),
        "graph from #{target} must show incoming relates_to from #{linker}: {from_target}"
    );
}

// ── (2) a missing target is rejected before any write ─────────────────────────

#[test]
fn relates_to_a_missing_target_is_rejected_and_stores_nothing() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db);

    memory_cmd(tmp.path(), &cfg, &mem_db)
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("Dangling link")
        .arg("--body")
        .arg("body text with no secrets")
        .arg("--relates-to")
        .arg("999")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "No memory entry with id 999 to relate to",
        ));

    // The new entry must not be written (no orphan) and no edge created.
    assert_eq!(
        note_row_count(&mem_db),
        0,
        "a rejected --relates-to must not leave an orphaned entry"
    );
    assert_eq!(
        total_edges(&mem_db),
        0,
        "no edge on a rejected --relates-to"
    );
}

// ── (3) contrast guard: --supersedes still archives, --relates-to does not ────

#[test]
fn supersedes_still_archives_while_relates_to_does_not() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db);

    // --supersedes: OLD archived + a supersedes edge NEW -> OLD (unchanged).
    let old = add_note(tmp.path(), &cfg, &mem_db, "Old decision", &[]);
    let new = add_note(
        tmp.path(),
        &cfg,
        &mem_db,
        "New decision",
        &["--supersedes", &old.to_string()],
    );
    assert_eq!(
        note_status(&mem_db, old),
        "archived",
        "--supersedes must archive OLD"
    );
    assert_eq!(superseded_by(&mem_db, old), Some(new));
    assert_eq!(edge_count(&mem_db, new, old, "supersedes"), 1);

    // --relates-to on the same store: no archiving, a relates_to edge, and NOT
    // a supersedes edge.
    let a = add_note(tmp.path(), &cfg, &mem_db, "Note A", &[]);
    let b = add_note(
        tmp.path(),
        &cfg,
        &mem_db,
        "Note B",
        &["--relates-to", &a.to_string()],
    );
    assert_eq!(
        note_status(&mem_db, a),
        "active",
        "--relates-to must NOT archive its target"
    );
    assert_eq!(note_status(&mem_db, b), "active");
    assert_eq!(edge_count(&mem_db, b, a, "relates_to"), 1);
    assert_eq!(
        edge_count(&mem_db, b, a, "supersedes"),
        0,
        "--relates-to must not write a supersedes edge"
    );
}
