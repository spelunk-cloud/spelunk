//! Integration tests for `spelunk memory reconcile`.
//!
//! Covers all 8 acceptance criteria:
//!
//! 1. Dedup by `entity_id` — sha256 over the canonical JSON of {body, kind,
//!    title} (ADR-068) — not by rowid.
//! 2. No-op when server.db is absent (exit 0, no error).
//! 3. Read-only guarantee on server.db (implementer opens it read-only).
//! 4. Archived rows in server.db import as archived in memory.db.
//! 5. --dry-run flag: report what would be imported without writing.
//! 6. SPELUNK_NO_SERVER=1 path: reconcile exits cleanly without a running server.
//! 7. Mid-run rollback: if a row import fails mid-transaction, the whole
//!    transaction rolls back (no partial import).
//! 8. Exit codes: 0 on success and on no-op; non-zero only on real fault.

mod plumbing_helpers;
use plumbing_helpers::{mount_health, mount_index_embed, spelunk_bin, spelunk_bin_in};

use assert_cmd::Command;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tempfile::TempDir;
use wiremock::MockServer;

// ── Test helpers ──────────────────────────────────────────────────────────────

/// Register the sqlite-vec extension once per test process so that rusqlite
/// connections in the test binary can open memory.db files that contain the
/// `note_embeddings` vec0 virtual table created by the CLI.
///
/// Must be called before any `Connection::open` on a spelunk memory.db.
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

/// Write a minimal spelunk config file and make `dir` a real project.
///
/// ADR-067: `memory reconcile` (a memory subcommand) fails closed without a
/// local `.spelunk/` project, so we create `<dir>/.spelunk/`. Memory is now
/// project-scoped: the CLI resolves it to `<dir>/.spelunk/memory.db` regardless
/// of the config `db_path`. The incoming `db_path` argument is ignored (kept for
/// call-site compatibility).
///
/// We deliberately do NOT pass `memory --db` in `reconcile_cmd` because the
/// `MemoryArgs.db` arg is `global = true` in clap, which means a second `--db`
/// on the `reconcile` subcommand would override the memory path rather than
/// setting the reconcile source path.
///
/// Returns `(config_path, mem_path)` where `mem_path` is where the CLI will
/// write `memory.db`.
fn write_config(dir: &Path, _db_path: &Path) -> (PathBuf, PathBuf) {
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
    // Project-scoped memory store lives next to the index inside `.spelunk/`.
    let mem_path = index_db.with_file_name("memory.db");
    (config_path, mem_path)
}

/// Create a minimal server.db with the server schema and a single project.
///
/// The database is created in WAL journal mode so that the CLI can open it
/// read-only with `PRAGMA journal_mode=WAL` without error (setting WAL on a
/// read-only connection is a no-op when the DB is already in WAL mode).
///
/// Returns `(db_path, project_id)`.
fn create_server_db(dir: &Path, slug: &str) -> (PathBuf, i64) {
    let path = dir.join("server.db");
    let conn = Connection::open(&path).expect("open server.db");

    // Enable WAL mode before creating the schema.
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .expect("set WAL");

    // Apply the server schema (matches spelunk-server/migrations/server_001.sql).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS projects (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            slug          TEXT    NOT NULL UNIQUE,
            embedding_dim INTEGER NOT NULL DEFAULT 0,
            created_at    INTEGER NOT NULL DEFAULT (unixepoch())
         );
         CREATE TABLE IF NOT EXISTS notes (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            project_id    INTEGER NOT NULL REFERENCES projects(id),
            kind          TEXT    NOT NULL DEFAULT 'note',
            title         TEXT    NOT NULL,
            body          TEXT    NOT NULL,
            tags          TEXT,
            linked_files  TEXT,
            created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
            status        TEXT    NOT NULL DEFAULT 'active',
            superseded_by INTEGER REFERENCES notes(id)
         );
         CREATE INDEX IF NOT EXISTS idx_notes_project ON notes(project_id);",
    )
    .expect("create server schema");

    conn.execute(
        "INSERT INTO projects (slug) VALUES (?1)",
        rusqlite::params![slug],
    )
    .expect("insert project");
    let project_id = conn.last_insert_rowid();

    (path, project_id)
}

/// Insert a note row into an already-open server.db connection.
#[allow(clippy::too_many_arguments)]
fn insert_server_note(
    conn: &Connection,
    project_id: i64,
    kind: &str,
    title: &str,
    body: &str,
    tags: Option<&str>,
    linked_files: Option<&str>,
    created_at: i64,
    status: &str,
    superseded_by: Option<i64>,
) -> i64 {
    conn.execute(
        "INSERT INTO notes \
         (project_id, kind, title, body, tags, linked_files, created_at, status, superseded_by) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            project_id,
            kind,
            title,
            body,
            tags,
            linked_files,
            created_at,
            status,
            superseded_by,
        ],
    )
    .expect("insert server note");
    conn.last_insert_rowid()
}

/// Count rows in memory.db's notes table.
fn count_memory_notes(mem_path: &Path) -> i64 {
    ensure_sqlite_vec();
    let conn = Connection::open(mem_path).expect("open memory.db");
    conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap_or(0)
}

/// Read all notes from memory.db, returning (kind, title, status) tuples.
fn read_memory_notes(mem_path: &Path) -> Vec<(String, String, String)> {
    ensure_sqlite_vec();
    let conn = Connection::open(mem_path).expect("open memory.db");
    let mut stmt = conn
        .prepare("SELECT kind, title, status FROM notes ORDER BY created_at ASC")
        .expect("prepare");
    stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })
    .expect("query")
    .collect::<rusqlite::Result<Vec<_>>>()
    .expect("collect")
}

/// Build a `spelunk memory reconcile` command with the common flags pre-set.
///
/// `config_path`  - the `--config` arg (controls where memory.db is resolved via config db_path)
/// `server_db`    - the `reconcile --source-db` arg (source server.db)
///
/// The memory.db location is derived from the `db_path` config key rather than
/// from `memory --db`, because `MemoryArgs.db` is a `global = true` clap arg
/// whose VALUE is propagated by clap to all sub-commands using the same field
/// name `db`.  The `MemoryReconcileArgs` field was renamed to `source_db` and
/// exposed as `--source-db` to break this collision; tests pass the source path
/// via `--source-db` and rely on config for memory.db resolution.
///
/// IMPORTANT: the process's `current_dir` is set to the temp dir so that
/// `find_project_db()` does not walk up into the repo root and discover the
/// project's real `.spelunk/index.db`, which would cause the CLI to write to
/// the repo's own memory.db instead of the test's isolated one.
fn reconcile_cmd(config_path: &Path, server_db: &Path) -> Command {
    // config_path is in the temp dir (e.g. /tmp/tmpXXX/config.toml).
    // Run from that temp dir so find_project_db() returns None and the CLI
    // uses the config db_path for memory.db resolution.
    let tmp_dir = config_path
        .parent()
        .expect("config_path must have a parent");
    let mut cmd = spelunk_bin();
    cmd.current_dir(tmp_dir)
        .env("SPELUNK_NO_SERVER", "1")
        .env("SPELUNK_NO_RECONCILE_NUDGE", "1")
        .arg("--config")
        .arg(config_path)
        .arg("memory")
        .arg("reconcile")
        .arg("--source-db")
        .arg(server_db);
    cmd
}

// ── AC-2: No-op when server.db is absent ─────────────────────────────────────

#[test]
fn noop_when_server_db_absent() {
    // Criteria #2: exit 0, no error output, when server.db does not exist.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);
    let missing_server_db = tmp.path().join("nonexistent_server.db");

    reconcile_cmd(&config_path, &missing_server_db)
        .assert()
        .success(); // exit 0
}

#[test]
fn noop_when_server_db_absent_json_output_is_valid() {
    // Criteria #2 + #5: when server.db is absent and --format json is passed,
    // the summary is emitted on stdout as valid JSON with candidates=0.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);
    let missing_server_db = tmp.path().join("nonexistent_server.db");

    let output = reconcile_cmd(&config_path, &missing_server_db)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).expect("stdout should be valid JSON when --format json");
    assert_eq!(
        value["candidates"].as_i64(),
        Some(0),
        "candidates should be 0 when server.db absent"
    );
    assert_eq!(
        value["imported"].as_i64(),
        Some(0),
        "imported should be 0 when server.db absent"
    );
}

// ── AC-6: SPELUNK_NO_SERVER=1 path ───────────────────────────────────────────

#[test]
fn spelunk_no_server_exits_cleanly_with_import() {
    // Criteria #6: even with SPELUNK_NO_SERVER=1 (no embedding server),
    // reconcile should complete successfully and import rows (without embeddings).
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "test-project";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Use SQLite for storage",
        "SQLite is the right choice because it is zero-infrastructure.",
        None,
        None,
        1_700_000_000,
        "active",
        None,
    );
    drop(conn);

    // SPELUNK_NO_SERVER=1 is already set by reconcile_cmd.
    // Use --all-projects so the slug from server.db is used regardless of cwd.
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success(); // exit 0

    // Notes should have been imported without embeddings.
    assert_eq!(
        count_memory_notes(&mem_path),
        1,
        "note should be imported even without embedding server"
    );
}

// ── local_first must embed via loopback, not a bare server_url ──

/// Start a mock spelunk-server (health + `/index/embed` mounted) on a
/// dedicated runtime kept alive for the caller's duration.
fn start_mock() -> (tokio::runtime::Runtime, MockServer) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let server = rt.block_on(async {
        let server = MockServer::start().await;
        mount_health(&server).await;
        mount_index_embed(&server).await;
        server
    });
    (rt, server)
}

#[test]
fn local_first_with_server_url_still_embeds_via_loopback() {
    // Regression guard: step 5's best-effort embed used
    // to call `ServerInferenceClient::from_config` on the raw (unbridged)
    // config, so a `local_first` project with an explicit `server_url`
    // silently imported every note WITHOUT an embedding, the exact
    // silent-unembedded-write symptom this fix eliminates. It must bridge
    // via `get_inference_tier`/`effective_config` like `add`/`reindex`/
    // `search` do, and reach the local loopback embedder instead.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "loopback-embed-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Prefer local embedding",
        "body one",
        None,
        None,
        1_700_005_000,
        "active",
        None,
    );
    drop(conn);

    let (_rt, mock) = start_mock();
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&state_dir).expect("create state dir");
    let port: u16 = mock
        .uri()
        .rsplit(':')
        .next()
        .expect("uri has a port")
        .trim_end_matches('/')
        .parse()
        .expect("uri port is numeric");
    std::fs::write(state_dir.join("server.port"), format!("{port}\n")).expect("write server.port");

    let output = reconcile_cmd(&config_path, &server_db)
        .env_remove("SPELUNK_NO_SERVER")
        .env("SPELUNK_STATE_DIR", &state_dir)
        // Deliberately unroutable: local_first must never fall back to this,
        // an accidental fallback surfaces as a connection error, not a
        // silent unembedded import.
        .env("SPELUNK_SERVER_URL", "https://cloud.invalid.example:1")
        .env("SPELUNK_PROJECT_ID", slug)
        .arg("--all-projects")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).expect("stdout must be valid JSON");

    assert_eq!(value["imported"].as_i64(), Some(1));
    assert_eq!(
        value["imported_without_embedding"].as_i64(),
        Some(0),
        "local_first must embed via the loopback server even with an explicit \
         (and unroutable) server_url configured: {value}"
    );
    assert_eq!(count_memory_notes(&mem_path), 1);
}

// ── AC-1: Dedup by content-hash, not rowid ───────────────────────────────────

#[test]
fn dedup_by_content_hash_not_rowid() {
    // Criteria #1: running reconcile twice imports rows once, not twice,
    // even if rowids differ between runs (we delete and reinsert in server.db).
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "dedup-project";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    // Insert two distinct notes with fixed timestamps.
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "First note",
        "body of first note",
        Some("tag-a"),
        None,
        1_700_000_001,
        "active",
        None,
    );
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Second note",
        "body of second note",
        None,
        None,
        1_700_000_002,
        "active",
        None,
    );
    drop(conn);

    // First reconcile run - imports both notes.
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(
        count_memory_notes(&mem_path),
        2,
        "both notes should be imported on first run"
    );

    // Second reconcile run - should be a no-op (same content, same hash).
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(
        count_memory_notes(&mem_path),
        2,
        "second run must not duplicate existing notes"
    );
}

#[test]
fn dedup_ignores_rowid_changes() {
    // Criteria #1: a note with a different server-side rowid but identical
    // content should NOT be re-imported.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "rowid-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    // Insert note with rowid=1 via autoincrement.
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Always use UTC",
        "Timezones cause bugs. Use UTC everywhere.",
        None,
        None,
        1_700_000_100,
        "active",
        None,
    );
    drop(conn);

    // First run: import.
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(count_memory_notes(&mem_path), 1);

    // Delete the note and re-insert it - new rowid, same content.
    let conn = Connection::open(&server_db).unwrap();
    conn.execute(
        "DELETE FROM notes WHERE project_id = ?1",
        rusqlite::params![project_id],
    )
    .unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Always use UTC",
        "Timezones cause bugs. Use UTC everywhere.",
        None,
        None,
        1_700_000_100, // same created_at
        "active",
        None,
    );
    drop(conn);

    // Second run: same content hash, still only 1 note in memory.db.
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(
        count_memory_notes(&mem_path),
        1,
        "content-identical note with new rowid must not be re-imported"
    );
}

/// Insert a server note at an explicit rowid, so the source store's ids can be
/// made to diverge from the ids memory.db will assign on import.
#[allow(clippy::too_many_arguments)]
fn insert_server_note_with_id(
    conn: &Connection,
    id: i64,
    project_id: i64,
    title: &str,
    body: &str,
    created_at: i64,
    status: &str,
    superseded_by: Option<i64>,
) {
    conn.execute(
        "INSERT INTO notes \
         (id, project_id, kind, title, body, tags, linked_files, created_at, status, superseded_by) \
         VALUES (?1, ?2, 'decision', ?3, ?4, NULL, NULL, ?5, ?6, ?7)",
        rusqlite::params![id, project_id, title, body, created_at, status, superseded_by],
    )
    .expect("insert server note with id");
}

#[test]
fn supersede_edge_resolves_across_a_rowid_renumber() {
    // Two independent reasons a rowid-based edge breaks here, both live:
    //  1. the source rows sit at ids 101/102 while memory.db numbers them 2/3;
    //  2. an earlier note is already imported, so the pair's position among the
    //     *candidates* differs from its position in the *import set*.
    // Resolved by entity_id, the edge is immune to both.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "supersede-renumber";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    // Phase 1: one earlier note, imported on its own.
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note_with_id(
        &conn,
        100,
        project_id,
        "Unrelated earlier note",
        "already imported",
        1_700_000_100,
        "active",
        None,
    );
    drop(conn);
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(
        count_memory_notes(&mem_path),
        1,
        "phase 1 imported one note"
    );

    // Phase 2: the supersede pair arrives. Note 100 is now already present, so
    // the pair's candidate indices (1, 2) no longer match its import-set
    // indices (0, 1).
    let conn = Connection::open(&server_db).unwrap();
    // Successor first: `superseded_by` is a FK, so 102 must exist before 101
    // can reference it. Import order is driven by created_at, not insert order.
    insert_server_note_with_id(
        &conn,
        102,
        project_id,
        "New approach",
        "successor body",
        1_700_000_501,
        "active",
        None,
    );
    insert_server_note_with_id(
        &conn,
        101,
        project_id,
        "Old approach",
        "superseded body",
        1_700_000_500,
        "archived",
        Some(102), // → server rowid of the successor
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    let mem = Connection::open(&mem_path).unwrap();
    let (old_id, old_succ): (i64, Option<i64>) = mem
        .query_row(
            "SELECT id, superseded_by FROM notes WHERE title = 'Old approach'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let new_id: i64 = mem
        .query_row(
            "SELECT id FROM notes WHERE title = 'New approach'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // Guard the premise: local ids must actually differ from the server's.
    assert!(
        old_id != 101 && new_id != 102,
        "memory.db must have renumbered ({old_id}, {new_id}) — otherwise this proves nothing"
    );
    assert_eq!(
        old_succ,
        Some(new_id),
        "the supersede edge must point at the successor's local rowid"
    );
}

#[test]
fn dedup_key_excludes_created_at() {
    // `created_at` is not part of the identity: a second machine recording the
    // same decision cannot reproduce the first one's timestamp. Two rows with
    // identical text therefore collapse to one entry, whatever their timestamps.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "hash-ts-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    // Same content, different timestamps.
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Identical title",
        "Identical body",
        None,
        None,
        1_700_000_001,
        "active",
        None,
    );
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Identical title",
        "Identical body",
        None,
        None,
        1_700_000_002, // different created_at
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    assert_eq!(
        count_memory_notes(&mem_path),
        1,
        "identical text at different times is one entity"
    );

    // Re-running is a no-op: the collapsed entry is already present.
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(count_memory_notes(&mem_path), 1, "re-run imports nothing");
}

#[test]
fn dedup_key_excludes_tags_which_union_on_collapse() {
    // Two rows with identical text but disjoint tags/linked_files are one
    // entity; the survivor carries the union rather than dropping either set.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "tag-union-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Union title",
        "Union body",
        Some("alpha"),
        Some("a.rs"),
        1_700_000_001,
        "active",
        None,
    );
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Union title",
        "Union body",
        Some("beta"),
        Some("b.rs"),
        1_700_000_002,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    assert_eq!(count_memory_notes(&mem_path), 1, "one entity");

    let mem = Connection::open(&mem_path).unwrap();
    let (tags, files): (String, String) = mem
        .query_row("SELECT tags, linked_files FROM notes", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    for want in ["alpha", "beta"] {
        assert!(tags.contains(want), "tags {tags:?} must union {want}");
    }
    for want in ["a.rs", "b.rs"] {
        assert!(files.contains(want), "files {files:?} must union {want}");
    }
}

#[test]
fn collapse_onto_stored_row_unions_tags_rather_than_dropping_them() {
    // The add-wins union must also fire when a candidate collapses onto a row
    // already in memory.db, not just against a sibling candidate. Tags/files
    // are outside the key, so without the merge the losing copy's metadata is
    // discarded silently — the row is "already present" and simply skipped.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "stored-union-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    // Pass 1: import the entry carrying only `alpha` / `a.rs`.
    let conn = Connection::open(&server_db).unwrap();
    let first_id = insert_server_note(
        &conn,
        project_id,
        "decision",
        "Union title",
        "Union body",
        Some("alpha"),
        Some("a.rs"),
        1_700_000_001,
        "active",
        None,
    );
    drop(conn);
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(count_memory_notes(&mem_path), 1, "pass 1 imports the entry");

    // Pass 2: the same text reappears with different tags/files. Same entity,
    // so it will not re-import — its metadata has to merge into the stored row.
    let conn = Connection::open(&server_db).unwrap();
    conn.execute(
        "DELETE FROM notes WHERE id = ?1",
        rusqlite::params![first_id],
    )
    .unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Union title",
        "Union body",
        Some("beta"),
        Some("b.rs"),
        1_700_000_002,
        "active",
        None,
    );
    drop(conn);
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    assert_eq!(
        count_memory_notes(&mem_path),
        1,
        "identical text stays one entity"
    );

    let mem = Connection::open(&mem_path).unwrap();
    let (tags, files): (String, String) = mem
        .query_row("SELECT tags, linked_files FROM notes", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    for want in ["alpha", "beta"] {
        assert!(
            tags.contains(want),
            "tags {tags:?} must keep {want} after collapsing onto the stored row"
        );
    }
    for want in ["a.rs", "b.rs"] {
        assert!(
            files.contains(want),
            "linked_files {files:?} must keep {want} after collapsing onto the stored row"
        );
    }
}

#[test]
fn dry_run_does_not_union_tags_into_a_stored_row() {
    // The tag merge is a write. `--dry-run` must stop before it, not just
    // before the insert.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "dryrun-union-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    let first_id = insert_server_note(
        &conn,
        project_id,
        "decision",
        "Union title",
        "Union body",
        Some("alpha"),
        None,
        1_700_000_001,
        "active",
        None,
    );
    drop(conn);
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    // Same entity, new tag — a live run would merge `beta` in.
    let conn = Connection::open(&server_db).unwrap();
    conn.execute(
        "DELETE FROM notes WHERE id = ?1",
        rusqlite::params![first_id],
    )
    .unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Union title",
        "Union body",
        Some("beta"),
        None,
        1_700_000_002,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .arg("--dry-run")
        .assert()
        .success();

    let mem = Connection::open(&mem_path).unwrap();
    let tags: String = mem
        .query_row("SELECT tags FROM notes", [], |r| r.get(0))
        .unwrap();
    assert!(
        !tags.contains("beta"),
        "--dry-run must not merge tags; found {tags:?}"
    );
}

#[test]
fn json_counts_partition_the_source_rows() {
    // The summary must account for every source row exactly once:
    // candidates == already_present + collapsed_duplicates + imported.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "partition-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    // Pass 1: one row, imported — it becomes the "already present" copy.
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Stored",
        "stored body",
        None,
        None,
        1_700_000_001,
        "active",
        None,
    );
    drop(conn);
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(count_memory_notes(&mem_path), 1);

    // Pass 2: the stored row again (already_present=1), plus two rows sharing
    // one entity_id (imported=1, collapsed_duplicates=1). 4 candidates total.
    let conn = Connection::open(&server_db).unwrap();
    for created_at in [1_700_000_010_i64, 1_700_000_011] {
        insert_server_note(
            &conn,
            project_id,
            "decision",
            "Twin",
            "twin body",
            None,
            None,
            created_at,
            "active",
            None,
        );
    }
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Fresh",
        "fresh body",
        None,
        None,
        1_700_000_012,
        "active",
        None,
    );
    drop(conn);

    let out = reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_slice(&out).expect("summary line must be valid JSON");

    let candidates = v["candidates"].as_i64().expect("candidates");
    let already = v["already_present"].as_i64().expect("already_present");
    let collapsed = v["collapsed_duplicates"]
        .as_i64()
        .expect("collapsed_duplicates");
    let imported = v["imported"].as_i64().expect("imported");

    assert_eq!(candidates, 4, "4 source rows: {v}");
    assert_eq!(already, 1, "the stored row is already present: {v}");
    assert_eq!(collapsed, 1, "the twin pair folds one row away: {v}");
    assert_eq!(imported, 2, "Twin (collapsed) and Fresh import: {v}");
    assert_eq!(
        candidates,
        already + collapsed + imported,
        "counts must partition the source rows exactly: {v}"
    );
    assert_eq!(count_memory_notes(&mem_path), 3, "Stored + Twin + Fresh");
}

#[test]
fn tag_reorder_does_not_reimport() {
    // Tags are excluded from the key outright (they were formerly sorted and
    // hashed), so reordering them cannot produce a second copy.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "normalize-tags";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    // Unsorted tags in server.db.
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Tag normalization test",
        "body",
        Some("beta, alpha"),
        None,
        1_700_000_200,
        "active",
        None,
    );
    drop(conn);

    // Import once.
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(count_memory_notes(&mem_path), 1);

    // Update the server note to have sorted tags (same logical content).
    let conn = Connection::open(&server_db).unwrap();
    conn.execute(
        "UPDATE notes SET tags = 'alpha,beta' WHERE project_id = ?1",
        rusqlite::params![project_id],
    )
    .unwrap();
    drop(conn);

    // Second run: same entity_id — no re-import.
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(
        count_memory_notes(&mem_path),
        1,
        "reordering tags must not re-import"
    );
}

// ── AC-3: Read-only guarantee on server.db ────────────────────────────────────

#[test]
fn server_db_not_modified_after_reconcile() {
    // Criteria #3: after reconcile, server.db must contain exactly the same
    // notes as before (no writes occurred).
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    let slug = "readonly-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "requirement",
        "Server must not be written",
        "body",
        None,
        None,
        1_700_000_300,
        "active",
        None,
    );
    drop(conn);

    // Record server.db note count before.
    let count_before: i64 = {
        let conn = Connection::open(&server_db).unwrap();
        conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
            .unwrap()
    };

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    // Record server.db note count after.
    let count_after: i64 = {
        let conn = Connection::open(&server_db).unwrap();
        conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
            .unwrap()
    };

    assert_eq!(
        count_before, count_after,
        "server.db note count must not change after reconcile"
    );
}

#[test]
fn server_db_opened_read_only_flag() {
    // Criteria #3: verify that reconcile opens server.db with SQLITE_OPEN_READ_ONLY
    // by making server.db read-only at the filesystem level and confirming the
    // import still succeeds (the write target is memory.db, not server.db).
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "readonly-flag-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Read-only open",
        "body",
        None,
        None,
        1_700_000_400,
        "active",
        None,
    );
    drop(conn);

    // Make server.db read-only at the filesystem level.
    let mut perms = std::fs::metadata(&server_db).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&server_db, perms).unwrap();

    // Reconcile should still succeed because server.db is opened read-only.
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    // Restore permissions so the temp dir cleanup can remove the file.
    // Use PermissionsExt to avoid the clippy::permissions_set_readonly_false lint.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o644);
        std::fs::set_permissions(&server_db, perms).unwrap();
    }
    #[cfg(not(unix))]
    {
        let mut perms = std::fs::metadata(&server_db).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&server_db, perms).unwrap();
    }

    // The import should have succeeded.
    assert_eq!(count_memory_notes(&mem_path), 1);
}

// ── AC-4: Archived rows stay archived ────────────────────────────────────────

#[test]
fn archived_rows_import_as_archived() {
    // Criteria #4: a note with status='archived' in server.db must land in
    // memory.db with status='archived'.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "archived-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Old approach - archived",
        "This approach was superseded.",
        None,
        None,
        1_700_000_500,
        "archived", // status in server.db
        None,
    );
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Current approach - active",
        "This approach is current.",
        None,
        None,
        1_700_000_501,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    let notes = read_memory_notes(&mem_path);
    assert_eq!(notes.len(), 2, "both notes should be imported");

    let archived: Vec<_> = notes
        .iter()
        .filter(|(_, title, _)| title.contains("archived"))
        .collect();
    assert_eq!(archived.len(), 1, "exactly one archived note expected");
    assert_eq!(
        archived[0].2, "archived",
        "archived note must have status='archived' in memory.db"
    );

    let active: Vec<_> = notes
        .iter()
        .filter(|(_, title, _)| title.contains("active"))
        .collect();
    assert_eq!(
        active[0].2, "active",
        "active note must have status='active'"
    );
}

// ── AC-5: --dry-run flag ──────────────────────────────────────────────────────

#[test]
fn dry_run_does_not_write_to_memory_db() {
    // Criteria #5: --dry-run must report would_import > 0 but write nothing
    // to memory.db.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "dryrun-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Dry run note",
        "This note should not be written.",
        None,
        None,
        1_700_000_600,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .arg("--dry-run")
        .assert()
        .success();

    // memory.db either doesn't exist or has no notes.
    let written = if mem_path.exists() {
        count_memory_notes(&mem_path)
    } else {
        0
    };
    assert_eq!(
        written, 0,
        "--dry-run must not write any notes to memory.db"
    );
}

#[test]
fn dry_run_json_reports_would_import() {
    // Criteria #5: --dry-run --format json must emit a summary with
    // would_import > 0 and imported == 0.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    let slug = "dryrun-json-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Would-import note",
        "body",
        None,
        None,
        1_700_000_700,
        "active",
        None,
    );
    drop(conn);

    let output = reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
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

    assert!(
        value["would_import"].as_i64().unwrap_or(0) > 0,
        "would_import must be positive in dry-run mode: {value}"
    );
    assert_eq!(
        value["imported"].as_i64(),
        Some(0),
        "imported must be 0 in dry-run mode: {value}"
    );
}

#[test]
fn dry_run_on_empty_server_db_exits_zero() {
    // Criteria #5 + #8: --dry-run with nothing to import exits 0.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    // server.db exists but has no notes.
    let (server_db, _project_id) = create_server_db(tmp.path(), "empty-slug");

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .arg("--dry-run")
        .assert()
        .success(); // exit 0
}

// ── AC-7: Mid-run rollback on import failure ──────────────────────────────────

#[test]
fn rollback_on_mid_transaction_failure_leaves_no_partial_import() {
    // Criteria #7: if the batch import transaction fails mid-way, no rows
    // should persist in memory.db.
    //
    // Strategy:
    //  1. Bootstrap memory.db via a preliminary reconcile so schema is in place.
    //  2. Reset memory.db to empty, then install a BEFORE INSERT trigger that
    //     raises ABORT when title = 'Rollback note 2' (the 3rd note in batch).
    //  3. Run reconcile - the 3rd insert fails, the BEGIN IMMEDIATE transaction
    //     is rolled back, and the table stays empty.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "rollback-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    // Put 3 notes in server.db.
    let conn = Connection::open(&server_db).unwrap();
    for i in 0..3i64 {
        insert_server_note(
            &conn,
            project_id,
            "note",
            &format!("Rollback note {i}"),
            "body",
            None,
            None,
            1_700_001_000 + i,
            "active",
            None,
        );
    }
    drop(conn);

    // Bootstrap memory.db: import one unrelated note from an isolated server.db
    // so the full schema (including note_embeddings vec0 virtual table) is
    // created.  We use a SEPARATE subdirectory to avoid overwriting the
    // rollback-test server.db (create_server_db always writes "server.db" in
    // the given dir).
    {
        let boot_dir = tmp.path().join("boot");
        std::fs::create_dir_all(&boot_dir).unwrap();
        let boot_config_path = boot_dir.join("config.toml");
        let boot_db_path = boot_dir.join("spelunk.db");
        std::fs::write(
            &boot_config_path,
            format!(
                "db_path = {:?}\nllm_model = \"test-model\"\n",
                boot_db_path.display().to_string()
            ),
        )
        .unwrap();
        // Write the boot config's mem_path to the SAME mem_path as the main test.
        // We do this by pointing boot_config's db_path to the same parent dir as
        // main config, so memory.db resolves to tmp.path()/memory.db.
        let boot_config_content = format!(
            "db_path = {:?}\nllm_model = \"test-model\"\n",
            tmp.path().join("spelunk.db").display().to_string()
        );
        std::fs::write(&boot_config_path, boot_config_content).unwrap();

        let (bootstrap_db, boot_pid) = create_server_db(&boot_dir, "boot-for-rollback");
        let bc = Connection::open(&bootstrap_db).unwrap();
        insert_server_note(
            &bc,
            boot_pid,
            "note",
            "Bootstrap note",
            "body",
            None,
            None,
            1_699_000_000,
            "active",
            None,
        );
        drop(bc);

        // Bootstrap reconcile: run from boot_dir so config resolves correctly.
        // Use --all-projects (only boot-for-rollback slug exists in bootstrap_db).
        let mut boot_cmd = spelunk_bin();
        boot_cmd
            .current_dir(&boot_dir)
            .env("SPELUNK_NO_SERVER", "1")
            .env("SPELUNK_NO_RECONCILE_NUDGE", "1")
            .arg("--config")
            .arg(&boot_config_path)
            .arg("memory")
            .arg("reconcile")
            .arg("--source-db")
            .arg(&bootstrap_db)
            .arg("--all-projects")
            .assert()
            .success();

        // Confirm memory.db exists now (at tmp.path()/memory.db).
        assert!(
            mem_path.exists(),
            "memory.db must exist after bootstrap reconcile"
        );
    }

    // Reset memory.db: delete the bootstrap note and install a sabotage trigger
    // that rejects the 3rd "Rollback note" to force a mid-transaction failure.
    {
        ensure_sqlite_vec();
        let mc = Connection::open(&mem_path).unwrap();
        // Remove the bootstrap note.
        mc.execute("DELETE FROM notes WHERE title = 'Bootstrap note'", [])
            .unwrap();
        // Install trigger: reject the 3rd note (title = 'Rollback note 2').
        mc.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS sabotage_third_note \
             BEFORE INSERT ON notes \
             WHEN NEW.title = 'Rollback note 2' \
             BEGIN \
               SELECT RAISE(ABORT, 'sabotage: reject third note'); \
             END;",
        )
        .expect("install sabotage trigger");
    }

    // reconcile must fail because the 3rd insert is rejected mid-transaction.
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .failure(); // non-zero exit per AC-8

    // The transaction was rolled back: memory.db still has 0 notes.
    assert_eq!(
        count_memory_notes(&mem_path),
        0,
        "all inserts must be rolled back when mid-transaction failure occurs"
    );
}

// ── AC-8: Exit codes ─────────────────────────────────────────────────────────

#[test]
fn exit_0_on_success_import() {
    // Criteria #8: exit 0 when rows are successfully imported.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    let slug = "exit-0-import";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Exit code note",
        "body",
        None,
        None,
        1_700_002_000,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success(); // exit 0
}

#[test]
fn exit_0_on_noop_already_imported() {
    // Criteria #8: exit 0 when all rows are already present (nothing to import).
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    let slug = "exit-0-noop";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Existing note",
        "body",
        None,
        None,
        1_700_002_100,
        "active",
        None,
    );
    drop(conn);

    // First import.
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    // Second import - should be a no-op, still exit 0.
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
}

#[test]
fn exit_0_on_no_rows_to_import() {
    // Criteria #8: exit 0 when server.db exists but has no notes (empty project).
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    // server.db with a project but no notes.
    let (server_db, _project_id) = create_server_db(tmp.path(), "empty-project");

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success(); // exit 0
}

#[test]
fn exit_nonzero_on_corrupt_server_db() {
    // Criteria #8: exit non-zero when server.db is present but corrupt / not a
    // valid SQLite file (real fault).
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    let corrupt_db = tmp.path().join("corrupt_server.db");
    std::fs::write(&corrupt_db, b"this is not a valid sqlite database file!!!")
        .expect("write corrupt db");

    reconcile_cmd(&config_path, &corrupt_db)
        .arg("--all-projects")
        .assert()
        .failure(); // non-zero exit on real fault
}

// ── Bonus: JSON summary shape ─────────────────────────────────────────────────

#[test]
fn json_summary_contains_expected_fields() {
    // Verify the NDJSON summary object has all documented fields.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    let slug = "json-fields-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "JSON fields note",
        "body",
        None,
        None,
        1_700_003_000,
        "active",
        None,
    );
    drop(conn);

    let output = reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).expect("stdout must be valid JSON");

    for field in &[
        "source_db",
        "project_slug",
        "candidates",
        "already_present",
        "imported",
        "would_import",
        "imported_without_embedding",
        "skipped_archived_supersede_unresolved",
        "errors",
    ] {
        assert!(
            value.get(field).is_some(),
            "JSON summary missing field '{field}': {value}"
        );
    }
}

#[test]
fn import_increments_count_correctly() {
    // Verify the JSON summary reports accurate candidate / imported / already_present counts.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    let slug = "count-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);
    let conn = Connection::open(&server_db).unwrap();
    for i in 0..3i64 {
        insert_server_note(
            &conn,
            project_id,
            "note",
            &format!("Note {i}"),
            "body",
            None,
            None,
            1_700_004_000 + i,
            "active",
            None,
        );
    }
    drop(conn);

    // First run: import all 3.
    let output = reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v1: serde_json::Value =
        serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap();
    assert_eq!(v1["candidates"].as_i64(), Some(3));
    assert_eq!(v1["imported"].as_i64(), Some(3));
    assert_eq!(v1["already_present"].as_i64(), Some(0));

    // Second run: all already present.
    let output2 = reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v2: serde_json::Value =
        serde_json::from_str(String::from_utf8(output2).unwrap().trim()).unwrap();
    assert_eq!(v2["candidates"].as_i64(), Some(3));
    assert_eq!(v2["imported"].as_i64(), Some(0));
    assert_eq!(v2["already_present"].as_i64(), Some(3));
}

// ── Security: SQL injection payload in note content ───────────────────────────

#[test]
fn sql_injection_payload_in_body_does_not_break_import() {
    // Security: a note body containing SQL injection payload characters must
    // be stored verbatim, not alter query structure.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "sqli-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let injection_body = "'); DROP TABLE notes; --";
    let injection_title = "<script>alert('xss')</script>";
    // Tags field also contains SQL injection payload.
    let injection_tags = "tag1'; DELETE FROM notes; --";

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        injection_title,
        injection_body,
        Some(injection_tags),
        None,
        1_700_005_000,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    // Table must still exist and contain the imported note with the verbatim content.
    ensure_sqlite_vec();
    let mem_conn = Connection::open(&mem_path).unwrap();
    let (title, body): (String, String) = mem_conn
        .query_row(
            "SELECT title, body FROM notes WHERE title = ?1",
            rusqlite::params![injection_title],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("injected note must be importable and table must still exist");

    assert_eq!(title, injection_title, "title stored verbatim");
    assert_eq!(body, injection_body, "body stored verbatim");
}

// ── Regression: default source path must honor SPELUNK_STATE_DIR ────────────

/// `spelunk server start` writes `server.db` through the shared
/// `capability::spelunk_state_dir` resolver, which honors `SPELUNK_STATE_DIR`.
/// Reconcile's default source path (used whenever `--source-db` is omitted)
/// must resolve through that same function rather than reconstructing
/// `~/.local/state/spelunk/` from `dirs::home_dir()` on its own. Otherwise a
/// daemon run under a `SPELUNK_STATE_DIR` override is invisible to reconcile:
/// it hits the "server.db absent" no-op branch instead of importing.
#[test]
fn default_source_db_honors_state_dir_override() {
    let home = TempDir::new().unwrap();
    let state_override = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let db_path = project.path().join("spelunk.db");
    let (config_path, mem_path) = write_config(project.path(), &db_path);

    // Write server.db directly into the override dir, NOT under
    // `<home>/.local/state/spelunk/`.
    let (server_db, project_id) = create_server_db(state_override.path(), "override-project");
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Use SQLite for storage",
        "SQLite is the right choice because it is zero-infrastructure.",
        None,
        None,
        1_700_000_000,
        "active",
        None,
    );
    drop(conn);

    // Sanity: nothing exists under HOME's default location.
    let home_default = home
        .path()
        .join(".local")
        .join("state")
        .join("spelunk")
        .join("server.db");
    assert!(
        !home_default.exists(),
        "fixture bug: server.db must only exist under the override"
    );

    // No --source-db: exercises default_server_db_path().
    let mut cmd = spelunk_bin_in(home.path());
    cmd.current_dir(project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .env("SPELUNK_NO_RECONCILE_NUDGE", "1")
        .env("SPELUNK_STATE_DIR", state_override.path())
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("reconcile")
        .arg("--all-projects");
    cmd.assert().success();

    assert_eq!(
        count_memory_notes(&mem_path),
        1,
        "reconcile must resolve server.db through SPELUNK_STATE_DIR when --source-db is omitted"
    );
}
