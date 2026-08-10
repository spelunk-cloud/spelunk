mod support;

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::Connection;
use serde_json::Value;
use spelunk_export::{dump, export, inventory, source};
use support::{LATEST, add_entry, memory_store_at, unstamped_memory_store_at, wal_memory_store_at};

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn export_to(store: &Path, out: &Path) -> spelunk_export::ExportOutcome {
    export(store, out, 1_700_000_000).unwrap()
}

fn records(out: &Path) -> Vec<Value> {
    std::fs::read_to_string(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn entities<'a>(recs: &'a [Value], kind: &str) -> Vec<&'a Value> {
    recs.iter()
        .filter(|r| r["record"] == "entity" && r["type"] == kind)
        .collect()
}

fn relationships<'a>(recs: &'a [Value], kind: &str) -> Vec<&'a Value> {
    recs.iter()
        .filter(|r| r["record"] == "relationship" && r["type"] == kind)
        .collect()
}

// ── E1, E2: every historic shape reads ────────────────────────────────────────

#[test]
fn exports_from_every_schema_version() {
    for version in 1..=LATEST {
        let dir = tmp();
        let store = dir.path().join("memory.db");
        let out = dir.path().join("dump.jsonl");
        {
            let conn = memory_store_at(&store, version);
            add_entry(&conn, version, "first", 1_000);
            add_entry(&conn, version, "second", 2_000);
        }
        let outcome = export_to(&store, &out);
        assert_eq!(
            outcome.counts.entity.get("memory_entry"),
            Some(&2),
            "schema version {version} did not export both entries"
        );
        let recs = records(&out);
        assert_eq!(recs.first().unwrap()["record"], "header");
        assert_eq!(recs.last().unwrap()["record"], "footer");
    }
}

#[test]
fn exports_from_a_store_that_never_had_its_version_stamped() {
    for version in 1..=LATEST {
        let dir = tmp();
        let store = dir.path().join("memory.db");
        let out = dir.path().join("dump.jsonl");
        {
            let conn = unstamped_memory_store_at(&store, version);
            assert_eq!(
                source::schema_version(&conn).unwrap(),
                0,
                "fixture should be unstamped"
            );
            add_entry(&conn, version, "only", 1_000);
        }
        let outcome = export_to(&store, &out);
        assert_eq!(outcome.counts.entity.get("memory_entry"), Some(&1));
    }
}

// ── E3: absent columns are omitted, not nulled ───────────────────────────────

#[test]
fn columns_absent_at_the_source_version_are_omitted_not_nulled() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = memory_store_at(&store, 1);
        add_entry(&conn, 1, "only", 1_000);
    }
    export_to(&store, &out);
    let recs = records(&out);
    let entry = entities(&recs, "memory_entry")[0];
    for absent in [
        "status",
        "source_ref",
        "valid_at",
        "invalid_at",
        "uuid",
        "remote_id",
        "entity_id",
    ] {
        assert!(
            entry.get(absent).is_none(),
            "{absent} should be absent, not null, at schema version 1"
        );
    }
}

// ── E4: no edge table at the source version ──────────────────────────────────

#[test]
fn a_store_without_an_edge_table_yields_no_relationships() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = memory_store_at(&store, 5);
        add_entry(&conn, 5, "only", 1_000);
    }
    let outcome = export_to(&store, &out);
    assert!(outcome.counts.relationship.is_empty());
    assert!(outcome.warnings.is_empty());
}

// ── E5, E6, E7: every edge kind survives ─────────────────────────────────────

#[test]
fn every_edge_kind_survives() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = memory_store_at(&store, LATEST);
        let a = add_entry(&conn, LATEST, "a", 1_000);
        let b = add_entry(&conn, LATEST, "b", 2_000);
        let c = add_entry(&conn, LATEST, "c", 3_000);
        edge(&conn, b, a, "supersedes");
        edge(&conn, a, c, "relates_to");
        edge(&conn, c, b, "contradicts");
    }
    let outcome = export_to(&store, &out);
    for kind in ["supersedes", "relates_to", "contradicts"] {
        assert_eq!(
            outcome.counts.relationship.get(kind),
            Some(&1),
            "{kind} did not survive"
        );
    }
}

// ── E8, E9: the two supersede encodings agree ────────────────────────────────

#[test]
fn the_lifecycle_column_is_emitted_in_the_edge_orientation() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    let (successor, predecessor);
    {
        let conn = memory_store_at(&store, LATEST);
        predecessor = add_entry(&conn, LATEST, "old", 1_000);
        successor = add_entry(&conn, LATEST, "new", 2_000);
        conn.execute(
            "UPDATE notes SET status='archived', superseded_by=?2 WHERE id=?1",
            rusqlite::params![predecessor, successor],
        )
        .unwrap();
    }
    export_to(&store, &out);
    let recs = records(&out);
    let rels = relationships(&recs, "supersedes");
    assert_eq!(rels.len(), 1);
    assert_eq!(
        rels[0]["from"],
        format!("e{successor}"),
        "the successor must be the 'from' endpoint"
    );
    assert_eq!(rels[0]["to"], format!("e{predecessor}"));
}

#[test]
fn a_store_holding_both_supersede_encodings_yields_one_link() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = memory_store_at(&store, LATEST);
        let predecessor = add_entry(&conn, LATEST, "old", 1_000);
        let successor = add_entry(&conn, LATEST, "new", 2_000);
        conn.execute(
            "UPDATE notes SET status='archived', superseded_by=?2 WHERE id=?1",
            rusqlite::params![predecessor, successor],
        )
        .unwrap();
        edge(&conn, successor, predecessor, "supersedes");
    }
    let outcome = export_to(&store, &out);
    assert_eq!(
        outcome.counts.relationship.get("supersedes"),
        Some(&1),
        "the column and the edge encode one fact and must not become two links"
    );
}

// ── E10, E11: identity is carried, never minted ──────────────────────────────

#[test]
fn an_existing_identifier_is_carried_verbatim() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = memory_store_at(&store, LATEST);
        let id = add_entry(&conn, LATEST, "only", 1_000);
        conn.execute(
            "UPDATE notes SET uuid='0192f0a0-dead-7000-8000-000000000001' WHERE id=?1",
            rusqlite::params![id],
        )
        .unwrap();
    }
    export_to(&store, &out);
    let recs = records(&out);
    assert_eq!(
        entities(&recs, "memory_entry")[0]["uuid"],
        "0192f0a0-dead-7000-8000-000000000001"
    );
}

#[test]
fn a_missing_identifier_is_omitted_and_never_minted() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = memory_store_at(&store, LATEST);
        add_entry(&conn, LATEST, "only", 1_000);
        conn.execute_batch("UPDATE notes SET uuid = NULL").unwrap();
    }
    export_to(&store, &out);
    let recs = records(&out);
    let entry = entities(&recs, "memory_entry")[0];
    assert!(
        entry.get("uuid").is_none(),
        "this tool must never mint an identifier: identity policy belongs to the reader"
    );
    assert!(
        entry.get("created_at").is_some(),
        "creation time must always be carried, so a reader can seed from it"
    );
}

// ── E12, E13, E14: verbatim fields ───────────────────────────────────────────

#[test]
fn provenance_content_identity_and_temporal_fields_are_carried_verbatim() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = memory_store_at(&store, LATEST);
        let id = add_entry(&conn, LATEST, "only", 1_000);
        conn.execute(
            "UPDATE notes SET source_ref=?2, entity_id=?3, valid_at=?4, invalid_at=?5
             WHERE id=?1",
            rusqlite::params![id, "a".repeat(40), "content-hash-1", 1_111, 2_222],
        )
        .unwrap();
    }
    export_to(&store, &out);
    let recs = records(&out);
    let entry = entities(&recs, "memory_entry")[0];
    assert_eq!(entry["source_ref"], "a".repeat(40));
    assert_eq!(entry["entity_id"], "content-hash-1");
    assert_eq!(entry["valid_at"], 1_111);
    assert_eq!(entry["invalid_at"], 2_222);
}

// ── E15, E16: list normalisation ─────────────────────────────────────────────

#[test]
fn comma_joined_lists_become_arrays() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = memory_store_at(&store, LATEST);
        add_entry(&conn, LATEST, "with lists", 1_000);
        let id = add_entry(&conn, LATEST, "empty lists", 2_000);
        conn.execute(
            "UPDATE notes SET tags='', linked_files=NULL WHERE id=?1",
            rusqlite::params![id],
        )
        .unwrap();
        let single = add_entry(&conn, LATEST, "one tag", 3_000);
        conn.execute(
            "UPDATE notes SET tags='solo' WHERE id=?1",
            rusqlite::params![single],
        )
        .unwrap();
    }
    export_to(&store, &out);
    let recs = records(&out);
    let by_title: BTreeMap<_, _> = entities(&recs, "memory_entry")
        .into_iter()
        .map(|e| (e["title"].as_str().unwrap().to_string(), e))
        .collect();

    assert_eq!(
        by_title["with lists"]["tags"],
        serde_json::json!(["alpha", "beta"])
    );
    assert_eq!(
        by_title["with lists"]["linked_files"],
        serde_json::json!(["src/a.rs"])
    );
    assert!(by_title["empty lists"].get("tags").is_none());
    assert!(by_title["empty lists"].get("linked_files").is_none());
    assert_eq!(by_title["one tag"]["tags"], serde_json::json!(["solo"]));
}

#[test]
fn a_value_containing_a_separator_splits_as_the_source_would() {
    // The stored form cannot distinguish one value containing a comma from two
    // values. The format inherits that, deliberately: resolving it would mean
    // inventing information the store never held.
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = memory_store_at(&store, LATEST);
        let id = add_entry(&conn, LATEST, "only", 1_000);
        conn.execute(
            "UPDATE notes SET tags='one,two' WHERE id=?1",
            rusqlite::params![id],
        )
        .unwrap();
    }
    export_to(&store, &out);
    let recs = records(&out);
    assert_eq!(
        entities(&recs, "memory_entry")[0]["tags"],
        serde_json::json!(["one", "two"])
    );
}

// ── E17 to E20: the other stores ─────────────────────────────────────────────

#[test]
fn accumulated_command_counts_export() {
    let dir = tmp();
    let store = dir.path().join("index.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = Connection::open(&store).unwrap();
        conn.execute_batch(
            "CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT);
             CREATE TABLE usage (command TEXT NOT NULL, called_at INTEGER NOT NULL);
             INSERT INTO usage VALUES ('search', 100), ('memory add', 200);",
        )
        .unwrap();
    }
    let outcome = export_to(&store, &out);
    assert_eq!(outcome.counts.entity.get("command_usage"), Some(&2));
    let recs = records(&out);
    let first = entities(&recs, "command_usage")[0];
    assert_eq!(first["command"], "search");
    assert_eq!(first["at"], 100);
}

#[test]
fn registered_projects_and_their_dependencies_export() {
    let dir = tmp();
    let store = dir.path().join("registry.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = Connection::open(&store).unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 root_path TEXT NOT NULL UNIQUE,
                 db_path TEXT NOT NULL,
                 registered_at INTEGER NOT NULL DEFAULT (unixepoch()));
             CREATE TABLE project_deps (
                 project_id INTEGER NOT NULL, dep_id INTEGER NOT NULL,
                 PRIMARY KEY (project_id, dep_id));
             INSERT INTO projects (root_path, db_path, registered_at)
                 VALUES ('/w/one', '/w/one/.spelunk/index.db', 10),
                        ('/w/two', '/w/two/.spelunk/index.db', 20);
             INSERT INTO project_deps VALUES (1, 2);",
        )
        .unwrap();
    }
    let outcome = export_to(&store, &out);
    assert_eq!(outcome.counts.entity.get("project"), Some(&2));
    assert_eq!(outcome.counts.relationship.get("depends_on"), Some(&1));
}

#[test]
fn the_registrys_derived_store_path_is_not_in_the_dump() {
    let dir = tmp();
    let store = dir.path().join("registry.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = Connection::open(&store).unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 root_path TEXT NOT NULL UNIQUE,
                 db_path TEXT NOT NULL,
                 registered_at INTEGER);
             INSERT INTO projects (root_path, db_path) VALUES ('/w/one', '/w/one/.spelunk/x.db');",
        )
        .unwrap();
    }
    export_to(&store, &out);
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(
        !text.contains("x.db"),
        "the store path is derived from the root; carrying it would carry a path that is \
         wrong for any reader laying its stores out differently"
    );
    assert!(text.contains("/w/one"));
}

#[test]
fn a_multi_project_store_names_the_project_rather_than_its_internal_id() {
    let dir = tmp();
    let store = dir.path().join("server.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = Connection::open(&store).unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, slug TEXT NOT NULL UNIQUE,
                 embedding_dim INTEGER NOT NULL DEFAULT 0, created_at INTEGER);
             CREATE TABLE notes (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 project_id INTEGER NOT NULL,
                 kind TEXT NOT NULL DEFAULT 'note', title TEXT NOT NULL, body TEXT NOT NULL,
                 tags TEXT, linked_files TEXT, created_at INTEGER NOT NULL,
                 status TEXT NOT NULL DEFAULT 'active', superseded_by INTEGER,
                 remote_id TEXT, sync_id TEXT);
             CREATE TABLE note_edges (
                 from_id INTEGER NOT NULL, to_id INTEGER NOT NULL, kind TEXT NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                 PRIMARY KEY (from_id, to_id, kind));
             INSERT INTO projects (slug) VALUES ('alpha');
             INSERT INTO notes (project_id, title, body, created_at)
                 VALUES (1, 'one', 'b', 100), (1, 'two', 'b', 200);
             INSERT INTO note_edges (from_id, to_id, kind) VALUES (2, 1, 'contradicts');",
        )
        .unwrap();
    }
    let outcome = export_to(&store, &out);
    assert_eq!(outcome.counts.entity.get("memory_entry"), Some(&2));
    assert_eq!(outcome.counts.relationship.get("contradicts"), Some(&1));
    let recs = records(&out);
    assert_eq!(entities(&recs, "memory_entry")[0]["namespace"], "alpha");
    assert!(
        entities(&recs, "memory_entry")[0].get("uuid").is_none(),
        "a multi project store's own identifiers are arrival ordered, so presenting one as \
         the entry's identity would hand a reader an ordering it did not choose"
    );
}

// ── E21: derived state is never read ─────────────────────────────────────────

#[test]
fn derived_state_is_never_carried() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = memory_store_at(&store, LATEST);
        add_entry(&conn, LATEST, "only", 1_000);
        conn.execute_batch(
            "INSERT INTO memory_fts (rowid, title, body, tags) VALUES (1, 't', 'b', 'x');
             INSERT INTO note_embeddings VALUES (1, x'0102');
             INSERT INTO notes_import_state VALUES (0, 'aaaa', 'bbbb');",
        )
        .unwrap();
    }
    export_to(&store, &out);
    let text = std::fs::read_to_string(&out).unwrap();
    for derived in [
        "memory_fts",
        "note_embeddings",
        "notes_import_state",
        "schema_v896",
        "aaaa",
        "bbbb",
    ] {
        assert!(
            !text.contains(derived),
            "{derived} is derived or invalidated by a move and must not be carried"
        );
    }
}

// ── E22: content detected, not declared ──────────────────────────────────────

#[test]
fn content_is_detected_from_table_shape() {
    let dir = tmp();
    let store = dir.path().join("mixed.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = memory_store_at(&store, LATEST);
        add_entry(&conn, LATEST, "only", 1_000);
        conn.execute_batch(
            "CREATE TABLE usage (command TEXT NOT NULL, called_at INTEGER NOT NULL);
             INSERT INTO usage VALUES ('search', 5);",
        )
        .unwrap();
    }
    let outcome = export_to(&store, &out);
    assert_eq!(outcome.counts.entity.get("memory_entry"), Some(&1));
    assert_eq!(outcome.counts.entity.get("command_usage"), Some(&1));
}

// ── E23: the source is never modified ────────────────────────────────────────

#[test]
fn the_source_store_is_never_modified() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = memory_store_at(&store, LATEST);
        add_entry(&conn, LATEST, "only", 1_000);
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").ok();
    }
    let before = std::fs::read(&store).unwrap();
    export_to(&store, &out);
    let after = std::fs::read(&store).unwrap();
    assert_eq!(before, after, "the store was modified by exporting it");
    for sidecar in ["memory.db-wal", "memory.db-journal", "memory.db-shm"] {
        assert!(
            !dir.path().join(sidecar).exists(),
            "{sidecar} was created; the store must be opened read-only"
        );
    }
}

// Every store the product creates runs in write-ahead-log mode, so a fixture in
// the default rollback-journal mode cannot fail for the shape that matters:
// committed rows can live in the log rather than the database file, and a
// reader that misses them silently exports a stale store.
#[test]
fn a_write_ahead_log_store_is_read_whole_and_left_unchanged() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    // The connection stays open for the whole test: closing the last one
    // checkpoints the log away, and a store nobody has open is not the state a
    // real export runs against.
    let conn = wal_memory_store_at(&store, LATEST);
    add_entry(&conn, LATEST, "checkpointed", 1_000);
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    add_entry(&conn, LATEST, "still in the log", 2_000);

    let log = dir.path().join("memory.db-wal");
    assert!(
        log.exists() && std::fs::metadata(&log).unwrap().len() > 0,
        "the fixture must leave a committed entry in the log"
    );

    let before_db = std::fs::read(&store).unwrap();
    let before_log = std::fs::read(&log).unwrap();
    let outcome = export_to(&store, &out);

    assert_eq!(
        outcome.counts.entity.get("memory_entry"),
        Some(&2),
        "an entry committed to the log is committed"
    );
    let recs = records(&out);
    let titles: Vec<&str> = entities(&recs, "memory_entry")
        .iter()
        .map(|e| e["title"].as_str().unwrap())
        .collect();
    assert!(titles.contains(&"still in the log"), "got: {titles:?}");

    assert_eq!(
        std::fs::read(&store).unwrap(),
        before_db,
        "the database file was modified by exporting it"
    );
    assert_eq!(
        std::fs::read(&log).unwrap(),
        before_log,
        "the log was modified by exporting it; no content may be checkpointed \
         or appended by a read"
    );
    assert!(
        !dir.path().join("memory.db-journal").exists(),
        "a rollback journal means the store was opened for writing"
    );
    drop(conn);
}

// Reading a write-ahead-log store requires SQLite's shared-memory index, and a
// read-only connection cannot decline to bring it into being. So a store that
// nobody currently has open gains an index file and an empty log from being
// exported. Neither carries any content and the database is untouched, but the
// files do appear in the user's directory and that is worth pinning rather than
// discovering. Removing them afterwards is not an option: a second process may
// have opened the store in the meantime and be coordinating through exactly
// those files.
#[test]
fn reading_a_write_ahead_log_store_adds_no_content_to_the_users_directory() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = wal_memory_store_at(&store, LATEST);
        add_entry(&conn, LATEST, "only", 1_000);
    }
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "closing the last connection should leave the store alone in the directory"
    );
    let before = std::fs::read(&store).unwrap();

    export_to(&store, &out);

    assert_eq!(
        std::fs::read(&store).unwrap(),
        before,
        "the database file was modified by exporting it"
    );
    assert!(
        !dir.path().join("memory.db-journal").exists(),
        "a rollback journal means the store was opened for writing"
    );
    let log = dir.path().join("memory.db-wal");
    if log.exists() {
        assert_eq!(
            std::fs::metadata(&log).unwrap().len(),
            0,
            "a read must not append anything to the log"
        );
    }
}

// Neither test above can fail if the store is opened for writing, and between
// them they are why: the first checkpoints the log away before it measures, so
// there is nothing left for a close-time checkpoint to flush, and the second
// holds a second connection open for its whole length, which suppresses the
// close-time checkpoint entirely. Both are right about what they test. Neither
// reaches the state where opening for writing costs the user something.
//
// That state is a store whose owning process died with rows still committed to
// the log: the log is hot and no connection is left. Closing the last
// connection to a log-mode store checkpoints it, so a read-write open here
// recovers the log and rewrites the database file, and deletes the log, purely
// as a side effect of having read it. Read-only is what prevents that, and it
// is one flag.
#[test]
fn a_store_left_hot_by_a_dead_process_is_not_checkpointed_by_reading_it() {
    let dir = tmp();
    let owned = dir.path().join("owned");
    std::fs::create_dir(&owned).unwrap();
    let conn = wal_memory_store_at(&owned.join("memory.db"), LATEST);
    add_entry(&conn, LATEST, "committed to the log", 1_000);

    // The schema was written before the switch to log mode, so the entry is the
    // only thing in the log, and an export that ignored the log would come back
    // with an empty store rather than a wrong one.
    let owned_log = owned.join("memory.db-wal");
    assert!(std::fs::metadata(&owned_log).unwrap().len() > 0);

    // Copying the pair out from under a live connection reproduces what a dead
    // process leaves, without killing one: the copies have a hot log and no
    // owner, and dropping the connection below checkpoints the original rather
    // than them.
    let abandoned = dir.path().join("abandoned");
    std::fs::create_dir(&abandoned).unwrap();
    let store = abandoned.join("memory.db");
    let log = abandoned.join("memory.db-wal");
    std::fs::copy(owned.join("memory.db"), &store).unwrap();
    std::fs::copy(&owned_log, &log).unwrap();
    drop(conn);

    let before_db = std::fs::read(&store).unwrap();
    let before_log = std::fs::read(&log).unwrap();

    // Run the binary, so the comparison below is made once every handle the
    // export held has been closed by the operating system, not merely dropped.
    let out = dir.path().join("dump.jsonl");
    let run = std::process::Command::new(env!("CARGO_BIN_EXE_spelunk-export"))
        .arg("export")
        .arg("--store")
        .arg(&store)
        .arg("--out")
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "the export failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    let titles: Vec<String> = entities(&records(&out), "memory_entry")
        .iter()
        .map(|e| e["title"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        titles,
        ["committed to the log"],
        "the entry lives only in the log, so anything else here means the log \
         was not read at all"
    );

    // Compared with `assert!` rather than `assert_eq!`: these files are
    // megabytes, and a failure here is about which file moved, not which byte.
    assert!(
        std::fs::read(&store).unwrap() == before_db,
        "the database file was rewritten by reading it: the log was checkpointed \
         into it on close"
    );
    assert!(
        log.exists(),
        "the log was deleted by reading it; the user's committed rows now exist \
         only wherever the checkpoint put them"
    );
    assert!(
        std::fs::read(&log).unwrap() == before_log,
        "the log was rewritten by reading it"
    );
}

// ── E26 to E31: refusal, emptiness, determinism ──────────────────────────────

#[test]
fn an_empty_store_produces_a_valid_empty_dump() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    drop(memory_store_at(&store, LATEST));
    let outcome = export_to(&store, &out);
    assert!(outcome.counts.entity.is_empty());
    let recs = records(&out);
    assert_eq!(recs.len(), 2, "an empty dump is a header and a footer");
    dump::verify_rendered(&std::fs::read_to_string(&out).unwrap()).unwrap();
}

#[test]
fn an_unrecognisable_store_is_refused_and_leaves_no_partial_dump() {
    let dir = tmp();
    let store = dir.path().join("not-a-store.db");
    let out = dir.path().join("dump.jsonl");
    std::fs::write(&store, b"this is not a database").unwrap();
    assert!(export(&store, &out, 0).is_err());
    assert!(!out.exists());
    assert!(!out.with_extension("partial").exists());
}

#[test]
fn a_store_with_no_authored_tables_is_refused_rather_than_read_as_entries() {
    let dir = tmp();
    let store = dir.path().join("notes-missing-columns.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = Connection::open(&store).unwrap();
        conn.execute_batch("CREATE TABLE notes (id INTEGER PRIMARY KEY, headline TEXT);")
            .unwrap();
    }
    let err = format!("{:#}", export(&store, &out, 0).unwrap_err());
    assert!(err.contains("missing required columns"), "got: {err}");
    assert!(err.contains("notes-missing-columns.db"), "got: {err}");
    assert!(!out.exists());
}

#[test]
fn exporting_an_unchanged_store_twice_is_byte_identical() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    {
        let conn = memory_store_at(&store, LATEST);
        add_entry(&conn, LATEST, "a", 1_000);
        add_entry(&conn, LATEST, "b", 2_000);
    }
    let first = dir.path().join("one.jsonl");
    let second = dir.path().join("two.jsonl");
    export_to(&store, &first);
    export_to(&store, &second);
    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap()
    );
}

// ── E25: no secret material ──────────────────────────────────────────────────

#[test]
fn a_dump_carries_no_credential_material() {
    // Nothing under the config directory is read at all, which is what makes
    // this true by construction rather than by filtering. The assertion is on
    // the record kinds a dump can contain: a new one would have to be added
    // here deliberately.
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = memory_store_at(&store, LATEST);
        add_entry(&conn, LATEST, "only", 1_000);
    }
    export_to(&store, &out);
    for rec in records(&out) {
        let kind = rec["record"].as_str().unwrap();
        assert!(
            matches!(kind, "header" | "footer" | "entity" | "relationship"),
            "unexpected record kind {kind}"
        );
        if kind == "entity" {
            let t = rec["type"].as_str().unwrap();
            assert!(
                matches!(t, "memory_entry" | "project" | "command_usage"),
                "unexpected entity type {t}"
            );
        }
    }
}

// ── An endpoint that is genuinely absent ─────────────────────────────────────
//
// The store is read as of one point in time, so a link whose endpoint is not in
// the read is a link whose endpoint is not in the store: damage that predates
// foreign key enforcement, or a build against a SQLite that had it off. It
// cannot be a write that arrived mid-export.
//
// That makes the question a real one rather than an ambiguity to paper over,
// and the answer is to report and continue rather than refuse. Refusing would
// mean a user whose store has one orphaned link cannot get any of their entries
// out, and no other tool can do better, because the format cannot express a
// link to something that does not exist and the missing row is not recoverable
// from anywhere. The loss is unavoidable; only the silence was ever the
// problem. So every entry is carried, the orphaned link is not, and the run
// says so on the same screen where it reports success.

#[test]
fn a_link_to_a_missing_entry_is_reported_rather_than_carried_or_hidden() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = memory_store_at(&store, LATEST);
        let a = add_entry(&conn, LATEST, "a", 1_000);
        // Foreign keys are on by default in this workspace's SQLite build, so a
        // dangling link cannot be created through it. It can still reach the
        // field: a build against a system SQLite has them off, and the stores
        // predate the enforcement. The export has to survive one either way.
        conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        conn.execute(
            "UPDATE notes SET superseded_by = 999 WHERE id = ?1",
            rusqlite::params![a],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO memory_edges (from_id, to_id, kind) VALUES (998, 1, 'relates_to')",
        )
        .unwrap();
    }
    let outcome = export(&store, &out, 1_700_000_000)
        .expect("an orphaned link must not cost the user every entry in the store");
    assert!(outcome.counts.relationship.is_empty());
    assert_eq!(
        outcome.counts.entity.get("memory_entry"),
        Some(&1),
        "the entries themselves are intact and must all be carried"
    );
    assert_eq!(
        outcome.warnings.len(),
        2,
        "both orphaned links must be reported: {:?}",
        outcome.warnings
    );
    let summary = outcome.summary(&out);
    assert!(
        summary.contains("2 link(s) were NOT carried"),
        "the run must not report success without reporting the omission: {summary}"
    );
}

// ── Inventory ────────────────────────────────────────────────────────────────

#[test]
fn inventory_reports_shape_and_counts_without_touching_the_store() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    {
        let conn = memory_store_at(&store, LATEST);
        add_entry(&conn, LATEST, "a", 1_000);
        add_entry(&conn, LATEST, "b", 2_000);
    }
    let before = std::fs::read(&store).unwrap();
    let report = inventory::describe(&store);
    assert!(report.exists);
    assert_eq!(report.schema_version, Some(LATEST as i64));
    let notes = report
        .contents
        .iter()
        .find(|t| t.table == "notes")
        .expect("notes should be reported");
    assert_eq!(notes.rows, 2);
    assert!(report.contents.iter().all(|t| t.table != "memory_fts"));
    assert_eq!(std::fs::read(&store).unwrap(), before);
}

#[test]
fn inventory_reports_a_missing_store_as_missing_rather_than_failing() {
    let dir = tmp();
    let report = inventory::describe(&dir.path().join("nothing-here.db"));
    assert!(!report.exists);
    assert!(report.contents.is_empty());
}

#[test]
fn inventory_reports_an_unreadable_store_without_losing_the_rest() {
    let dir = tmp();
    let store = dir.path().join("broken.db");
    std::fs::write(&store, b"not a database").unwrap();
    let report = inventory::describe(&store);
    assert!(report.exists);
    assert!(report.unreadable.is_some());
}

fn edge(conn: &Connection, from: i64, to: i64, kind: &str) {
    conn.execute(
        "INSERT INTO memory_edges (from_id, to_id, kind, created_at) VALUES (?1, ?2, ?3, 42)",
        rusqlite::params![from, to, kind],
    )
    .unwrap();
}
