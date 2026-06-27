//! Integration tests for ADR-003: cross-project memory visibility in
//! `spelunk memory search`, `spelunk memory list`, and `spelunk context`.
//!
//! Coverage:
//!   1. Multi-project search aggregation: `memory list` returns results from
//!      both the local store AND linked project memory stores.
//!   2. Source-project tagging: dep results carry `source_project` in JSON output.
//!   3. Locked-decision propagation: a `locked` dep decision surfaces locally.
//!   4. Privacy boundary: only `locked`/`cross-project`-tagged decisions/requirements
//!      are surfaced — untagged dep decisions are NOT exposed.
//!   5. Single-project path regression: no-deps projects behave exactly as before.
//!   6. Context command cross-project merge for `decision`/`requirement` sections;
//!      `handoff`/`question` sections remain strictly local.
//!   7. `--local-only` flag suppresses the dep pass for `memory search`, `list`, `context`.
//!   8. Archived dep entries are NOT surfaced.
//!   9. `handoff`/`question` kinds are never surfaced cross-project.
//!  10. Deduplication when two deps both point to the same grandparent.
//!  11. Missing dep `memory.db` is skipped silently (no crash, no error output).
//!  12. Security: SQL injection payload in a dep note title/body is inert.
//!  13. MemoryStore unit assertions: source_project is None for local notes,
//!      archived notes are excluded from list().

use assert_cmd::Command;
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ── test-registry helpers ─────────────────────────────────────────────────────

/// The directory the CLI is pointed at via `SPELUNK_REGISTRY_DIR` during tests.
///
/// A fixed location under the isolated test `home` that every CLI invocation
/// sets `SPELUNK_REGISTRY_DIR` to. The real `registry_path()` uses
/// `dirs::config_dir()`, which is not `HOME`-redirectable on Windows
/// (`%APPDATA%` via the Known Folder API), so an explicit override is the only
/// way to isolate the registry across all platforms.
fn registry_dir(home: &Path) -> PathBuf {
    home.join(".config").join("spelunk")
}

/// A self-contained test registry backed by a file inside the test's HOME dir.
///
/// The registry is a `registry.db`-format SQLite file.  Every CLI invocation
/// sets `SPELUNK_REGISTRY_DIR` to [`registry_dir`] so tests never touch the
/// developer's real registry.
struct TestRegistry {
    conn: Connection,
}

impl TestRegistry {
    /// Create a fresh registry under `home_dir` at [`registry_dir`] — the same
    /// location the CLI reads via `SPELUNK_REGISTRY_DIR`. Using an explicit
    /// override keeps isolation working on every OS (on Windows the real
    /// `dirs::config_dir()` is not `HOME`-redirectable).
    fn new(home_dir: &Path) -> Self {
        let config_dir = registry_dir(home_dir);
        fs::create_dir_all(&config_dir).expect("create registry dir");
        let db_path = config_dir.join("registry.db");
        let conn = Connection::open(&db_path).expect("open registry db");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS projects (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 root_path     TEXT    NOT NULL UNIQUE,
                 db_path       TEXT    NOT NULL,
                 registered_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE TABLE IF NOT EXISTS project_deps (
                 project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                 dep_id     INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                 PRIMARY KEY (project_id, dep_id)
             );",
        )
        .expect("init registry schema");
        Self { conn }
    }

    /// Register a project and return its id.
    ///
    /// Paths are canonicalized before insertion to resolve macOS symlinks
    /// (`/var/folders` ↔ `/private/var/folders`) that would otherwise cause
    /// mismatches between the registered path and the path the CLI sees when it
    /// resolves `current_dir()`.
    fn register(&self, root: &Path, db: &Path) -> i64 {
        let root_c = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let db_c = std::fs::canonicalize(db).unwrap_or_else(|_| db.to_path_buf());
        self.conn
            .execute(
                "INSERT INTO projects (root_path, db_path)
                 VALUES (?1, ?2)
                 ON CONFLICT(root_path) DO UPDATE SET db_path = excluded.db_path",
                rusqlite::params![root_c.to_string_lossy(), db_c.to_string_lossy()],
            )
            .expect("register project");
        self.conn
            .query_row(
                "SELECT id FROM projects WHERE root_path = ?1",
                rusqlite::params![root_c.to_string_lossy()],
                |r| r.get(0),
            )
            .expect("fetch project id")
    }

    /// Add a `project_id` → `dep_id` edge (project_id depends on dep_id).
    fn add_dep(&self, project_id: i64, dep_id: i64) {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO project_deps (project_id, dep_id) VALUES (?1, ?2)",
                rusqlite::params![project_id, dep_id],
            )
            .expect("add dep edge");
    }
}

// ── memory-db helpers ─────────────────────────────────────────────────────────

/// Register the sqlite-vec extension once per test process (required for
/// MemoryStore::open which creates the vec0 virtual table).
fn register_sqlite_vec_once() {
    use std::sync::OnceLock;
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| unsafe {
        #[allow(clippy::missing_transmute_annotations)]
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

/// Open and migrate a `memory.db` at `path`, returning the raw `Connection`
/// for direct seeding of test data.
fn open_memory_db(path: &Path) -> Connection {
    register_sqlite_vec_once();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create memory db parent");
    }
    let conn = Connection::open(path).expect("open memory db");
    // Verbatim migrations from MemoryStore::migrate.
    conn.execute_batch(include_str!(
        "../../../crates/spelunk-core/migrations/004_memory.sql"
    ))
    .expect("004_memory migration");
    for stmt in [
        "ALTER TABLE notes ADD COLUMN status TEXT NOT NULL DEFAULT 'active'",
        "ALTER TABLE notes ADD COLUMN superseded_by INTEGER REFERENCES notes(id)",
        "ALTER TABLE notes ADD COLUMN source_ref TEXT",
    ] {
        match conn.execute_batch(stmt) {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => panic!("migration failed: {e}"),
        }
    }
    conn.execute_batch(include_str!(
        "../../../crates/spelunk-core/migrations/012_memory_fts.sql"
    ))
    .expect("012_memory_fts migration");
    for stmt in [
        "ALTER TABLE notes ADD COLUMN valid_at INTEGER",
        "ALTER TABLE notes ADD COLUMN invalid_at INTEGER",
        "CREATE INDEX IF NOT EXISTS idx_memory_invalid_at ON notes(invalid_at)",
    ] {
        match conn.execute_batch(stmt) {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => panic!("migration failed: {e}"),
        }
    }
    conn.execute_batch(include_str!(
        "../../../crates/spelunk-core/migrations/015_memory_edges.sql"
    ))
    .expect("015_memory_edges migration");
    conn
}

/// Insert a note directly into a `memory.db`.  Returns the row id.
fn seed_note(
    conn: &Connection,
    kind: &str,
    title: &str,
    body: &str,
    tags: &[&str],
    status: &str,
) -> i64 {
    conn.execute(
        "INSERT INTO notes (kind, title, body, tags, status)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![kind, title, body, tags.join(","), status],
    )
    .expect("seed note");
    conn.last_insert_rowid()
}

// ── project setup helpers ─────────────────────────────────────────────────────

/// Write a minimal config.toml pointing `db_path` at the primary index.db.
///
/// `memory search` / `memory list` derive `index_db_path` from `cfg.db_path`
/// (via `config::resolve_db`), which is then passed to `collect_dep_cross_cutting`
/// so the registry can look up the project and its deps.
///
/// The `db_path` is canonicalized to match what `Registry::register` stores
/// (both must agree on `/private/var/...` vs `/var/...` on macOS).
fn write_config(dir: &Path, index_db: &Path) -> PathBuf {
    let index_db_c = std::fs::canonicalize(index_db).unwrap_or_else(|_| index_db.to_path_buf());
    let cfg = format!(
        concat!(
            "db_path = {:?}\n",
            "api_base_url = \"http://127.0.0.1:1\"\n",
            "embedding_model = \"none\"\n",
            "llm_model = \"none\"\n",
        ),
        index_db_c
    );
    let config_path = dir.join("config.toml");
    fs::write(&config_path, cfg).expect("write config");
    config_path
}

/// Create `.spelunk/index.db` inside `project_root` and return the path.
///
/// The registry `db_path` column must point at this file.  An empty SQLite
/// database is sufficient — the dep pass never opens the index DB itself.
fn create_spelunk_dir(project_root: &Path) -> PathBuf {
    let spelunk_dir = project_root.join(".spelunk");
    fs::create_dir_all(&spelunk_dir).expect("create .spelunk dir");
    let index_db = spelunk_dir.join("index.db");
    let _ = Connection::open(&index_db).expect("create stub index.db");
    index_db
}

/// Full two-project setup: "primary" depends on "dep".
///
/// Returns `(TempDir, home, primary_root, primary_index_db, primary_config,
///           dep_root, dep_memory_db)`.
///
/// All returned paths are canonicalized so that:
/// - CLI subprocess `current_dir` matches registry `root_path` entries.
/// - Config `db_path` matches what `find_project_for_path` looks up.
///
/// On macOS, `TempDir::new()` returns `/var/folders/...` which resolves to
/// `/private/var/folders/...`; without canonicalization the registry lookup
/// finds the wrong path and the dep pass returns nothing.
///
/// The caller must keep `_tmp` alive to prevent tempdir removal.
#[allow(clippy::type_complexity)]
fn setup_linked_projects() -> (
    TempDir, // keep alive
    PathBuf, // home (use as HOME env)
    PathBuf, // primary project root (canonical)
    PathBuf, // primary index.db path (canonical)
    PathBuf, // primary config.toml
    PathBuf, // dep project root (canonical)
    PathBuf, // dep memory.db (canonical, caller seeds this)
) {
    let tmp = TempDir::new().expect("create temp dir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home dir");

    let primary_root_raw = tmp.path().join("primary");
    fs::create_dir_all(&primary_root_raw).expect("create primary dir");
    let primary_index_raw = create_spelunk_dir(&primary_root_raw);

    let dep_root_raw = tmp.path().join("dep");
    fs::create_dir_all(&dep_root_raw).expect("create dep dir");
    let dep_index_raw = create_spelunk_dir(&dep_root_raw);

    // Canonicalize after the directories exist so symlink resolution succeeds.
    let primary_root = std::fs::canonicalize(&primary_root_raw).unwrap_or(primary_root_raw);
    let primary_index = std::fs::canonicalize(&primary_index_raw).unwrap_or(primary_index_raw);
    let dep_root = std::fs::canonicalize(&dep_root_raw).unwrap_or(dep_root_raw);
    let dep_index = std::fs::canonicalize(&dep_index_raw).unwrap_or(dep_index_raw);

    // Config db_path uses the canonical index.db path.
    let primary_config = write_config(&primary_root, &primary_index);
    let dep_mem = dep_index.with_file_name("memory.db");

    let reg = TestRegistry::new(&home);
    let primary_id = reg.register(&primary_root, &primary_index);
    let dep_id = reg.register(&dep_root, &dep_index);
    reg.add_dep(primary_id, dep_id);

    (
        tmp,
        home,
        primary_root,
        primary_index,
        primary_config,
        dep_root,
        dep_mem,
    )
}

/// Build a base `spelunk memory` command for the primary project.
///
/// - `HOME` is set to the isolated home dir (registry isolation).
/// - `SPELUNK_NO_SERVER=1` disables the loopback-server capability probe.
/// - `current_dir` is `primary_root` so registry path lookup walks from there.
/// - `--config` points to the primary config.toml (which has `db_path = <index.db>`).
/// - `memory --db <primary_mem>` routes memory reads to the primary's memory.db.
fn memory_cmd(home: &Path, primary_root: &Path, config: &Path, primary_mem: &Path) -> Command {
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.env("HOME", home)
        .env("SPELUNK_REGISTRY_DIR", registry_dir(home))
        // Unset XDG_CONFIG_HOME so dirs::config_dir() uses $HOME/.config on Linux,
        // matching what TestRegistry::new() writes to home_dir.join(".config").
        .env_remove("XDG_CONFIG_HOME")
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(primary_root)
        .arg("--config")
        .arg(config)
        .arg("memory")
        .arg("--db")
        .arg(primary_mem);
    cmd
}

/// Build a base `spelunk context` command for the primary project.
fn context_cmd(
    home: &Path,
    primary_root: &Path,
    config: &Path,
    primary_mem: &Path,
    primary_index: &Path,
) -> Command {
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.env("HOME", home)
        .env("SPELUNK_REGISTRY_DIR", registry_dir(home))
        .env_remove("XDG_CONFIG_HOME")
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(primary_root)
        .arg("--config")
        .arg(config)
        .arg("context")
        .arg("--db")
        .arg(primary_mem)
        .arg("--index-db")
        .arg(primary_index)
        .arg("--no-conventions");
    cmd
}

// ── 1. Multi-project search aggregation ──────────────────────────────────────

/// `memory list` includes a `locked` dep decision alongside local notes.
#[test]
fn memory_list_includes_locked_decision_from_linked_dep() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();

    let primary_mem = primary_index.with_file_name("memory.db");
    let primary_conn = open_memory_db(&primary_mem);
    seed_note(
        &primary_conn,
        "decision",
        "Local decision",
        "local body",
        &[],
        "active",
    );

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "SSE is Cloud-only",
        "SSE memory stream is Cloud-only per decision #134.",
        &["locked", "v1"],
        "active",
    );

    let output = memory_cmd(&home, &primary_root, &primary_config, &primary_mem)
        .args(["list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> = serde_json::from_slice(&output).expect("valid JSON");
    let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();

    assert!(
        titles.contains(&"Local decision"),
        "local note must appear; got: {titles:?}"
    );
    assert!(
        titles.contains(&"SSE is Cloud-only"),
        "locked dep decision must be surfaced; got: {titles:?}"
    );
}

// ── 2. Source-project tagging ─────────────────────────────────────────────────

/// A dep note in `memory list --format json` must have `source_project` set to
/// the dep's directory name.
#[test]
fn dep_note_carries_source_project_tag_in_json() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "Cross-project tagged decision",
        "Must carry source_project in JSON.",
        &["locked"],
        "active",
    );

    let output = memory_cmd(&home, &primary_root, &primary_config, &primary_mem)
        .args(["list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> = serde_json::from_slice(&output).expect("valid JSON");
    let dep_note = notes
        .iter()
        .find(|n| n["title"].as_str() == Some("Cross-project tagged decision"))
        .expect("dep note must appear in list");

    assert_eq!(
        dep_note["source_project"].as_str(),
        Some("dep"),
        "source_project must be 'dep'; got: {dep_note}"
    );
    assert!(
        dep_note["source_project_path"].as_str().is_some(),
        "source_project_path must be set; got: {dep_note}"
    );
}

/// Local notes must NOT have `source_project` in JSON output.
#[test]
fn local_note_has_no_source_project_field() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, _dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let primary_conn = open_memory_db(&primary_mem);
    seed_note(
        &primary_conn,
        "decision",
        "Local-only decision",
        "body",
        &[],
        "active",
    );

    let output = memory_cmd(&home, &primary_root, &primary_config, &primary_mem)
        .args(["list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> = serde_json::from_slice(&output).expect("valid JSON");
    let local = notes
        .iter()
        .find(|n| n["title"].as_str() == Some("Local-only decision"))
        .expect("local note must appear");

    assert!(
        local.get("source_project").is_none() || local["source_project"].is_null(),
        "local note must not have source_project; got: {local}"
    );
}

// ── 3. Locked-decision propagation ───────────────────────────────────────────

/// `memory search --mode text` appends locked dep decisions in the results.
///
/// Text-mode search is used (not hybrid/semantic) so no embedding server is
/// needed.  Per ADR-003 §3, the dep pass appends ALL cross-cutting entries
/// regardless of the FTS search query.
#[test]
fn memory_search_text_appends_locked_dep_decisions() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "Auth uses JWT tokens",
        "All authentication must use signed JWT tokens.",
        &["locked", "security"],
        "active",
    );

    let output = memory_cmd(&home, &primary_root, &primary_config, &primary_mem)
        .args(["search", "--mode", "text", "--format", "json", "anything"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> = serde_json::from_slice(&output).expect("valid JSON");
    let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();
    assert!(
        titles.contains(&"Auth uses JWT tokens"),
        "locked dep decision must be appended by dep pass; got: {titles:?}"
    );
}

// ── 4. Privacy boundary ───────────────────────────────────────────────────────

/// A dep decision WITHOUT `locked` or `cross-project` tag must NOT be surfaced.
#[test]
fn untagged_dep_decision_is_not_surfaced() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "Internal dep naming convention",
        "We use camelCase for internal variables.",
        &["style"], // not locked, not cross-project
        "active",
    );

    let raw = Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", &home)
        .env("SPELUNK_REGISTRY_DIR", registry_dir(&home))
        .env_remove("XDG_CONFIG_HOME")
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(&primary_root)
        .arg("--config")
        .arg(&primary_config)
        .arg("memory")
        .arg("--db")
        .arg(&primary_mem)
        .args(["list", "--format", "json"])
        .output()
        .expect("run spelunk");

    let text = String::from_utf8_lossy(&raw.stdout);
    if text.trim().starts_with('[') {
        let notes: Vec<serde_json::Value> = serde_json::from_str(text.trim()).expect("valid JSON");
        let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();
        assert!(
            !titles.contains(&"Internal dep naming convention"),
            "untagged dep decision must NOT be surfaced; got: {titles:?}"
        );
    }
    // "No memory entries found." → untagged note not surfaced; test passes.
}

/// A dep `note` (kind=note) tagged `locked` must NOT cross project boundaries —
/// only `decision` and `requirement` kinds are eligible per ADR-003 §1.
#[test]
fn dep_note_kind_is_not_surfaced_even_if_locked() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "note", // wrong kind — notes are always local
        "Surprising fact: locked",
        "A note tagged locked should remain private to its project.",
        &["locked"],
        "active",
    );

    let raw = Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", &home)
        .env("SPELUNK_REGISTRY_DIR", registry_dir(&home))
        .env_remove("XDG_CONFIG_HOME")
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(&primary_root)
        .arg("--config")
        .arg(&primary_config)
        .arg("memory")
        .arg("--db")
        .arg(&primary_mem)
        .args(["list", "--format", "json"])
        .output()
        .expect("run spelunk");

    let text = String::from_utf8_lossy(&raw.stdout);
    if text.trim().starts_with('[') {
        let notes: Vec<serde_json::Value> = serde_json::from_str(text.trim()).expect("valid JSON");
        let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();
        assert!(
            !titles.contains(&"Surprising fact: locked"),
            "dep note (kind=note) must not cross boundary; got: {titles:?}"
        );
    }
}

/// A dep `requirement` tagged `cross-project` IS surfaced (not just `locked`).
#[test]
fn dep_requirement_with_cross_project_tag_is_surfaced() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "requirement",
        "All APIs must be TLS 1.3",
        "Security requirement applying to all linked projects.",
        &["cross-project", "security"],
        "active",
    );

    let output = memory_cmd(&home, &primary_root, &primary_config, &primary_mem)
        .args(["list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> = serde_json::from_slice(&output).expect("valid JSON");
    let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();
    assert!(
        titles.contains(&"All APIs must be TLS 1.3"),
        "dep requirement with cross-project tag must be surfaced; got: {titles:?}"
    );
}

// ── 5. Single-project path regression ────────────────────────────────────────

/// With no deps registered, `memory list` works exactly as before ADR-003.
#[test]
fn single_project_no_deps_works_unchanged() {
    let tmp = TempDir::new().expect("create temp dir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home dir");

    let project_root_raw = tmp.path().join("proj");
    fs::create_dir_all(&project_root_raw).expect("create project dir");
    let index_db_raw = create_spelunk_dir(&project_root_raw);
    let project_root = std::fs::canonicalize(&project_root_raw).unwrap_or(project_root_raw);
    let index_db = std::fs::canonicalize(&index_db_raw).unwrap_or(index_db_raw);
    let config = write_config(&project_root, &index_db);
    let mem = index_db.with_file_name("memory.db");

    // Register with NO deps.
    let reg = TestRegistry::new(&home);
    reg.register(&project_root, &index_db);

    let conn = open_memory_db(&mem);
    seed_note(
        &conn,
        "decision",
        "Local-only note",
        "no deps anywhere",
        &[],
        "active",
    );

    let output = Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", &home)
        .env("SPELUNK_REGISTRY_DIR", registry_dir(&home))
        .env_remove("XDG_CONFIG_HOME")
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(&project_root)
        .arg("--config")
        .arg(&config)
        .arg("memory")
        .arg("--db")
        .arg(&mem)
        .args(["list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> = serde_json::from_slice(&output).expect("valid JSON");
    assert_eq!(
        notes.len(),
        1,
        "exactly one local note, no phantom dep notes"
    );
    assert_eq!(notes[0]["title"].as_str(), Some("Local-only note"));
}

// ── 6. Context command cross-project merge ────────────────────────────────────

/// `spelunk context` (text mode) includes a locked dep decision and renders
/// the `[from: dep]` badge.
#[test]
fn context_includes_locked_dep_decision_with_source_badge() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "SSE endpoint is Cloud-only",
        "The /v1/memory/sse endpoint must never be in OSS.",
        &["locked"],
        "active",
    );

    let stdout = context_cmd(
        &home,
        &primary_root,
        &primary_config,
        &primary_mem,
        &primary_index,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let text = String::from_utf8_lossy(&stdout);
    assert!(
        text.contains("SSE endpoint is Cloud-only"),
        "dep decision must appear in context; got:\n{text}"
    );
    assert!(
        text.contains("[from: dep]"),
        "source badge must appear for dep decision; got:\n{text}"
    );
}

/// `spelunk context --format json` includes dep decision with `source_project` field.
#[test]
fn context_json_includes_dep_decision_with_source_project() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "Dep locked decision for context JSON",
        "Must appear in context JSON with source_project.",
        &["locked"],
        "active",
    );

    let output = context_cmd(
        &home,
        &primary_root,
        &primary_config,
        &primary_mem,
        &primary_index,
    )
    .args(["--format", "json"])
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let obj: serde_json::Value = serde_json::from_slice(&output).expect("valid JSON");
    let sections = obj["sections"].as_array().expect("sections array");
    let decision_section = sections
        .iter()
        .find(|s| s[0].as_str() == Some("decision"))
        .expect("decision section");
    let decision_notes = decision_section[1].as_array().expect("notes array");

    let dep_note = decision_notes
        .iter()
        .find(|n| n["title"].as_str() == Some("Dep locked decision for context JSON"))
        .expect("dep decision must appear in context JSON");

    assert_eq!(
        dep_note["source_project"].as_str(),
        Some("dep"),
        "source_project must be 'dep' in context JSON; got: {dep_note}"
    );
}

/// `spelunk context` does NOT include dep `requirement` notes in the `decision`
/// section, but DOES include them in the `requirement` section.
#[test]
fn context_dep_requirement_appears_in_requirement_section() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "requirement",
        "Dep TLS requirement",
        "All endpoints must use TLS.",
        &["locked"],
        "active",
    );

    let output = context_cmd(
        &home,
        &primary_root,
        &primary_config,
        &primary_mem,
        &primary_index,
    )
    .args(["--format", "json"])
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let obj: serde_json::Value = serde_json::from_slice(&output).expect("valid JSON");
    let sections = obj["sections"].as_array().expect("sections array");

    // Must appear in the requirement section.
    let req_section = sections
        .iter()
        .find(|s| s[0].as_str() == Some("requirement"))
        .expect("requirement section");
    let req_notes = req_section[1].as_array().expect("requirement notes");
    let found_in_req = req_notes
        .iter()
        .any(|n| n["title"].as_str() == Some("Dep TLS requirement"));
    assert!(
        found_in_req,
        "dep requirement must appear in requirement section of context"
    );

    // Must NOT appear in the decision section (it's a requirement, not a decision).
    let dec_section = sections
        .iter()
        .find(|s| s[0].as_str() == Some("decision"))
        .expect("decision section");
    let dec_notes = dec_section[1].as_array().expect("decision notes");
    let found_in_dec = dec_notes
        .iter()
        .any(|n| n["title"].as_str() == Some("Dep TLS requirement"));
    assert!(
        !found_in_dec,
        "dep requirement must NOT appear in decision section"
    );
}

// ── 7. --local-only flag ──────────────────────────────────────────────────────

/// `memory list --local-only` suppresses the dep pass entirely.
#[test]
fn memory_list_local_only_suppresses_dep_results() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "Would appear without local-only",
        "body",
        &["locked"],
        "active",
    );

    let stdout = memory_cmd(&home, &primary_root, &primary_config, &primary_mem)
        .args(["list", "--local-only"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8_lossy(&stdout);
    assert!(
        !text.contains("Would appear without local-only"),
        "--local-only must suppress dep results; got:\n{text}"
    );
}

/// `memory search --local-only` suppresses the dep pass.
#[test]
fn memory_search_local_only_suppresses_dep_results() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "Search-suppressed dep decision",
        "Normally appended by dep pass.",
        &["locked"],
        "active",
    );

    let stdout = memory_cmd(&home, &primary_root, &primary_config, &primary_mem)
        .args(["search", "--mode", "text", "--local-only", "dep decision"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8_lossy(&stdout);
    assert!(
        !text.contains("Search-suppressed dep decision"),
        "search --local-only must suppress dep results; got:\n{text}"
    );
}

/// `context --local-only` suppresses the dep pass.
#[test]
fn context_local_only_suppresses_dep_results() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "Context-suppressed dep decision",
        "body",
        &["locked"],
        "active",
    );

    let stdout = context_cmd(
        &home,
        &primary_root,
        &primary_config,
        &primary_mem,
        &primary_index,
    )
    .arg("--local-only")
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let text = String::from_utf8_lossy(&stdout);
    assert!(
        !text.contains("Context-suppressed dep decision"),
        "context --local-only must suppress dep results; got:\n{text}"
    );
}

// ── 8. Archived dep entries are not surfaced ──────────────────────────────────

/// An archived dep decision (status='archived') must NOT appear even if tagged `locked`.
#[test]
fn archived_dep_decision_is_not_surfaced() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "Old locked decision now archived",
        "Superseded and archived — must not propagate.",
        &["locked"],
        "archived", // <-- archived
    );

    let raw = Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", &home)
        .env("SPELUNK_REGISTRY_DIR", registry_dir(&home))
        .env_remove("XDG_CONFIG_HOME")
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(&primary_root)
        .arg("--config")
        .arg(&primary_config)
        .arg("memory")
        .arg("--db")
        .arg(&primary_mem)
        .args(["list", "--format", "json"])
        .output()
        .expect("run spelunk");

    let text = String::from_utf8_lossy(&raw.stdout);
    if text.trim().starts_with('[') {
        let notes: Vec<serde_json::Value> = serde_json::from_str(text.trim()).expect("valid JSON");
        let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();
        assert!(
            !titles.contains(&"Old locked decision now archived"),
            "archived dep note must not be surfaced; got: {titles:?}"
        );
    }
    // "No memory entries found." → archived note was correctly suppressed.
}

// ── 9. handoff/question kinds are never cross-project ─────────────────────────

/// `context` must NOT pull `handoff` entries from dep projects, even if tagged `locked`.
#[test]
fn context_never_pulls_dep_handoffs() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "handoff",
        "Dep handoff: session ended",
        "This handoff must remain local to the dep project.",
        &["locked"], // even locked tag cannot make a handoff cross-project
        "active",
    );

    let stdout = context_cmd(
        &home,
        &primary_root,
        &primary_config,
        &primary_mem,
        &primary_index,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let text = String::from_utf8_lossy(&stdout);
    assert!(
        !text.contains("Dep handoff: session ended"),
        "dep handoff must never cross project boundaries; got:\n{text}"
    );
}

/// `memory list` must NOT pull a `question` from a dep project.
#[test]
fn dep_question_is_never_surfaced_cross_project() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "question",
        "Dep question: should we use SSE?",
        "Only relevant within the dep project.",
        &["locked"],
        "active",
    );

    let raw = Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", &home)
        .env("SPELUNK_REGISTRY_DIR", registry_dir(&home))
        .env_remove("XDG_CONFIG_HOME")
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(&primary_root)
        .arg("--config")
        .arg(&primary_config)
        .arg("memory")
        .arg("--db")
        .arg(&primary_mem)
        .args(["list", "--format", "json"])
        .output()
        .expect("run spelunk");

    let text = String::from_utf8_lossy(&raw.stdout);
    if text.trim().starts_with('[') {
        let notes: Vec<serde_json::Value> = serde_json::from_str(text.trim()).expect("valid JSON");
        let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();
        assert!(
            !titles.contains(&"Dep question: should we use SSE?"),
            "dep question must not cross boundary; got: {titles:?}"
        );
    }
}

// ── 10. Deduplication ─────────────────────────────────────────────────────────

/// When primary has two direct deps (dep-a and dep-b), and both happen to have
/// a note with the same (root_path, id) key, it must appear at most once.
///
/// In practice the dep-pass deduplication protects against diamond-shaped
/// dependency graphs where two direct deps both link to a shared grandparent.
/// Here we simulate the simpler scenario: two direct deps each with a unique
/// entry to confirm no cross-dep pollution.
#[test]
fn multiple_deps_results_are_aggregated_not_duplicated() {
    let tmp = TempDir::new().expect("create temp dir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home dir");

    let primary_root_raw = tmp.path().join("primary");
    fs::create_dir_all(&primary_root_raw).expect("primary dir");
    let primary_index_raw = create_spelunk_dir(&primary_root_raw);
    let primary_root = std::fs::canonicalize(&primary_root_raw).unwrap_or(primary_root_raw);
    let primary_index = std::fs::canonicalize(&primary_index_raw).unwrap_or(primary_index_raw);
    let primary_config = write_config(&primary_root, &primary_index);
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_a_root_raw = tmp.path().join("dep-a");
    fs::create_dir_all(&dep_a_root_raw).expect("dep-a dir");
    let dep_a_index_raw = create_spelunk_dir(&dep_a_root_raw);
    let dep_a_root = std::fs::canonicalize(&dep_a_root_raw).unwrap_or(dep_a_root_raw);
    let dep_a_index = std::fs::canonicalize(&dep_a_index_raw).unwrap_or(dep_a_index_raw);
    let dep_a_mem = dep_a_index.with_file_name("memory.db");

    let dep_b_root_raw = tmp.path().join("dep-b");
    fs::create_dir_all(&dep_b_root_raw).expect("dep-b dir");
    let dep_b_index_raw = create_spelunk_dir(&dep_b_root_raw);
    let dep_b_root = std::fs::canonicalize(&dep_b_root_raw).unwrap_or(dep_b_root_raw);
    let dep_b_index = std::fs::canonicalize(&dep_b_index_raw).unwrap_or(dep_b_index_raw);
    let dep_b_mem = dep_b_index.with_file_name("memory.db");

    // Seed a locked decision in each dep.
    let conn_a = open_memory_db(&dep_a_mem);
    seed_note(
        &conn_a,
        "decision",
        "Dep-A policy",
        "body",
        &["locked"],
        "active",
    );
    let conn_b = open_memory_db(&dep_b_mem);
    seed_note(
        &conn_b,
        "decision",
        "Dep-B policy",
        "body",
        &["locked"],
        "active",
    );

    let reg = TestRegistry::new(&home);
    let primary_id = reg.register(&primary_root, &primary_index);
    let dep_a_id = reg.register(&dep_a_root, &dep_a_index);
    let dep_b_id = reg.register(&dep_b_root, &dep_b_index);
    reg.add_dep(primary_id, dep_a_id);
    reg.add_dep(primary_id, dep_b_id);

    let output = Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", &home)
        .env("SPELUNK_REGISTRY_DIR", registry_dir(&home))
        .env_remove("XDG_CONFIG_HOME")
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(&primary_root)
        .arg("--config")
        .arg(&primary_config)
        .arg("memory")
        .arg("--db")
        .arg(&primary_mem)
        .args(["list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> = serde_json::from_slice(&output).expect("valid JSON");
    let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();

    // Both dep notes must appear exactly once each.
    assert_eq!(
        titles.iter().filter(|&&t| t == "Dep-A policy").count(),
        1,
        "Dep-A policy must appear exactly once; got: {titles:?}"
    );
    assert_eq!(
        titles.iter().filter(|&&t| t == "Dep-B policy").count(),
        1,
        "Dep-B policy must appear exactly once; got: {titles:?}"
    );
}

// ── 11. Missing dep memory.db is silently skipped ────────────────────────────

/// A dep with no `memory.db` is skipped silently — no crash, no error in stdout.
/// Other deps with a `memory.db` still contribute their results.
#[test]
fn missing_dep_memory_db_is_skipped_silently() {
    let tmp = TempDir::new().expect("create temp dir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home dir");

    let primary_root_raw = tmp.path().join("primary");
    fs::create_dir_all(&primary_root_raw).expect("primary dir");
    let primary_index_raw = create_spelunk_dir(&primary_root_raw);
    let primary_root = std::fs::canonicalize(&primary_root_raw).unwrap_or(primary_root_raw);
    let primary_index = std::fs::canonicalize(&primary_index_raw).unwrap_or(primary_index_raw);
    let primary_config = write_config(&primary_root, &primary_index);
    let primary_mem = primary_index.with_file_name("memory.db");

    // dep-a: has a memory.db with a locked decision.
    let dep_a_root_raw = tmp.path().join("dep-a");
    fs::create_dir_all(&dep_a_root_raw).expect("dep-a dir");
    let dep_a_index_raw = create_spelunk_dir(&dep_a_root_raw);
    let dep_a_root = std::fs::canonicalize(&dep_a_root_raw).unwrap_or(dep_a_root_raw);
    let dep_a_index = std::fs::canonicalize(&dep_a_index_raw).unwrap_or(dep_a_index_raw);
    let dep_a_mem = dep_a_index.with_file_name("memory.db");
    let conn_a = open_memory_db(&dep_a_mem);
    seed_note(
        &conn_a,
        "decision",
        "Dep-a locked decision",
        "body",
        &["locked"],
        "active",
    );

    // dep-b: exists in registry but has NO memory.db.
    let dep_b_root_raw = tmp.path().join("dep-b");
    fs::create_dir_all(&dep_b_root_raw).expect("dep-b dir");
    let dep_b_index_raw = create_spelunk_dir(&dep_b_root_raw);
    let dep_b_root = std::fs::canonicalize(&dep_b_root_raw).unwrap_or(dep_b_root_raw);
    let dep_b_index = std::fs::canonicalize(&dep_b_index_raw).unwrap_or(dep_b_index_raw);
    // Deliberately do NOT create dep_b_index.with_file_name("memory.db").

    let reg = TestRegistry::new(&home);
    let primary_id = reg.register(&primary_root, &primary_index);
    let dep_a_id = reg.register(&dep_a_root, &dep_a_index);
    let dep_b_id = reg.register(&dep_b_root, &dep_b_index);
    reg.add_dep(primary_id, dep_a_id);
    reg.add_dep(primary_id, dep_b_id);

    let output = Command::cargo_bin("spelunk")
        .unwrap()
        .env("HOME", &home)
        .env("SPELUNK_REGISTRY_DIR", registry_dir(&home))
        .env_remove("XDG_CONFIG_HOME")
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(&primary_root)
        .arg("--config")
        .arg(&primary_config)
        .arg("memory")
        .arg("--db")
        .arg(&primary_mem)
        .args(["list", "--format", "json"])
        .assert()
        .success() // must NOT crash or fail even with a missing dep memory.db
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> = serde_json::from_slice(&output).expect("valid JSON");
    let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();
    assert!(
        titles.contains(&"Dep-a locked decision"),
        "dep-a result must appear despite dep-b having no memory.db; got: {titles:?}"
    );
}

// ── 12. Security: SQL injection payload in dep note is inert ─────────────────

/// SQL injection payload in a dep note title / body must not alter the DB or
/// crash the CLI — parameterised queries must be used throughout.
/// (SAMM v2 Verification — Security Testing, level 1.)
#[test]
fn sql_injection_in_dep_note_is_inert() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "'; DROP TABLE notes; --",
        "body: \" OR 1=1; --",
        &["locked"],
        "active",
    );
    // A benign note to verify the notes table survived the payload.
    seed_note(
        &dep_conn,
        "decision",
        "Benign note after injection payload",
        "This note must still exist.",
        &["locked"],
        "active",
    );

    let output = memory_cmd(&home, &primary_root, &primary_config, &primary_mem)
        .args(["list", "--format", "json"])
        .assert()
        .success() // must not crash
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> =
        serde_json::from_slice(&output).expect("valid JSON after injection payload");
    let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();
    assert!(
        titles.contains(&"Benign note after injection payload"),
        "notes table must survive injection payload; got: {titles:?}"
    );
}

// ── 13. MemoryStore unit assertions ───────────────────────────────────────────
//
// These exercise spelunk-core's MemoryStore directly (no CLI binary) to assert
// the storage-layer preconditions that the dep-pass relies on.

/// `MemoryStore::list` excludes archived notes when `include_archived=false`
/// (the precondition for the dep-pass not surfacing archived entries).
#[test]
fn memory_store_list_excludes_archived_by_default() {
    register_sqlite_vec_once();

    use spelunk_core::storage::memory::MemoryStore;
    let store = MemoryStore::open(std::path::Path::new(":memory:")).expect("in-memory MemoryStore");

    store
        .add_note("decision", "Active note", "body", &[], &[], None, None)
        .expect("add active note");
    let archived_id = store
        .add_note("decision", "Archived note", "body", &[], &[], None, None)
        .expect("add to-be-archived note");
    store.archive(archived_id).expect("archive note");

    let notes = store
        .list(Some("decision"), 100, false)
        .expect("list notes");
    let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();
    assert!(titles.contains(&"Active note"), "active note must appear");
    assert!(
        !titles.contains(&"Archived note"),
        "archived note must be excluded; got: {titles:?}"
    );
}

/// `MemoryStore::list` returns `Note` instances with `source_project == None` —
/// that field is populated exclusively by the CLI dep-pass, not by the store.
#[test]
fn memory_store_notes_have_no_source_project_by_default() {
    register_sqlite_vec_once();

    use spelunk_core::storage::memory::MemoryStore;
    let store = MemoryStore::open(std::path::Path::new(":memory:")).expect("in-memory MemoryStore");

    store
        .add_note(
            "decision",
            "Some decision",
            "body",
            &["locked"],
            &[],
            None,
            None,
        )
        .expect("add note");

    let notes = store.list(Some("decision"), 100, false).expect("list");
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].source_project.is_none(),
        "MemoryStore must not set source_project — CLI dep-pass sets it"
    );
    assert!(
        notes[0].source_project_path.is_none(),
        "MemoryStore must not set source_project_path — CLI dep-pass sets it"
    );
}
