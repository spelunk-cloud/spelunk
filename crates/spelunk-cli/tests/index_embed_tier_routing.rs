// Regression tests for `spelunk index`'s primary embed phase local-vs-remote
// tier routing: the foreground embed phase (`index/mod.rs`'s phase 2) and
// the `--detach-embed` worker it can hand off to.
//
// Mirrors the loopback-vs-explicit-`server_url` routing bug already fixed
// for `spelunk explore` / `memory add` / `memory reindex` et al: under the
// default `local_first` mode, inference must always prefer the local
// loopback embedder, even when an explicit (here, deliberately unroutable)
// `server_url` is configured. `cloud_first` is the one mode where an
// explicit `server_url` legitimately serves inference too (test 2 is a
// regression guard for that path).
//
// The mock loopback server is wired in via `SPELUNK_STATE_DIR`/`server.port`
// (real auto-discovery), not `server_url`, so a routing regression surfaces
// as a genuine connection/DNS failure against the deliberately-unroutable
// `server_url` rather than a silently-passing test.

mod plumbing_helpers;
use plumbing_helpers::{FIXTURE_PROJECT_ID, mount_health, mount_index_embed, spelunk_bin_in};

use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Failsafe only: hit solely if the detached child never finishes.
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);

// ── fixture project ───────────────────────────────────────────────────────

// A tiny project: enough source for a couple of chunks, so the embed phase
// has real work without slowing the suite down.
fn write_project(dir: &Path) {
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("create src dir");
    std::fs::write(
        src.join("lib.rs"),
        "pub fn greet(name: &str) -> String {\n    format!(\"hello, {name}\")\n}\n\
         pub fn farewell(name: &str) -> String {\n    format!(\"bye, {name}\")\n}\n",
    )
    .expect("write lib.rs");
}

// Write `<project_dir>/.spelunk/config.toml` with `server_url` + `project_id`.
//
// `ProjectConfig` (`spelunk-core/src/config/mod.rs`) only deserializes
// `server_url`/`project_id`/`server_ca`/`index` from this file; any other
// key (notably `mode`) is silently dropped by serde. `mode` must go through
// `SPELUNK_MODE` (or the personal global `--config` file) instead.
fn write_server_config(project_dir: &Path, server_url: &str) {
    let spelunk_dir = project_dir.join(".spelunk");
    std::fs::create_dir_all(&spelunk_dir).expect("create .spelunk dir");
    let cfg = format!("server_url = {server_url:?}\nproject_id = {FIXTURE_PROJECT_ID:?}\n");
    std::fs::write(spelunk_dir.join("config.toml"), cfg).expect("write project config");
}

// Point loopback auto-discovery (`SPELUNK_STATE_DIR`/`server.port`, step 3a
// of `capability::probe`) at `url`.
fn write_loopback_state(state_dir: &Path, url: &str) {
    std::fs::create_dir_all(state_dir).expect("create state dir");
    let port: u16 = url
        .rsplit(':')
        .next()
        .expect("uri has a port")
        .trim_end_matches('/')
        .parse()
        .expect("uri port is numeric");
    std::fs::write(state_dir.join("server.port"), format!("{port}\n")).expect("write server.port");
}

// `GET /v1/health` reporting an embedder still `loading` (no `index.embed`
// capability advertised yet).
async fn mount_health_loading(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory"],
            "embedder": { "state": "loading", "detail": null },
        })))
        .mount(server)
        .await;
}

// Build a `spelunk index --db <db> .` command against `project`, defensively
// scrubbed of every `SPELUNK_*` env var these tests care about isolating
// (an ambient value in the developer/CI shell must never leak into the
// child and quietly change which tier gets probed). Callers add back
// exactly the env each scenario needs.
fn index_cmd(home: &Path, project: &Path, db: &Path) -> assert_cmd::Command {
    let mut cmd = spelunk_bin_in(home);
    cmd.current_dir(project)
        .env_remove("SPELUNK_SERVER_URL")
        .env_remove("SPELUNK_MODE")
        .env_remove("SPELUNK_PROJECT_ID")
        .env_remove("SPELUNK_NO_SERVER")
        .env_remove("SPELUNK_STATE_DIR")
        .arg("index")
        .arg("--db")
        .arg(db)
        .arg(".");
    cmd
}

fn ensure_sqlite_vec() {
    use std::sync::OnceLock;
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

fn count_embeddings(db_path: &Path) -> i64 {
    ensure_sqlite_vec();
    let conn = rusqlite::Connection::open(db_path).expect("open db");
    conn.query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
        .expect("count embeddings")
}

fn count_chunks(db_path: &Path) -> i64 {
    ensure_sqlite_vec();
    let conn = rusqlite::Connection::open(db_path).expect("open db");
    conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
        .expect("count chunks")
}

fn wait_for_embeddings(db_path: &Path) -> i64 {
    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        if db_path.exists() {
            let n = count_embeddings(db_path);
            if n > 0 {
                return n;
            }
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for embeddings to land in {db_path:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ── foreground embed phase (mod.rs's phase 2) ────────────────────────────

// Test 1 (the routing bug): `local_first` (default) with an explicit
// unroutable `server_url` and a loopback mock present must embed via the
// loopback mock, never attempt the unroutable `server_url`.
#[tokio::test]
async fn local_first_foreground_embeds_via_loopback_not_unroutable_server_url() {
    let loopback = MockServer::start().await;
    mount_health(&loopback).await;
    mount_index_embed(&loopback).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    // Deliberately unroutable: local_first must never fall back to this. An
    // accidental fallback surfaces as a connection/DNS error, not a silent
    // unembedded index.
    write_server_config(project.path(), "https://cloud.invalid.example:1");
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    let db = project.path().join("index.db");
    index_cmd(home.path(), project.path(), &db)
        .env("SPELUNK_STATE_DIR", &state_dir)
        .assert()
        .success();

    assert!(
        count_embeddings(&db) > 0,
        "local_first must embed via the loopback mock, not skip because the \
         unreachable explicit server_url was probed instead"
    );
}

// Test 2 (regression guard): `cloud_first` with an explicit `server_url`
// that DOES advertise `index.embed` must still route embedding to and
// succeed against that `server_url`, unchanged by this fix.
#[tokio::test]
async fn cloud_first_foreground_still_embeds_via_explicit_server_url() {
    let mock = MockServer::start().await;
    mount_health(&mock).await;
    mount_index_embed(&mock).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    write_server_config(project.path(), &mock.uri());
    // `mode` is not a recognized `.spelunk/config.toml` project-level field
    // (see `write_server_config`); set it via env so `cloud_first` actually
    // takes effect, rather than silently falling back to `local_first`.
    let state_dir = home.path().join("state"); // never written to: no server.port

    let db = project.path().join("index.db");
    index_cmd(home.path(), project.path(), &db)
        .env("SPELUNK_MODE", "cloud_first")
        // Defensive: an isolated, empty state dir means any accidental
        // fallback to `local_first`'s loopback probe fails loudly (nothing
        // listens on the default port from this dir), instead of silently
        // hitting a real spelunk-server daemon that happens to be running on
        // this machine's default port 7777.
        .env("SPELUNK_STATE_DIR", &state_dir)
        .assert()
        .success();

    assert!(
        count_embeddings(&db) > 0,
        "cloud_first must still embed via the explicit server_url"
    );
    let requests = mock.received_requests().await.expect("requests recorded");
    assert!(
        requests
            .iter()
            .any(|r| r.url.path().contains("/index/embed")),
        "the configured server_url must have actually been used for embedding; got: {:?}",
        requests
            .iter()
            .map(|r| (r.method.to_string(), r.url.path().to_string()))
            .collect::<Vec<_>>()
    );
}

// Test 3 (unaffected): no `server_url` configured at all (pure loopback
// auto-discovery, the default no-team-server case) must embed via loopback
// exactly as before this fix.
#[tokio::test]
async fn no_server_url_configured_embeds_via_loopback_auto_discovery() {
    let loopback = MockServer::start().await;
    mount_health(&loopback).await;
    mount_index_embed(&loopback).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    // No `.spelunk/config.toml` at all: no server_url, no project_id.
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    let db = project.path().join("index.db");
    index_cmd(home.path(), project.path(), &db)
        .env("SPELUNK_STATE_DIR", &state_dir)
        .assert()
        .success();

    assert!(
        count_embeddings(&db) > 0,
        "a project with no server_url at all must still embed via loopback \
         auto-discovery, unaffected by this fix"
    );
}

// Test 4 (unchanged): explicit offline (`SPELUNK_NO_SERVER=1`) skips the
// embed phase with the existing differentiated notice; no server is
// contacted.
#[tokio::test]
async fn explicit_offline_skips_embed_phase_with_no_server_configured() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());

    let db = project.path().join("index.db");
    let assert = index_cmd(home.path(), project.path(), &db)
        .env("SPELUNK_NO_SERVER", "1")
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    assert!(
        stderr.contains("spelunk server start"),
        "explicit offline must still print the no-server skip notice: {stderr}"
    );
    assert_eq!(
        count_embeddings(&db),
        0,
        "explicit offline must never embed"
    );
    assert!(
        count_chunks(&db) > 0,
        "chunks must still be indexed for text/ast-grep search"
    );
}

// Test 5 (unchanged): a loopback server present but with the embedder still
// `loading` at index time keeps the existing "still loading, skipped"
// notice for the foreground path.
#[tokio::test]
async fn loopback_embedder_loading_skips_foreground_embed_with_warmup_notice() {
    let loopback = MockServer::start().await;
    mount_health_loading(&loopback).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    let db = project.path().join("index.db");
    let assert = index_cmd(home.path(), project.path(), &db)
        .env("SPELUNK_STATE_DIR", &state_dir)
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    assert!(
        stderr.contains("warming up"),
        "a loading embedder must print the warm-up notice: {stderr}"
    );
    assert_eq!(
        count_embeddings(&db),
        0,
        "a loading embedder must not be embedded against"
    );
}

// ── detached-worker path (--detach-embed) ─────────────────────────────────

// Test 6 (the routing bug, detached path): the same scenario as test 1, but
// through `--detach-embed`/`--_embed-phases`: the detached worker must poll
// and embed via the loopback mock, not the explicit unroutable `server_url`.
#[tokio::test]
async fn local_first_detached_embed_routes_to_loopback_not_unroutable_server_url() {
    let loopback = MockServer::start().await;
    mount_health(&loopback).await;
    mount_index_embed(&loopback).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    write_server_config(project.path(), "https://cloud.invalid.example:1");
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    let db = project.path().join("index.db");
    index_cmd(home.path(), project.path(), &db)
        .env("SPELUNK_STATE_DIR", &state_dir)
        .arg("--detach-embed")
        .assert()
        .success();

    let n = wait_for_embeddings(&db);
    assert!(
        n > 0,
        "the detached worker must embed via the loopback mock, not skip \
         because the unreachable explicit server_url was polled instead"
    );
}
