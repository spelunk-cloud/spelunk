//! Build a memory store at every shape the product has ever written, and prove
//! the exporter reads all of them.
//!
//! The shapes here are the shipped schema statements, applied in the order
//! the product applies them and stopped at a chosen step. They cover what the
//! schema *is* at each version. What they cannot cover is what a real released
//! binary put in the file: that needs artifacts captured by running those
//! binaries, which is what the upgrade corpus exists for. These two are
//! complements, not substitutes, and neither is sufficient alone.

use rusqlite::Connection;

pub const LATEST: i32 = 10;

/// Each step, verbatim from the corresponding shipped schema file.
///
/// The vector table is created as an ordinary table rather than the virtual
/// table the product creates. The module backing it is linked into the product
/// and not into a plain SQLite, and its contents are never read here anyway, so
/// standing it in costs nothing and buys a second assertion: whatever shape it
/// has, the export must ignore it.
fn step(conn: &Connection, version: i32) {
    let sql: &str = match version {
        1 => {
            "CREATE TABLE IF NOT EXISTS notes (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 kind          TEXT    NOT NULL DEFAULT 'note',
                 title         TEXT    NOT NULL,
                 body          TEXT    NOT NULL,
                 tags          TEXT,
                 linked_files  TEXT,
                 created_at    INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE TABLE IF NOT EXISTS note_embeddings (
                 note_id INTEGER PRIMARY KEY, embedding BLOB
             );"
        }
        2 => {
            "ALTER TABLE notes ADD COLUMN status TEXT NOT NULL DEFAULT 'active';
             ALTER TABLE notes ADD COLUMN superseded_by INTEGER REFERENCES notes(id);"
        }
        3 => "ALTER TABLE notes ADD COLUMN source_ref TEXT;",
        4 => {
            "CREATE TABLE IF NOT EXISTS memory_fts (
                 rowid INTEGER PRIMARY KEY, title TEXT, body TEXT, tags TEXT
             );"
        }
        5 => {
            "ALTER TABLE notes ADD COLUMN valid_at INTEGER;
             ALTER TABLE notes ADD COLUMN invalid_at INTEGER;
             CREATE INDEX IF NOT EXISTS idx_memory_invalid_at ON notes(invalid_at);"
        }
        6 => {
            "CREATE TABLE IF NOT EXISTS memory_edges (
                 from_id    INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
                 to_id      INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
                 kind       TEXT    NOT NULL
                            CHECK(kind IN ('supersedes', 'relates_to', 'contradicts')),
                 created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                 PRIMARY KEY (from_id, to_id, kind)
             );
             CREATE INDEX IF NOT EXISTS idx_memory_edges_from ON memory_edges(from_id);
             CREATE INDEX IF NOT EXISTS idx_memory_edges_to   ON memory_edges(to_id);"
        }
        7 => {
            "ALTER TABLE notes ADD COLUMN uuid TEXT;
             ALTER TABLE notes ADD COLUMN remote_id TEXT;
             CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_uuid
                 ON notes(uuid) WHERE uuid IS NOT NULL;
             CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_remote_id
                 ON notes(remote_id) WHERE remote_id IS NOT NULL;"
        }
        8 => {
            "ALTER TABLE notes ADD COLUMN entity_id TEXT;
             CREATE INDEX IF NOT EXISTS idx_notes_entity_id
                 ON notes(entity_id) WHERE entity_id IS NOT NULL;"
        }
        9 => {
            "CREATE TABLE IF NOT EXISTS schema_v896_note_embeddings
                 (sentinel INTEGER PRIMARY KEY);"
        }
        10 => {
            "CREATE TABLE IF NOT EXISTS notes_import_state (
                 id INTEGER PRIMARY KEY CHECK (id = 0),
                 last_merged_tracking_oid TEXT,
                 last_imported_working_oid TEXT
             );"
        }
        other => panic!("no schema step {other}"),
    };
    conn.execute_batch(sql).unwrap();
}

/// A memory store at `version`, with `user_version` stamped.
pub fn memory_store_at(path: &std::path::Path, version: i32) -> Connection {
    stamped(path, version, true)
}

/// A memory store at `version` that never had its version stamped, which is
/// every store written before the runner existed.
pub fn unstamped_memory_store_at(path: &std::path::Path, version: i32) -> Connection {
    stamped(path, version, false)
}

fn stamped(path: &std::path::Path, version: i32, stamp: bool) -> Connection {
    let conn = Connection::open(path).unwrap();
    for v in 1..=version {
        step(&conn, v);
    }
    if stamp {
        conn.execute_batch(&format!("PRAGMA user_version = {version}"))
            .unwrap();
    }
    conn
}

/// Insert one entry using only the columns that exist at `version`.
pub fn add_entry(conn: &Connection, version: i32, title: &str, created_at: i64) -> i64 {
    conn.execute(
        "INSERT INTO notes (kind, title, body, tags, linked_files, created_at)
         VALUES ('decision', ?1, 'body of ' || ?1, 'alpha, beta', 'src/a.rs', ?2)",
        rusqlite::params![title, created_at],
    )
    .unwrap();
    let id = conn.last_insert_rowid();
    if version >= 7 {
        conn.execute(
            "UPDATE notes SET uuid = ?2 WHERE id = ?1",
            rusqlite::params![id, format!("0192f0a0-0000-7000-8000-0000000000{id:02x}")],
        )
        .unwrap();
    }
    if version >= 8 {
        conn.execute(
            "UPDATE notes SET entity_id = ?2 WHERE id = ?1",
            rusqlite::params![id, format!("sha256-of-{title}")],
        )
        .unwrap();
    }
    id
}
