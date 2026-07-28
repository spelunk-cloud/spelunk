// End-to-end tests for `spelunk memory reindex` against the built CLI binary.
//
// The command re-embeds memory notes left without a local vector (the 768→896
// store upgrade dropped them, or no embedder was reachable at add time). These
// tests drive the real binary with the embed endpoint mocked via wiremock, so
// none of them depend on the in-process native embedder (the `--no-default-
// features` gate stays valid). The no-embedder case uses `SPELUNK_NO_SERVER=1`.
//
// The mock embedder is wired in via **loopback auto-discovery**
// (`SPELUNK_STATE_DIR`/`server.port`), not `server_url`, since the
// 2026-07-23 ADR-004 revision: `reindex` runs in the
// default `local_first` mode here (no explicit `mode` is set), and
// `local_first` never routes inference through an explicit `server_url` —
// only the local loopback embedder. Using the real discovery mechanism (a
// per-fixture, isolated state dir) rather than `mode = "cloud_first"` also
// sidesteps a real hazard: a `cloud_first` fixture would still fall back to
// hard-coded port 7777 if the state dir ever went unset, which could hit a
// developer's own long-running `spelunk-server` instead of this fixture's
// mock.

mod plumbing_helpers;
use plumbing_helpers::{
    FIXTURE_PROJECT_ID, mount_health, spelunk_bin, write_project_server_config,
};

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::TempDir;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── sqlite-vec registration for direct memory.db inspection ──────────────────

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

// ── mock embed endpoint (records request bodies; can inject a failure) ────────

// Custom responder for `POST /v1/projects/{id}/index/embed`. Returns the server
// wire format (raw little-endian f32 bytes, one `value`-filled 896-dim vector
// per request chunk), records each request body for parity assertions, and can
// start returning 503 once `calls >= fail_after` to simulate a mid-run outage.
#[derive(Clone)]
struct EmbedResponder {
    value: f32,
    fail_after: Option<usize>,
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
}

impl EmbedResponder {
    fn new(value: f32) -> Self {
        Self {
            value,
            fail_after: None,
            calls: Arc::new(AtomicUsize::new(0)),
            bodies: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn failing_after(value: f32, limit: usize) -> Self {
        Self {
            fail_after: Some(limit),
            ..Self::new(value)
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn recorded_bodies(&self) -> Vec<String> {
        self.bodies.lock().expect("bodies mutex").clone()
    }
}

impl wiremock::Respond for EmbedResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let idx = self.calls.fetch_add(1, Ordering::SeqCst);
        self.bodies
            .lock()
            .expect("bodies mutex")
            .push(String::from_utf8_lossy(&request.body).to_string());

        if let Some(limit) = self.fail_after
            && idx >= limit
        {
            return ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "error": "embedder unavailable",
                "state": "unavailable",
                "detail": "injected mid-run failure",
            }));
        }

        let n_chunks = serde_json::from_slice::<serde_json::Value>(&request.body)
            .ok()
            .and_then(|v| v.get("chunks").and_then(|c| c.as_array()).map(|a| a.len()))
            .unwrap_or(1);
        let mut bytes = Vec::with_capacity(n_chunks * 896 * 4);
        for _ in 0..n_chunks {
            for _ in 0..896 {
                bytes.extend_from_slice(&self.value.to_le_bytes());
            }
        }
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/octet-stream")
            .set_body_bytes(bytes)
    }
}

// A running mock spelunk-server with the health probe and embed endpoint
// mounted. `rt` and `server` are kept alive by the returned struct for the
// duration of a `.assert()` against the child process.
struct MockServerHandle {
    _rt: tokio::runtime::Runtime,
    server: MockServer,
    embed: EmbedResponder,
}

impl MockServerHandle {
    fn uri(&self) -> String {
        self.server.uri()
    }
}

fn start_mock(embed: EmbedResponder) -> MockServerHandle {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let server = rt.block_on(async {
        let server = MockServer::start().await;
        mount_health(&server).await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(embed.clone())
            .mount(&server)
            .await;
        server
    });
    MockServerHandle {
        _rt: rt,
        server,
        embed,
    }
}

// ── test project fixture ─────────────────────────────────────────────────────

struct Fixture {
    _tmp: TempDir,
    project_dir: PathBuf,
    mem_path: PathBuf,
    global_config: PathBuf,
    // Isolated `SPELUNK_STATE_DIR` for this fixture's loopback auto-discovery
    // (`server.port`, step 3a). Never the hard-coded default port 7777.
    state_dir: PathBuf,
}

// A temp project with a global `--config` (no server_url; `store_in_git_notes =
// false` so seeding never touches git notes) and a `.spelunk/` dir where memory
// lives.
fn fixture() -> Fixture {
    let tmp = TempDir::new().expect("tempdir");
    let project_dir = tmp.path().to_path_buf();
    let spelunk_dir = project_dir.join(".spelunk");
    std::fs::create_dir_all(&spelunk_dir).expect("create .spelunk");
    let mem_path = spelunk_dir.join("memory.db");
    let index_db = spelunk_dir.join("index.db");
    let global_config = project_dir.join("global-config.toml");
    std::fs::write(
        &global_config,
        format!(
            "db_path = {:?}\nstore_in_git_notes = false\nllm_model = \"test-model\"\n",
            index_db.display().to_string()
        ),
    )
    .expect("write global config");
    let state_dir = project_dir.join("state");
    std::fs::create_dir_all(&state_dir).expect("create state dir");
    Fixture {
        _tmp: tmp,
        project_dir,
        mem_path,
        global_config,
        state_dir,
    }
}

// Seed one unembedded note via the real `memory add`. `SPELUNK_NO_SERVER=1` plus
// the absence of a project server config means the add path stores no vector.
fn seed(f: &Fixture, kind: &str, title: &str, body: &str) {
    spelunk_bin()
        .current_dir(&f.project_dir)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(&f.global_config)
        .arg("memory")
        .arg("--db")
        .arg(&f.mem_path)
        .arg("add")
        .arg("--kind")
        .arg(kind)
        .arg("--title")
        .arg(title)
        .arg("--body")
        .arg(body)
        .assert()
        .success();
}

// Write `<state_dir>/server.port` so loopback auto-discovery (step 3a) finds
// the mock embedder at `url`, mirroring the file `spelunk server start`
// writes. `local_first` (the mode every test in this file runs under) never
// routes inference through `server_url`, only the local loopback embedder.
fn set_server(f: &Fixture, url: &str) {
    let port: u16 = url
        .rsplit(':')
        .next()
        .expect("uri has a port")
        .trim_end_matches('/')
        .parse()
        .expect("uri port is numeric");
    std::fs::write(f.state_dir.join("server.port"), format!("{port}\n"))
        .expect("write server.port");
}

// Remove the port file so `seed` stores notes unembedded: loopback
// auto-discovery is honored even under `SPELUNK_NO_SERVER=0` (unset), so
// seeding an unembedded note after a server has been configured requires
// clearing it. `seed`/`archive_note` set `SPELUNK_NO_SERVER=1` themselves, so
// in practice this is defensive; kept for symmetry with `set_server`.
fn clear_server(f: &Fixture) {
    let p = f.state_dir.join("server.port");
    if p.exists() {
        std::fs::remove_file(&p).expect("remove server.port");
    }
}

// Build a `spelunk memory reindex` command against the fixture.
fn reindex_cmd(f: &Fixture) -> Command {
    let mut cmd = spelunk_bin();
    cmd.current_dir(&f.project_dir)
        .env("SPELUNK_STATE_DIR", &f.state_dir)
        .arg("--config")
        .arg(&f.global_config)
        .arg("memory")
        .arg("--db")
        .arg(&f.mem_path)
        .arg("reindex");
    cmd
}

// Archive a seeded note via the real `memory archive`. Runs with no server so
// it stays a purely local status change (no git-notes carry: global config
// pins store_in_git_notes = false).
fn archive_note(f: &Fixture, id: i64) {
    spelunk_bin()
        .current_dir(&f.project_dir)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(&f.global_config)
        .arg("memory")
        .arg("--db")
        .arg(&f.mem_path)
        .arg("archive")
        .arg(id.to_string())
        .assert()
        .success();
}

// Extract the embed document string a recorded `/index/embed` body carried.
fn embed_content(body: &str) -> String {
    let sent: serde_json::Value = serde_json::from_str(body).expect("embed body is json");
    sent["chunks"][0]["content"]
        .as_str()
        .expect("chunk content string")
        .to_string()
}

// Build a genuine pre-0.9 store: a FLOAT[768] `note_embeddings` vec0 table with
// `n` notes and no v896 sentinel, exactly what an upgraded user's memory.db
// looks like before the first 0.9 open.
fn make_pre_v896_store(mem_path: &Path, n: usize) {
    ensure_sqlite_vec();
    let conn = Connection::open(mem_path).expect("open raw pre-v896 store");
    conn.execute_batch(
        "CREATE TABLE notes (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            kind          TEXT    NOT NULL DEFAULT 'note',
            title         TEXT    NOT NULL,
            body          TEXT    NOT NULL,
            tags          TEXT,
            linked_files  TEXT,
            created_at    INTEGER NOT NULL DEFAULT (unixepoch())
        );
        CREATE VIRTUAL TABLE note_embeddings USING vec0(
            note_id INTEGER PRIMARY KEY, embedding FLOAT[768]
        );",
    )
    .expect("create pre-v896 schema");
    for i in 0..n {
        conn.execute(
            "INSERT INTO notes (kind, title, body) VALUES ('note', ?1, ?2)",
            rusqlite::params![format!("t{i}"), format!("b{i}")],
        )
        .expect("seed note");
    }
}

fn note_id_by_title(mem_path: &Path, title: &str) -> i64 {
    let conn = Connection::open(mem_path).expect("open memory.db");
    conn.query_row(
        "SELECT id FROM notes WHERE title = ?1",
        rusqlite::params![title],
        |r| r.get(0),
    )
    .expect("note id by title")
}

fn embedded_note_ids(mem_path: &Path) -> Vec<i64> {
    ensure_sqlite_vec();
    let conn = Connection::open(mem_path).expect("open memory.db");
    let mut stmt = conn
        .prepare("SELECT note_id FROM note_embeddings ORDER BY note_id")
        .expect("prepare");
    stmt.query_map([], |r| r.get::<_, i64>(0))
        .expect("query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect")
}

fn embedding_blob(mem_path: &Path, note_id: i64) -> Vec<u8> {
    ensure_sqlite_vec();
    let conn = Connection::open(mem_path).expect("open memory.db");
    conn.query_row(
        "SELECT embedding FROM note_embeddings WHERE note_id = ?1",
        rusqlite::params![note_id],
        |r| r.get::<_, Vec<u8>>(0),
    )
    .expect("embedding blob")
}

// ── tests ────────────────────────────────────────────────────────────────────

// A store with missing embeddings is fully backfilled: every note gains an
// 896-dim vector, the run exits 0 and reports counts, and a second run embeds
// nothing (idempotent).
#[test]
fn reindex_embeds_missing_and_is_idempotent() {
    let f = fixture();
    seed(&f, "decision", "one", "body one");
    seed(&f, "note", "two", "body two");
    seed(&f, "requirement", "three", "body three");
    assert!(
        embedded_note_ids(&f.mem_path).is_empty(),
        "seeded notes must start unembedded"
    );

    let mock = start_mock(EmbedResponder::new(0.1));
    set_server(&f, &mock.uri());

    reindex_cmd(&f)
        .assert()
        .success()
        .stdout(predicates::str::contains("3 embedded"))
        // Progress goes to stderr so the machine summary on stdout stays clean.
        .stderr(predicates::str::contains("embedded 3/3"));

    let ids = embedded_note_ids(&f.mem_path);
    assert_eq!(ids.len(), 3, "all three notes must be embedded");
    for id in &ids {
        assert_eq!(
            embedding_blob(&f.mem_path, *id).len(),
            896 * 4,
            "each stored vector must be 896 f32 values"
        );
    }

    // Second run: nothing missing, no vectors written, exit 0.
    let before = mock.embed.call_count();
    reindex_cmd(&f)
        .assert()
        .success()
        .stdout(predicates::str::contains("Nothing to reindex"));
    assert_eq!(
        embedded_note_ids(&f.mem_path).len(),
        3,
        "idempotent: still exactly three embeddings"
    );
    assert_eq!(
        mock.embed.call_count(),
        before,
        "a fully-embedded store must make no further embed calls"
    );
}

// The embed request carries the exact add-time document string
// (`title: {t} | text: {b}`) and is NOT wrapped in the F2LLM `Instruct:/Query:`
// query prefix, so a backfilled vector matches an add-time one.
#[test]
fn reindex_embed_text_matches_add_time_document() {
    let f = fixture();
    seed(&f, "decision", "Title A", "Body B");

    let mock = start_mock(EmbedResponder::new(0.1));
    set_server(&f, &mock.uri());

    reindex_cmd(&f).assert().success();

    let bodies = mock.embed.recorded_bodies();
    assert_eq!(bodies.len(), 1, "exactly one embed call for one note");
    let sent: serde_json::Value = serde_json::from_str(&bodies[0]).expect("embed body is json");
    let content = sent["chunks"][0]["content"]
        .as_str()
        .expect("chunk content string");
    assert_eq!(
        content, "title: Title A | text: Body B",
        "reindex must embed the identical add-time document string"
    );
    assert!(
        !content.contains("Instruct:") && !content.contains("Query:"),
        "documents must not be sent with the F2LLM query instruction prefix: {content:?}"
    );
}

// An interrupted run is resumable: after embedding J of K, a re-run embeds only
// the remaining K−J with no duplicate rows and a final count of K.
#[test]
fn reindex_resumes_after_midrun_failure() {
    let f = fixture();
    for i in 0..4 {
        seed(&f, "note", &format!("n{i}"), &format!("body {i}"));
    }

    // Run 1: the embedder serves two notes then fails; reindex stops and exits
    // non-zero with two durably-committed vectors. The failure must report the
    // honest partial count (not just fail silently) and point at a re-run, so a
    // user knows work was saved and how to finish it.
    let mock_a = start_mock(EmbedResponder::failing_after(0.1, 2));
    set_server(&f, &mock_a.uri());
    reindex_cmd(&f)
        .assert()
        .failure()
        .stderr(predicates::str::contains("2 of 4 done and durably stored"))
        .stderr(predicates::str::contains("re-run 'spelunk memory reindex'"));
    assert_eq!(
        embedded_note_ids(&f.mem_path).len(),
        2,
        "the two notes embedded before the failure must be durable"
    );

    // Run 2: a healthy embedder; reindex embeds only the remaining two.
    let mock_b = start_mock(EmbedResponder::new(0.1));
    set_server(&f, &mock_b.uri());
    reindex_cmd(&f).assert().success();

    let ids = embedded_note_ids(&f.mem_path);
    assert_eq!(ids.len(), 4, "all four notes embedded after resume");
    let mut unique = ids.clone();
    unique.dedup();
    assert_eq!(unique.len(), 4, "no duplicate note_embeddings rows");
    assert_eq!(
        mock_b.embed.call_count(),
        2,
        "resume must only re-embed the two notes still missing"
    );
}

// No embedder reachable: reindex fails with the actionable inference-server
// message, exits non-zero, and writes no vectors (no partial success).
#[test]
fn reindex_without_embedder_errors_and_writes_nothing() {
    let f = fixture();
    seed(&f, "decision", "one", "body one");
    seed(&f, "note", "two", "body two");
    // No project server config set: no server_url anywhere.

    reindex_cmd(&f)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
        .assert()
        .failure()
        .stderr(predicates::str::contains("requires spelunk-server"));

    assert!(
        embedded_note_ids(&f.mem_path).is_empty(),
        "a no-embedder run must write no vectors"
    );
}

// `--force` re-embeds already-embedded notes, replacing the stored vector in
// place (no duplicate rows).
#[test]
fn reindex_force_replaces_existing_vectors() {
    let f = fixture();
    seed(&f, "decision", "only", "the body");
    let id = note_id_by_title(&f.mem_path, "only");

    // First embed with a distinguishable constant vector.
    let mock1 = start_mock(EmbedResponder::new(0.1));
    set_server(&f, &mock1.uri());
    reindex_cmd(&f).assert().success();
    let blob_before = embedding_blob(&f.mem_path, id);

    // --force with a different vector must overwrite the existing row.
    let mock2 = start_mock(EmbedResponder::new(0.5));
    set_server(&f, &mock2.uri());
    reindex_cmd(&f).arg("--force").assert().success();

    let blob_after = embedding_blob(&f.mem_path, id);
    assert_ne!(
        blob_before, blob_after,
        "--force must replace the already-embedded note's vector"
    );
    assert_eq!(
        embedded_note_ids(&f.mem_path),
        vec![id],
        "--force must replace in place, not duplicate the row"
    );
}

// The `--format json` summary partitions the store honestly: total_active ==
// embedded + already_embedded, embedded == missing_before, no count exceeds the
// total.
#[test]
fn reindex_json_summary_partitions_counts() {
    let f = fixture();
    for i in 0..3 {
        seed(&f, "note", &format!("first{i}"), &format!("b{i}"));
    }

    let mock = start_mock(EmbedResponder::new(0.1));
    set_server(&f, &mock.uri());
    // Embed the first three.
    reindex_cmd(&f).assert().success();

    // Two more unembedded notes (seeded with the server config cleared so add
    // stores no vector), then a JSON reindex over the mixed store.
    clear_server(&f);
    seed(&f, "note", "later0", "later b0");
    seed(&f, "note", "later1", "later b1");
    set_server(&f, &mock.uri());

    let output = reindex_cmd(&f)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let summary: serde_json::Value =
        serde_json::from_slice(&output).expect("json summary on stdout");

    let total_active = summary["total_active"].as_u64().unwrap();
    let missing_before = summary["missing_before"].as_u64().unwrap();
    let already_embedded = summary["already_embedded"].as_u64().unwrap();
    let embedded = summary["embedded"].as_u64().unwrap();
    let remaining = summary["remaining"].as_u64().unwrap();

    assert_eq!(total_active, 5, "five active notes total");
    assert_eq!(
        remaining, 0,
        "on full success nothing targeted is left unembedded"
    );
    assert_eq!(
        total_active,
        embedded + already_embedded,
        "counts must partition the store"
    );
    assert_eq!(
        embedded, missing_before,
        "on success everything missing before is embedded"
    );
    assert_eq!(missing_before, 2, "only the two later notes were missing");
    assert!(
        embedded <= total_active && already_embedded <= total_active,
        "no count exceeds the total"
    );
}

// `--dry-run` reports the would-embed count, contacts the embedder zero times,
// writes no vectors, and exits 0.
#[test]
fn reindex_dry_run_counts_and_writes_nothing() {
    let f = fixture();
    seed(&f, "decision", "one", "body one");
    seed(&f, "note", "two", "body two");

    let mock = start_mock(EmbedResponder::new(0.1));
    set_server(&f, &mock.uri());

    reindex_cmd(&f)
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicates::str::contains("2 note(s) would be embedded"));

    assert!(
        embedded_note_ids(&f.mem_path).is_empty(),
        "--dry-run must write no vectors"
    );
    assert_eq!(
        mock.embed.call_count(),
        0,
        "--dry-run must not contact the embedder"
    );
}

// D5(b): after the 768→896 upgrade drops old vectors, a memory command surfaces
// the one-line reindex notice once (without RUST_LOG), and the migrated notes
// are then present-but-unembedded.
#[test]
fn migration_notice_fires_once_after_768_upgrade() {
    let f = fixture();
    make_pre_v896_store(&f.mem_path, 3);

    // First memory command after the upgrade: the notice fires on stderr.
    spelunk_bin()
        .current_dir(&f.project_dir)
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&f.global_config)
        .arg("memory")
        .arg("--db")
        .arg(&f.mem_path)
        .arg("list")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "3 note(s) need re-embedding for semantic search",
        ));

    // The migrated notes are present-but-unembedded (the 768 vectors were
    // dropped), so reindex has exactly 3 to backfill.
    assert!(embedded_note_ids(&f.mem_path).is_empty());

    // The sentinel is now set: a second command must NOT repeat the notice.
    spelunk_bin()
        .current_dir(&f.project_dir)
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&f.global_config)
        .arg("memory")
        .arg("--db")
        .arg(&f.mem_path)
        .arg("list")
        .assert()
        .success()
        .stderr(predicates::str::contains("need re-embedding").not());
}

// Every note (not just the first) is embedded via its own add-time document
// string, in id order, with no F2LLM query prefix. Guards against a partial
// wrong-format bug that a single-note parity check would miss.
#[test]
fn reindex_embeds_every_note_with_its_own_document() {
    let f = fixture();
    seed(&f, "decision", "First Title", "First Body");
    seed(&f, "note", "Second Title", "Second Body");

    let mock = start_mock(EmbedResponder::new(0.1));
    set_server(&f, &mock.uri());
    reindex_cmd(&f).assert().success();

    let bodies = mock.embed.recorded_bodies();
    assert_eq!(bodies.len(), 2, "one embed call per note");
    let contents: Vec<String> = bodies.iter().map(|b| embed_content(b)).collect();
    // Candidate order is `ORDER BY note id`, which is the seed order here.
    assert_eq!(
        contents,
        vec![
            "title: First Title | text: First Body".to_string(),
            "title: Second Title | text: Second Body".to_string(),
        ],
        "each note must embed its own document, in id order"
    );
    for c in &contents {
        assert!(
            !c.contains("Instruct:") && !c.contains("Query:"),
            "no document may carry the F2LLM query prefix: {c:?}"
        );
    }
}

// `--include-archived` is load-bearing: a default run skips archived notes, and
// only `--include-archived` backfills them. Without the flag an archived note
// stays vectorless (missing from timeline semantic recall).
#[test]
fn reindex_include_archived_covers_archived_only_with_the_flag() {
    let f = fixture();
    seed(&f, "note", "active-note", "active body");
    seed(&f, "note", "archived-note", "archived body");
    let active_id = note_id_by_title(&f.mem_path, "active-note");
    let archived_id = note_id_by_title(&f.mem_path, "archived-note");
    archive_note(&f, archived_id);

    let mock = start_mock(EmbedResponder::new(0.1));
    set_server(&f, &mock.uri());

    // Default: only the active note is embedded; the archived one is skipped.
    reindex_cmd(&f).assert().success();
    assert_eq!(
        embedded_note_ids(&f.mem_path),
        vec![active_id],
        "default reindex must not touch archived notes"
    );

    // With the flag the archived note gets its vector too.
    reindex_cmd(&f).arg("--include-archived").assert().success();
    let mut ids = embedded_note_ids(&f.mem_path);
    ids.sort();
    let mut want = vec![active_id, archived_id];
    want.sort();
    assert_eq!(
        ids, want,
        "--include-archived backfills the archived note as well"
    );
}

// A store created by `memory add` is a fresh FLOAT[896] store that never went
// through the 768 drop, so no command may print the re-embed notice. Pins the
// CLI-layer negative end-to-end, not just the library flag.
#[test]
fn no_reembed_notice_on_fresh_store() {
    let f = fixture();
    seed(&f, "note", "one", "body one");

    spelunk_bin()
        .current_dir(&f.project_dir)
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&f.global_config)
        .arg("memory")
        .arg("--db")
        .arg(&f.mem_path)
        .arg("list")
        .assert()
        .success()
        .stderr(predicates::str::contains("need re-embedding").not());
}

// `cloud_first` WITH `server_url` set: `memory.db` is not the store of record
// there (`server_url` is, via `RemoteMemoryBackend`), so `reindex` has
// nothing local to re-embed. It must fail with an actionable "not
// applicable" message rather than silently no-op'ing or (worse) reindexing a
// store nothing reads (2026-07-23 founder decision). No
// mock embedder is set up at all: a real embed attempt would also fail this
// test, just for the wrong reason (proving the bail happens before any embed
// call, not after one fails).
#[test]
fn reindex_in_cloud_first_with_server_url_is_not_applicable() {
    let f = fixture();
    seed(&f, "decision", "one", "body one");
    assert!(
        embedded_note_ids(&f.mem_path).is_empty(),
        "seeded note must start unembedded"
    );
    write_project_server_config(
        &f.project_dir,
        "https://team.example.com",
        FIXTURE_PROJECT_ID,
    );

    reindex_cmd(&f)
        .env("SPELUNK_MODE", "cloud_first")
        .assert()
        .failure()
        .stderr(predicates::str::contains("not applicable in cloud_first"))
        .stderr(predicates::str::contains("memory.db"));

    assert!(
        embedded_note_ids(&f.mem_path).is_empty(),
        "a rejected cloud_first reindex must write no vectors"
    );
}

// `cloud_first` with NO `server_url` set: nothing routes memory remotely, so
// `open_memory_backend` itself falls back to `memory.db` (`storage/mod.rs`'s
// `route_remote` requires BOTH `cloud_first` AND a configured `server_url`).
// `memory.db` genuinely is the store of record here, so `reindex` must
// proceed and embed normally, not bail. Regression guard: an earlier version
// of this check gated on `mode` alone and rejected this valid case too.
#[test]
fn reindex_in_cloud_first_without_server_url_proceeds() {
    let f = fixture();
    seed(&f, "decision", "one", "body one");
    assert!(
        embedded_note_ids(&f.mem_path).is_empty(),
        "seeded note must start unembedded"
    );

    let mock = start_mock(EmbedResponder::new(0.1));
    set_server(&f, &mock.uri());

    reindex_cmd(&f)
        .env("SPELUNK_MODE", "cloud_first")
        .assert()
        .success()
        .stdout(predicates::str::contains("1 embedded"));

    let ids = embedded_note_ids(&f.mem_path);
    assert_eq!(
        ids.len(),
        1,
        "cloud_first with no server_url must still reindex the local memory.db"
    );
}
