// Chaos-engineering drills for the layer *this codebase* owns on top of
// SQLite: our transaction boundaries, our blake3-hash resume/skip logic, our
// multi-DB (index.db / memory.db) consistency, and our concurrent-access
// behaviour. SQLite's own WAL/fsync/B-tree durability is out of scope, so
// every drill here targets a window this codebase controls, not one SQLite
// already guarantees.
//
// Every SIGKILL below is real: the target process is a real `spelunk` child
// spawned via `Command`, parked at a specific write-window by
// `crash_test_hook::pause_at`/`storage::pause_for_crash_test` (env-gated,
// inert for every real invocation), and killed with `Child::kill()`, which
// sends `SIGKILL` on Unix. Nothing here simulates a crash by catching a panic
// or calling `std::process::exit` in-process.

mod plumbing_helpers;

use plumbing_helpers::{
    mount_health, mount_index_embed, register_sqlite_vec, write_project_server_config,
};
use rusqlite::Connection;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use tempfile::TempDir;

const MARKER_TIMEOUT: Duration = Duration::from_secs(30);

// ── Process plumbing ─────────────────────────────────────────────────────────

/// Build a `spelunk` `Command` isolated from the developer's real keychain,
/// config dir, and git identity, mirroring `plumbing_helpers::spelunk_bin_in`
/// but returning a raw `std::process::Command` so callers get full control
/// over stdio (needed to pipe stdin/stdout for the marker-then-kill protocol
/// below; `assert_cmd::Command` does not expose that).
fn spelunk_command(home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("spelunk"));
    cmd.env("SPELUNK_SECRET_STORE", "file")
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env("SPELUNK_CONFIG_DIR", home.join(".config").join("spelunk"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

/// A child parked at a crash point: stdin/stdout piped and a background
/// thread draining stdout into `stdout_so_far` (so the child never blocks on
/// a full pipe buffer after the marker line, and so a failed assertion can
/// print what the child actually said).
struct PausedChild {
    child: Child,
    stdout_so_far: std::sync::Arc<std::sync::Mutex<String>>,
}

/// Spawn `cmd` with `SPELUNK_TEST_CRASH_POINT=<point>` and block until the
/// child prints the matching `SPELUNK_TEST_CRASH_POINT_REACHED:<point>`
/// marker (see `storage::pause_for_crash_test` / `crash_test_hook::pause_at`),
/// proving it is parked exactly inside the write window under test rather
/// than merely "probably there by now". Panics loudly (with the child's
/// stdout so far) if the marker never arrives, rather than hanging or
/// silently no-opping the drill.
fn spawn_paused_at(mut cmd: Command, point: &str) -> PausedChild {
    cmd.env("SPELUNK_TEST_CRASH_POINT", point)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn spelunk");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // Drain stderr too so a chatty child can't deadlock on a full pipe while
    // we wait on the stdout marker.
    std::thread::spawn(move || {
        let mut r = BufReader::new(stderr);
        let mut line = String::new();
        while r.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });

    let buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let buf_writer = buf.clone();
    let marker = format!("SPELUNK_TEST_CRASH_POINT_REACHED:{point}");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = tx.send(false);
                    return;
                }
                Err(_) => {
                    let _ = tx.send(false);
                    return;
                }
                Ok(_) => {
                    buf_writer.lock().unwrap().push_str(&line);
                    if line.contains(&marker) {
                        let _ = tx.send(true);
                        // Keep draining afterward so the child never blocks
                        // on a full stdout pipe for the rest of its life.
                        loop {
                            line.clear();
                            match reader.read_line(&mut line) {
                                Ok(0) | Err(_) => return,
                                Ok(_) => buf_writer.lock().unwrap().push_str(&line),
                            }
                        }
                    }
                }
            }
        }
    });

    let reached = rx.recv_timeout(MARKER_TIMEOUT).unwrap_or(false);
    if !reached {
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "child never reached crash point {point:?} within {MARKER_TIMEOUT:?}; stdout so \
             far:\n{}",
            buf.lock().unwrap()
        );
    }
    PausedChild {
        child,
        stdout_so_far: buf,
    }
}

/// SIGKILL the paused child and wait for it to be reaped. Asserts it actually
/// died by signal (not a coincidental clean exit), which would otherwise mean
/// the drill never really tested a crash.
fn kill_and_reap(mut pc: PausedChild) {
    pc.child.kill().expect("SIGKILL the paused child");
    let status = pc.child.wait().expect("reap the killed child");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert!(
            status.signal().is_some(),
            "child must have died by signal, not exited on its own (status: {status:?}); stdout \
             so far:\n{}",
            pc.stdout_so_far.lock().unwrap()
        );
    }
    #[cfg(not(unix))]
    let _ = status;
}

/// Release a paused child without crashing it: write a byte to its stdin
/// (unblocking the `read` in `pause_at`/`pause_for_crash_test`) and wait for
/// a normal exit. Used by drills that need a real, held write/lock window but
/// are not themselves testing a kill (e.g. the concurrent-reader drill).
fn release_and_wait(mut pc: PausedChild) -> std::process::ExitStatus {
    {
        let stdin = pc.child.stdin.as_mut().expect("piped stdin");
        let _ = stdin.write_all(b"\n");
    }
    pc.child.wait().expect("wait for released child")
}

// ── DB assertions shared by every drill ──────────────────────────────────────

/// SQLite's own structural guarantee: never violated by a `SIGKILL` at any
/// point, since it is exactly what WAL/journal recovery on the next open
/// exists to uphold. Asserted in every drill as the baseline "reopens clean"
/// check, distinct from (and less interesting than) the product-level
/// invariants asserted alongside it.
fn assert_integrity_ok(db_path: &Path) {
    register_sqlite_vec();
    let conn = Connection::open(db_path).expect("reopen db after crash");
    let result: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "SQLite-level corruption after a crash");
}

fn file_hash(conn: &Connection, path: &str) -> Option<String> {
    conn.query_row(
        "SELECT hash FROM files WHERE path = ?1",
        rusqlite::params![path],
        |r| r.get(0),
    )
    .ok()
}

fn chunk_count_for(conn: &Connection, path: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM chunks c JOIN files f ON f.id = c.file_id WHERE f.path = ?1",
        rusqlite::params![path],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

fn all_file_paths(conn: &Connection) -> Vec<String> {
    let mut stmt = conn.prepare("SELECT path FROM files").unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn embedding_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
        .expect("query embeddings (requires register_sqlite_vec() before opening the connection)")
}

fn page_count(db_path: &Path) -> i64 {
    let conn = Connection::open(db_path).expect("open for page_count");
    conn.query_row("PRAGMA page_count", [], |r| r.get(0))
        .expect("read page_count")
}

// ── Drill 1-3: the parse-phase per-file crash window ─────────────────────────
//
// `process_text_file` (parse_phase.rs) commits the file's new blake3 hash via
// `upsert_file` *before* it deletes/inserts that file's chunks - there is no
// transaction spanning the two. A SIGKILL landed between them (pinned here by
// `crash_test_hook::pause_at("after_index_hash_write", path)`) leaves the
// file's `files.hash` already matching its on-disk content while `chunks` for
// it is empty. Drills 1-3 pin exactly that window and its consequences.

struct InterruptedFixture {
    _home: TempDir,
    project: TempDir,
    db_path: PathBuf,
}

/// Three files; the crash point targets `target.py` specifically so the
/// window is pinned regardless of the walk's (unspecified) file order. The
/// other two are asserted only for "fully present or fully absent, never
/// partial" - not for a specific order - since the walk order is not a
/// contract this suite should pin.
fn write_three_file_project(dir: &Path) {
    std::fs::write(dir.join("alpha.py"), "def alpha():\n    return 1\n").unwrap();
    std::fs::write(dir.join("target.py"), "def target():\n    return 2\n").unwrap();
    std::fs::write(dir.join("gamma.py"), "def gamma():\n    return 3\n").unwrap();
}

/// Run `spelunk index`, killing it exactly after `target.py`'s hash commits
/// and before any of its chunks do.
fn crash_mid_target_file() -> InterruptedFixture {
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    write_three_file_project(project.path());
    let db_path = project.path().join(".spelunk").join("index.db");

    let mut cmd = spelunk_command(home.path());
    cmd.current_dir(project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("index")
        .arg(".");
    let paused = spawn_paused_at(cmd, "after_index_hash_write:target.py");
    kill_and_reap(paused);

    InterruptedFixture {
        _home: home,
        project,
        db_path,
    }
}

#[test]
fn interrupted_file_hash_commits_before_its_chunks_pinning_the_real_write_ordering() {
    let f = crash_mid_target_file();
    assert_integrity_ok(&f.db_path);

    let conn = Connection::open(&f.db_path).expect("open db");
    assert!(
        file_hash(&conn, "target.py").is_some(),
        "upsert_file must have committed before the kill (that is the window under test)"
    );
    assert_eq!(
        chunk_count_for(&conn, "target.py"),
        0,
        "the kill landed before any chunk of target.py was written, so it must have none - a \
         nonzero count here would mean the crash point fired too late to test the intended \
         window"
    );

    // Every other file must be fully present or fully absent - never the same
    // half-state target.py is in. Walk order is not pinned, so both outcomes
    // are accepted per file; a partial one is not.
    for path in ["alpha.py", "gamma.py"] {
        match file_hash(&conn, path) {
            None => {} // never reached: fine, that is not the window under test
            Some(_) => assert!(
                chunk_count_for(&conn, path) > 0,
                "{path} has a committed hash but zero chunks - the same half-indexed state as \
                 target.py, on a file the crash point never targeted"
            ),
        }
    }
}

#[test]
fn plain_reindex_heals_a_hash_current_empty_chunks_file() {
    // Regression pin for the fix: `process_text_file`'s skip check
    // (parse_phase.rs) now requires "hash matches AND chunks exist for this
    // file", not just a hash match, so a plain re-index (no `--force`)
    // reprocesses target.py despite its hash already being current and
    // converges it back to indexed - the user never needs to know about
    // `--force` to recover from this crash window.
    let f = crash_mid_target_file();

    let mut cmd = spelunk_command(f._home.path());
    let out = cmd
        .current_dir(f.project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("index")
        .arg(".")
        .output()
        .expect("run plain re-index");
    assert!(
        out.status.success(),
        "a plain re-index must not itself fail: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_integrity_ok(&f.db_path);
    let conn = Connection::open(&f.db_path).expect("open db");
    assert!(
        chunk_count_for(&conn, "target.py") > 0,
        "a plain re-index (no --force) must self-heal a hash-current, zero-chunk file left \
         behind by the interrupted crash window"
    );
    for path in ["alpha.py", "gamma.py", "target.py"] {
        assert!(
            all_file_paths(&conn).contains(&path.to_string()),
            "{path} must be present after the plain re-index"
        );
    }
}

#[test]
fn plain_reindex_keeps_reprocessing_a_legitimately_empty_file_every_run() {
    // Scope check on the self-heal fix, not a bug pin: an empty file (zero
    // lines) parses to zero chunks by design (`sliding_window`'s
    // `lines.is_empty()` short-circuit, which every language falls back to
    // when tree-sitter finds no semantic nodes either) - `file_has_chunks`
    // has no way to distinguish that from the crash-window half-indexed
    // state it exists to catch, so it reprocesses this file on every plain
    // re-index. Accepted: extra parse-phase work on a file that is legitimately
    // empty, not a correctness issue, since it produces the same zero chunks
    // every time.
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    std::fs::write(
        project.path().join("normal.py"),
        "def normal():\n    return 1\n",
    )
    .unwrap();
    std::fs::write(project.path().join("empty.py"), "").unwrap();
    let db_path = project.path().join(".spelunk").join("index.db");

    let mut first = spelunk_command(home.path());
    let first_out = first
        .current_dir(project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("index")
        .arg(".")
        .output()
        .expect("first index");
    assert!(
        first_out.status.success(),
        "first index must succeed: {}",
        String::from_utf8_lossy(&first_out.stderr)
    );

    {
        let conn = Connection::open(&db_path).expect("open db");
        assert!(
            file_hash(&conn, "empty.py").is_some(),
            "an empty file is still indexed (present in `files`) with a real content hash"
        );
        assert_eq!(
            chunk_count_for(&conn, "empty.py"),
            0,
            "an empty file legitimately produces zero chunks - not a crash artifact"
        );
    }

    // The decisive check: a second plain re-index must still *reach*
    // empty.py's per-file processing (the pause point fires) rather than
    // skip it. If `file_has_chunks` somehow distinguished this case,
    // `spawn_paused_at` would time out waiting for the marker and panic.
    let mut second = spelunk_command(home.path());
    second
        .current_dir(project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("index")
        .arg(".");
    let paused = spawn_paused_at(second, "after_index_hash_write:empty.py");
    let status = release_and_wait(paused);
    assert!(
        status.success(),
        "the released second re-index must finish cleanly"
    );
}

#[test]
fn force_reindex_heals_the_interrupted_file() {
    let f = crash_mid_target_file();

    let mut cmd = spelunk_command(f._home.path());
    let out = cmd
        .current_dir(f.project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("index")
        .arg(".")
        .arg("--force")
        .output()
        .expect("run forced re-index");
    assert!(
        out.status.success(),
        "forced re-index must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_integrity_ok(&f.db_path);
    let conn = Connection::open(&f.db_path).expect("open db");
    assert!(
        chunk_count_for(&conn, "target.py") > 0,
        "--force bypasses the hash-skip check, so it must recover the interrupted file"
    );
    for path in ["alpha.py", "gamma.py", "target.py"] {
        assert!(
            all_file_paths(&conn).contains(&path.to_string()),
            "{path} must be present after a full forced re-index"
        );
    }
}

// ── Drill 4: the embed-phase crash window ────────────────────────────────────
//
// Unlike the parse-write path above, `insert_embeddings` (db.rs) commits one
// whole batch per transaction by design (ADR-070 D2), and
// `chunks_missing_embeddings` (chunks.rs) re-derives the embed queue from
// presence/absence of an `embeddings` row rather than trusting any in-memory
// state. This drill exercises that resume path end to end through the real
// CLI orchestration (parse -> embed -> a second, independent process), not
// just the single already-covered unit test that hard-exits mid-transaction
// in-process (`insert_embeddings_shaped_batch_leaves_nothing_after_a_hard_
// process_exit` in spelunk-core).

struct EmbedFixture {
    _home: TempDir,
    project: TempDir,
    db_path: PathBuf,
    server: wiremock::MockServer,
}

fn embed_fixture(rt: &tokio::runtime::Runtime) -> EmbedFixture {
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    std::fs::write(project.path().join("one.py"), "def one():\n    return 1\n").unwrap();
    std::fs::write(project.path().join("two.py"), "def two():\n    return 2\n").unwrap();
    let db_path = project.path().join(".spelunk").join("index.db");

    let server = rt.block_on(async {
        let server = wiremock::MockServer::start().await;
        mount_health(&server).await;
        mount_index_embed(&server).await;
        server
    });
    write_project_server_config(project.path(), &server.uri(), "test-org/test-project");

    EmbedFixture {
        _home: home,
        project,
        db_path,
        server,
    }
}

#[test]
fn sigkill_mid_embed_phase_resumes_exactly_the_missing_chunk() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let f = embed_fixture(&rt);

    // 2 chunks total: calibration batch 1 takes exactly 1 (CALIBRATION_BATCH_1),
    // so pausing after "after_embed_batch:1" commits leaves exactly 1 embedded
    // and 1 missing - a small, deterministic split, not an approximation.
    let mut cmd = spelunk_command(f._home.path());
    cmd.current_dir(f.project.path())
        .env("SPELUNK_MODE", "cloud_first")
        .arg("index")
        .arg(".")
        .arg("--no-summaries");
    let paused = spawn_paused_at(cmd, "after_embed_batch:1");
    kill_and_reap(paused);

    assert_integrity_ok(&f.db_path);
    {
        register_sqlite_vec();
        let conn = Connection::open(&f.db_path).expect("open db");
        assert_eq!(
            embedding_count(&conn),
            1,
            "exactly the first calibration batch must have committed before the kill"
        );
    }

    // Plain re-run: both files' hashes are already current, so parse_phase
    // skips reparsing them, but the missing-embeddings backfill union must
    // still queue the one chunk that never got embedded.
    let mut cmd2 = spelunk_command(f._home.path());
    let out = cmd2
        .current_dir(f.project.path())
        .env("SPELUNK_MODE", "cloud_first")
        .arg("index")
        .arg(".")
        .arg("--no-summaries")
        .output()
        .expect("run resume index");
    assert!(
        out.status.success(),
        "resume run must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    register_sqlite_vec();
    let conn = Connection::open(&f.db_path).expect("reopen db");
    assert_eq!(
        embedding_count(&conn),
        2,
        "the resume run must have embedded exactly the missing chunk, reaching full coverage"
    );
    let distinct: i64 = conn
        .query_row("SELECT COUNT(DISTINCT chunk_id) FROM embeddings", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        distinct, 2,
        "no chunk may have been embedded twice (insert_embeddings uses delete-then-insert per \
         chunk_id, so a duplicate here would mean the resume re-queued an already-embedded chunk)"
    );
    drop(f.server);
}

// ── Drill 5-6: disk-full (SQLITE_FULL) surfaces cleanly, never corrupts ──────
//
// `SPELUNK_TEST_MAX_PAGE_COUNT` caps a freshly-opened connection's
// `PRAGMA max_page_count` (see `storage::apply_test_page_cap`), which forces
// the identical `SQLITE_FULL` SQLite would raise for a real disk-full without
// needing a size-capped filesystem or a custom VFS - `max_page_count` is a
// per-connection setting SQLite does not persist to the file, so a fresh,
// uncapped process re-opening the same file afterward behaves like any real
// disk-full recovery: the earlier writer's cap is gone, not baked into the DB.

#[test]
fn disk_full_during_index_surfaces_a_clean_error_and_db_stays_valid() {
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    std::fs::write(
        project.path().join("seed.py"),
        "def seed():\n    return 0\n",
    )
    .unwrap();
    let db_path = project.path().join(".spelunk").join("index.db");

    // Uncapped baseline: establishes the schema and a small amount of data,
    // so the capped run below is growing an existing file, not failing
    // during first-open migrations (which would test migration behaviour,
    // not the index write path this drill targets).
    let mut baseline = spelunk_command(home.path());
    let out = baseline
        .current_dir(project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("index")
        .arg(".")
        .output()
        .expect("baseline index");
    assert!(out.status.success(), "baseline index must succeed");
    let baseline_pages = page_count(&db_path);

    // A lot of new content, forced to fully reparse: guarantees the write
    // volume needed to blow past a cap set just above the baseline, however
    // small the margin.
    for i in 0..40 {
        std::fs::write(
            project.path().join(format!("bulk_{i}.py")),
            format!(
                "def bulk_{i}():\n    \"\"\"{}\n    padding to grow the row.\n    \"\"\"\n    return {i}\n",
                "x".repeat(400)
            ),
        )
        .unwrap();
    }

    let mut capped = spelunk_command(home.path());
    let out = capped
        .current_dir(project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .env(
            "SPELUNK_TEST_MAX_PAGE_COUNT",
            (baseline_pages + 2).to_string(),
        )
        .arg("index")
        .arg(".")
        .arg("--force")
        .output()
        .expect("capped index");

    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        !out.status.success(),
        "a run that cannot fit its writes must not report success"
    );
    assert!(
        !stderr.contains("panicked"),
        "a full disk must surface as a returned error, never a Rust panic: {stderr}"
    );
    assert!(
        stderr.contains("full") || stderr.contains("disk"),
        "the error must name the actual condition (SQLite's own SQLITE_FULL message says \
         'database or disk is full'), not a generic failure: {stderr}"
    );

    // The cap was per-connection: a fresh, uncapped open must succeed and
    // find a structurally valid file.
    assert_integrity_ok(&db_path);
}

#[test]
fn disk_full_during_memory_add_surfaces_a_clean_error_and_note_is_not_partially_stored() {
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    let mem_db = project.path().join(".spelunk").join("memory.db");
    let config_path = project.path().join("config.toml");
    std::fs::write(
        &config_path,
        "llm_model = \"test-model\"\nstore_in_git_notes = false\n",
    )
    .unwrap();

    let memory_add =
        |home: &Path, extra_env: Option<(&str, &str)>, body: &str| -> std::process::Output {
            let mut cmd = spelunk_command(home);
            cmd.current_dir(project.path())
                .env("SPELUNK_NO_SERVER", "1")
                .env_remove("SPELUNK_SERVER_URL")
                .arg("--config")
                .arg(&config_path)
                .arg("memory")
                .arg("--db")
                .arg(&mem_db)
                .arg("add")
                .arg("--kind")
                .arg("note")
                .arg("--title")
                .arg("baseline")
                .arg("--body")
                .arg(body);
            if let Some((k, v)) = extra_env {
                cmd.env(k, v);
            }
            cmd.output().expect("run memory add")
        };

    let out = memory_add(home.path(), None, "seed note");
    assert!(
        out.status.success(),
        "baseline memory add must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let baseline_pages = page_count(&mem_db);
    let baseline_rows: i64 = {
        let conn = Connection::open(&mem_db).unwrap();
        conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
            .unwrap()
    };

    // Large enough to need far more than the +2-page cap margin below (FTS5
    // indexing alone roughly doubles the stored bytes), small enough to stay
    // under a single-process-argument command line on every CI platform:
    // Windows' CreateProcess caps the whole command line around 32KB, and
    // Linux caps a single argv entry at 128KB (32 pages) regardless of the
    // much larger total argv+envp budget.
    let huge_body = "y".repeat(20_000);
    let out = memory_add(
        home.path(),
        Some((
            "SPELUNK_TEST_MAX_PAGE_COUNT",
            &(baseline_pages + 2).to_string(),
        )),
        &huge_body,
    );

    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        !out.status.success(),
        "an add that cannot fit must not report success"
    );
    assert!(
        !stderr.contains("panicked"),
        "must surface as a returned error, never a panic: {stderr}"
    );
    assert!(
        stderr.contains("full") || stderr.contains("disk"),
        "error must name the actual condition: {stderr}"
    );

    assert_integrity_ok(&mem_db);
    let conn = Connection::open(&mem_db).unwrap();
    let rows_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        rows_after, baseline_rows,
        "a single failed INSERT must leave no partial row: SQLite's own per-statement \
         autocommit already guarantees this (unlike the multi-statement index write path above), \
         so this is the positive control confirming the cap actually exercised SQLITE_FULL \
         rather than silently no-op'ing"
    );
}

// ── Drill 7: two concurrent `spelunk index` runs on one project ─────────────
//
// Neither index.db, memory.db, nor registry.db ever sets `PRAGMA
// busy_timeout` anywhere in this codebase (confirmed by reading `Database::
// open`, `MemoryStore::open`, and `Registry::init`), so a second writer that
// arrives while another holds the WAL write lock gets `SQLITE_BUSY`
// immediately, not after a retry window.
//
// CONFIRMED FINDING against unmodified code: this drill used to reproducibly
// hit real SQLite-level corruption ("database disk image is malformed",
// SQLITE_CORRUPT), not merely a busy/locked error from the losing process.
// The fix is `run_lock.rs`: a per-project cross-process advisory lock (same
// shape as `storage::git_notes::lock`) taken as the first thing `spelunk
// index` does, non-blocking. A second process that finds it held exits
// immediately with a clean "index already running" error instead of racing
// the first process's writes - it never touches the DB at all, so there is
// nothing left to corrupt. This test now pins that behaviour: no longer
// `#[ignore]`d, and re-run in a loop (not just this internal `TRIALS` count)
// during development to build confidence the fix is real, not just less
// likely to lose the race within one process run.
#[test]
fn two_concurrent_index_runs_on_one_project_do_not_corrupt_the_db() {
    const TRIALS: usize = 8;
    const FILES_PER_TRIAL: usize = 150;

    // Across all trials, at least one must have actually contended for the
    // lock (one process observed the other mid-run) - otherwise the two
    // processes just happened to run back-to-back every time and this test
    // would pass trivially without ever exercising the fix.
    let mut observed_contention = false;

    for trial in 0..TRIALS {
        let home = TempDir::new().expect("home");
        let project = TempDir::new().expect("project");
        for i in 0..FILES_PER_TRIAL {
            std::fs::write(
                project.path().join(format!("f{i}.py")),
                format!("def f{i}():\n    return {i}\n"),
            )
            .unwrap();
        }
        let db_path = project.path().join(".spelunk").join("index.db");

        let run = |home_dir: PathBuf, project_dir: PathBuf| {
            std::thread::spawn(move || {
                let mut cmd = spelunk_command(&home_dir);
                cmd.current_dir(&project_dir)
                    .env("SPELUNK_NO_SERVER", "1")
                    .arg("index")
                    .arg(".")
                    .arg("--force")
                    .arg("--no-summaries")
                    .output()
                    .expect("run concurrent index")
            })
        };

        let t1 = run(home.path().to_path_buf(), project.path().to_path_buf());
        let t2 = run(home.path().to_path_buf(), project.path().to_path_buf());
        let out1 = t1.join().expect("thread 1");
        let out2 = t2.join().expect("thread 2");

        for (label, out) in [("run 1", &out1), ("run 2", &out2)] {
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                !stderr.to_lowercase().contains("panicked"),
                "trial {trial}, {label} must never panic, whichever loses the race: {stderr}"
            );
        }

        let successes = [&out1, &out2].iter().filter(|o| o.status.success()).count();
        assert!(
            successes >= 1,
            "trial {trial}: at least one of the two concurrent runs must complete successfully: \
             run1={:?} run2={:?}",
            out1.status,
            out2.status
        );

        // A losing process must fail CLEANLY with the lock-contention error,
        // never hang, panic, or partially write before bailing.
        for (label, out) in [("run 1", &out1), ("run 2", &out2)] {
            if !out.status.success() {
                observed_contention = true;
                let stderr = String::from_utf8_lossy(&out.stderr);
                assert!(
                    stderr.contains("already running"),
                    "trial {trial}, {label}: a losing process must fail with the clean \
                     lock-contention error, not some other failure: {stderr}"
                );
            }
        }

        // The decisive assertion, now expected to hold unconditionally: the
        // index-run lock makes the two processes mutually exclusive, so
        // index.db is never corrupted regardless of how they interleaved.
        assert_integrity_ok(&db_path);

        // The winner (or both, if they never actually overlapped this trial)
        // must have fully indexed the project: a clean-error loser exits
        // before writing anything, so it can never leave the index
        // half-written from its own aborted attempt.
        let conn = Connection::open(&db_path).expect("reopen db after concurrent runs");
        let file_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .expect("count files");
        assert_eq!(
            file_count, FILES_PER_TRIAL as i64,
            "trial {trial}: the project must be fully indexed by whichever process(es) \
             actually wrote, with no half-written state from an aborted loser"
        );
    }

    assert!(
        observed_contention,
        "across {TRIALS} trials, the two concurrent runs never actually contended for the lock \
         (both always happened to run fully sequentially) - this test never exercised the \
         behaviour it is meant to pin"
    );
}

// ── Drill 8: a concurrent reader is never blocked by an open writer ─────────
//
// WAL mode's whole purpose is that a reader never contends with a writer -
// only writer-vs-writer does. This pins that guarantee for the real CLI
// paths: `spelunk search --mode text` (a pure FTS read against index.db)
// must complete cleanly while a `spelunk index` embed batch's transaction is
// genuinely open (held via `storage::pause_for_crash_test("embed_tx_open")`,
// not merely "probably in progress").

#[test]
fn concurrent_full_text_search_during_an_open_embed_transaction_never_sees_busy() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let f = embed_fixture(&rt);

    let mut cmd = spelunk_command(f._home.path());
    cmd.current_dir(f.project.path())
        .env("SPELUNK_MODE", "cloud_first")
        .arg("index")
        .arg(".")
        .arg("--no-summaries");
    let paused = spawn_paused_at(cmd, "embed_tx_open");

    let mut search_cmd = spelunk_command(f._home.path());
    let search_out = search_cmd
        .current_dir(f.project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("search")
        .arg("one")
        .arg("--mode")
        .arg("text")
        .arg("--db")
        .arg(&f.db_path)
        .arg("--no-stale-check")
        .output()
        .expect("run concurrent search");

    let search_stderr = String::from_utf8_lossy(&search_out.stderr).to_lowercase();
    assert!(
        search_out.status.success(),
        "a concurrent read must succeed while a writer transaction is open (WAL mode): {}",
        search_stderr
    );
    assert!(
        !search_stderr.contains("busy") && !search_stderr.contains("locked"),
        "a concurrent read must never surface SQLITE_BUSY to the user: {search_stderr}"
    );

    let status = release_and_wait(paused);
    assert!(status.success(), "the released indexer must finish cleanly");
    drop(f.server);
}

// ── Drill 9: run_lock.rs hardening ───────────────────────────────────────────
//
// Three properties the fix's own doc comments claim but don't yet pin with a
// real process: (1) a SIGKILLed lock holder must not wedge every future run
// on that project - there is no stale-lock detection/cleanup path in
// run_lock.rs, so this only holds if the OS itself releases the advisory
// lock on process death; (2) the lock is genuinely per-project, not
// per-machine or per-user, so two unrelated projects indexing at the same
// time must never contend; (3) the "release before spawning a continuation
// child" handoff (mod.rs) is race-free against *corruption* specifically -
// whichever process the child's own re-acquisition loses to, it must bail
// before touching the DB, never race it - even though the handoff is not
// race-free against the child's continuation work simply not happening (see
// the test below for that residual gap, documented rather than fixed here).

#[test]
fn sigkilled_lock_holder_never_wedges_a_future_index_run() {
    // `crash_mid_target_file` SIGKILLs a `spelunk index` process while it is
    // parked at "after_index_hash_write", which is well before either
    // continuation-spawn site that releases the run lock explicitly - so the
    // kill lands with the lock still held, and its file descriptor closes
    // only because the OS reaps the process, not because any in-process
    // cleanup ran.
    let f = crash_mid_target_file();

    let mut cmd = spelunk_command(f._home.path());
    let out = cmd
        .current_dir(f.project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("index")
        .arg(".")
        .output()
        .expect("run index after the lock holder was killed");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a fresh index run after the lock holder was SIGKILLed must succeed, not report the \
         lock as still held: {stderr}"
    );
    assert!(
        !stderr.contains("already running"),
        "the OS advisory lock must be released when the holder's process dies by SIGKILL - \
         there is no stale-lock cleanup path in run_lock.rs, and a killed holder must not \
         need one: {stderr}"
    );
    assert_integrity_ok(&f.db_path);
}

#[test]
fn concurrent_index_on_different_projects_is_not_blocked_by_an_unrelated_lock() {
    // A lock keyed by anything broader than the single project (e.g. a
    // shared path, or missing the project root from the key entirely) would
    // make indexing project B hang or fail while project A's run is merely
    // in progress - that would be a regression the fix must not introduce.
    let home = TempDir::new().expect("home");

    let project_a = TempDir::new().expect("project a");
    write_three_file_project(project_a.path());
    let mut cmd_a = spelunk_command(home.path());
    cmd_a
        .current_dir(project_a.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("index")
        .arg(".");
    let paused_a = spawn_paused_at(cmd_a, "after_index_hash_write:target.py");

    let project_b = TempDir::new().expect("project b");
    write_three_file_project(project_b.path());
    let mut cmd_b = spelunk_command(home.path());
    let out_b = cmd_b
        .current_dir(project_b.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("index")
        .arg(".")
        .output()
        .expect("run index on project b while project a's run is held open");

    assert!(
        out_b.status.success(),
        "indexing project B must not be blocked by project A's held lock: {}",
        String::from_utf8_lossy(&out_b.stderr)
    );

    kill_and_reap(paused_a);
}

#[test]
fn losing_child_continuation_mode_fails_clean_without_touching_the_db() {
    // Models the detach-embed / phases-3-5 handoff's continuation child
    // losing its lock re-acquisition to *some* other holder (whether that is
    // a genuinely unrelated third `spelunk index` process racing into the
    // gap between the parent's release and the child's own acquire, or - as
    // set up deterministically here - anything else holding the lock at that
    // moment). `index()` re-acquires the same per-project lock
    // unconditionally, before either `--_background-phases` or
    // `--_embed-phases` branches into real work and before `Database::open`
    // is even called, so a child that loses this race must bail before
    // touching the DB at all - never interleave writes with whoever holds it.
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    write_three_file_project(project.path());
    let db_path = project.path().join(".spelunk").join("index.db");

    let mut cmd = spelunk_command(home.path());
    cmd.current_dir(project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("index")
        .arg(".");
    let paused = spawn_paused_at(cmd, "after_index_hash_write:target.py");

    let page_count_while_held = page_count(&db_path);

    for mode_flag in ["--_background-phases", "--_embed-phases"] {
        let mut child_cmd = spelunk_command(home.path());
        let child_out = child_cmd
            .current_dir(project.path())
            .env("SPELUNK_NO_SERVER", "1")
            .arg("index")
            .arg(".")
            .arg(mode_flag)
            .output()
            .expect("run continuation-mode child while the lock is held");

        assert!(
            !child_out.status.success(),
            "a {mode_flag} child must fail while the project's lock is held by another \
             process, not proceed"
        );
        let child_stderr = String::from_utf8_lossy(&child_out.stderr);
        assert!(
            child_stderr.contains("already running"),
            "{mode_flag} must fail with the clean lock-contention error, not some other \
             failure: {child_stderr}"
        );
        assert_eq!(
            page_count(&db_path),
            page_count_while_held,
            "a {mode_flag} child that loses the lock race must never touch the db - not even \
             open it - so the page count must be identical before and after the attempt"
        );
    }

    assert_integrity_ok(&db_path);
    kill_and_reap(paused);
}

#[test]
fn parent_reports_the_handoff_honestly_when_a_third_process_wins_the_lock_race() {
    // The previous test drives the losing child directly, never the real
    // parent-releases/parent-spawns handoff, so it cannot see what the
    // *parent* tells the user. This test does: the parent must not claim
    // "embedding in the background" unless the spawned child, specifically,
    // became the run lock's recorded holder - `wait_for_holder_pid` is what
    // confirms that before the parent reports success.
    //
    // Reproduced deterministically (rather than by racing wall-clock timing)
    // the same way the test above does: pause the parent right after it
    // releases the lock and before it spawns its continuation child, let an
    // entirely separate `spelunk index` process win and hold the lock in
    // that window, then resume the parent and inspect what it told the user.
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let f = embed_fixture(&rt);

    let mut cmd = spelunk_command(f._home.path());
    cmd.current_dir(f.project.path())
        .env("SPELUNK_MODE", "cloud_first")
        .arg("index")
        .arg(".")
        .arg("--detach-embed")
        .arg("--no-summaries");
    let paused_parent = spawn_paused_at(cmd, "after_run_lock_drop:embed");
    let parent_stdout = paused_parent.stdout_so_far.clone();

    // The parent's own parse phase (upstream of the pause point above) has
    // already hashed `one.py`/`two.py`, so a third `spelunk index` run over
    // them now would see unchanged hashes and skip straight past the
    // hash-write pause point without ever hitting it. A brand new file the
    // parent never saw gives the third process something to actually hash.
    std::fs::write(
        f.project.path().join("three.py"),
        "def three():\n    return 3\n",
    )
    .expect("write third file");

    // The parent has released the lock but not yet spawned its continuation
    // child. A genuinely separate process wins it here and holds it well
    // past the child's own (bounded) confirmation window below.
    let mut third_cmd = spelunk_command(f._home.path());
    third_cmd
        .current_dir(f.project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("index")
        .arg(".");
    let paused_third = spawn_paused_at(third_cmd, "after_index_hash_write:three.py");

    let status = release_and_wait(paused_parent);
    assert!(
        status.success(),
        "the parent must still exit cleanly even though its handoff was raced away"
    );

    let stdout = parent_stdout.lock().unwrap().clone();
    assert!(
        !stdout.contains("in the background"),
        "must not claim embedding is proceeding in the background when the spawned child never \
         confirmed it took over the lock: {stdout}"
    );
    assert!(
        stdout.contains("claimed this project's lock"),
        "must tell the user why the background handoff could not be confirmed: {stdout}"
    );
    assert!(
        stdout.contains("Run `spelunk index` again"),
        "must give the user a concrete recovery step rather than leaving the chunks silently \
         unembedded forever: {stdout}"
    );

    kill_and_reap(paused_third);
    assert_integrity_ok(&f.db_path);
}
