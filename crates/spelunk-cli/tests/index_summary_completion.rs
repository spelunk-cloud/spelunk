//! `spelunk index` must run its LLM summary pass to completion before it returns.
//! A summary still in flight at process exit is silently lost; the run exits 0.
//!
//! Nothing here is timing-based: the mock `/llm/complete` cannot answer until the
//! test releases it, so "summaries finished before phase 5" holds by program order
//! on an awaited pass and cannot hold on a detached one.

mod plumbing_helpers;

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer};

/// Failsafe only: hit solely when the child never reaches the summary pass.
const ARRIVAL_TIMEOUT: Duration = Duration::from_secs(90);

/// A single-chunk project, so exactly one batch and one summary are expected.
fn write_fixture(dir: &Path, name: &str) {
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("create src dir");
    std::fs::write(
        src.join("lib.rs"),
        "pub fn clean_fn(x: i32) -> i32 {\n    x + 1\n}\n",
    )
    .expect("write lib.rs");
    std::fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    )
    .expect("write Cargo.toml");
}

/// `spelunk index` as a raw `std::process::Command` so the test can hold a live
/// `Child` across the gate; `assert_cmd`'s runner blocks until exit.
/// Mirrors `plumbing_helpers::spelunk_bin_in`'s keychain/home pinning.
///
/// `SPELUNK_MODE=cloud_first`: every test in this file drives its fixture's
/// explicit `server_url` for LLM summaries (2026-07-23 ADR-004 revision).
/// `index/summaries.rs::generate_summaries` calls
/// `ServerInferenceClient::from_config` directly on the loaded `Config` with
/// no loopback auto-discovery bridging, so under the default `local_first`
/// mode a bare `server_url` no longer resolves to any inference target at
/// all. `cloud_first` is what makes this file's premise — an explicit
/// `server_url` IS used for summaries — hold.
fn index_command(home: &Path, config: &Path, db: &Path, project: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("spelunk"));
    cmd.env("SPELUNK_SECRET_STORE", "file")
        .env("HOME", home)
        .env("SPELUNK_MODE", "cloud_first")
        .env_remove("XDG_CONFIG_HOME")
        // Project-level config discovery walks up from CWD; every caller here
        // writes `server_url` to `<project>/.spelunk/config.toml`.
        .current_dir(project)
        .arg("--config")
        .arg(config)
        .arg("index")
        .arg("--db")
        .arg(db)
        .arg(project);
    cmd
}

/// `/llm/complete` responder that reports arrival, then blocks until released.
///
/// Parking a worker here is why the runtime below is built with several.
struct GatedLlmResponder {
    arrived: Mutex<Sender<()>>,
    release: Mutex<Receiver<()>>,
    body: String,
}

impl wiremock::Respond for GatedLlmResponder {
    fn respond(&self, _req: &wiremock::Request) -> wiremock::ResponseTemplate {
        let _ = self.arrived.lock().unwrap().send(());
        let _ = self.release.lock().unwrap().recv();
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(plumbing_helpers::sse_token_response(&self.body))
    }
}

/// Drain stderr on its own thread: the child stays gated mid-run, and a full
/// pipe would block it before it could answer.
fn drain(pipe: Option<std::process::ChildStderr>) -> std::thread::JoinHandle<String> {
    let mut pipe = pipe.expect("stderr piped");
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = pipe.read_to_string(&mut s);
        s
    })
}

fn assert_summary_precedes_conventions(stderr: &str) {
    let summarised = stderr
        .find("Summarised 1 batch(es).")
        .unwrap_or_else(|| panic!("expected a completed summary batch in stderr:\n{stderr}"));
    let conventions = stderr
        .find("Extracting conventions")
        .unwrap_or_else(|| panic!("expected phase 5 to run in stderr:\n{stderr}"));
    assert!(
        summarised < conventions,
        "the summary pass must complete before phase 5 begins; a detached pass lets \
         `index` exit with summaries still in flight, losing them.\n--- stderr ---\n{stderr}"
    );
}

/// The core guard: `index` cannot return until the summary pass has finished.
#[test]
fn summary_pass_completes_before_index_returns() {
    let project = TempDir::new().expect("temp project dir");
    write_fixture(project.path(), "summary-order-fixture");
    let db_tmp = TempDir::new().expect("temp db dir");
    let db_path = db_tmp.path().join("spelunk.db");

    let (arrived_tx, arrived_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build test runtime");

    let mock_server = rt.block_on(async {
        let server = MockServer::start().await;
        plumbing_helpers::mount_health(&server).await;
        plumbing_helpers::mount_index_embed(&server).await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/llm/complete$"))
            .respond_with(GatedLlmResponder {
                arrived: Mutex::new(arrived_tx),
                release: Mutex::new(release_rx),
                body: serde_json::json!([{"id": 1, "summary": "Adds one to x."}]).to_string(),
            })
            .mount(&server)
            .await;
        server
    });

    let mock_url = mock_server.uri();
    let config_path = plumbing_helpers::write_config_with_server(
        project.path(),
        &db_path,
        &mock_url,
        &mock_url,
        project.path(),
    );

    let mut child = index_command(project.path(), &config_path, &db_path, project.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn spelunk index");
    let stderr_reader = drain(child.stderr.take());

    // Wait for the summary request to reach the mock. Exiting first is itself
    // the bug: `index` returned without the summary pass having run.
    let deadline = Instant::now() + ARRIVAL_TIMEOUT;
    loop {
        match arrived_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(()) => break,
            Err(RecvTimeoutError::Timeout) => {
                if let Some(status) = child.try_wait().expect("poll child") {
                    let _ = release_tx.send(());
                    let stderr = stderr_reader.join().expect("join stderr reader");
                    panic!(
                        "`index` exited ({status}) before the summary request reached the \
                         server: the summary pass is not awaited.\n--- stderr ---\n{stderr}"
                    );
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for the summary request to reach the mock server"
                );
            }
            Err(RecvTimeoutError::Disconnected) => panic!("mock dropped the arrival channel"),
        }
    }

    // The response is still withheld, so an awaited pass is necessarily blocked here.
    assert!(
        child.try_wait().expect("poll child").is_none(),
        "`index` returned while the summary request was still unanswered"
    );

    release_tx
        .send(())
        .expect("release the gated summary response");
    let status = child.wait().expect("wait for index");
    let stderr = stderr_reader.join().expect("join stderr reader");

    assert!(status.success(), "index failed ({status}):\n{stderr}");
    assert_summary_precedes_conventions(&stderr);

    // Post-condition: the summary is durable by the time `index` returns.
    let conn = rusqlite::Connection::open(&db_path).expect("open db");
    let unsummarised: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chunks WHERE summary IS NULL",
            [],
            |r| r.get(0),
        )
        .expect("count unsummarised chunks");
    assert_eq!(
        unsummarised, 0,
        "every chunk must have been summarised before `index` returned"
    );
    let stored: String = conn
        .query_row("SELECT summary FROM chunks WHERE id = 1", [], |r| r.get(0))
        .expect("read stored summary");
    assert_eq!(
        stored, "Adds one to x.",
        "the awaited summary must be the one the server returned"
    );
}

/// A failing LLM must be visible and must not fail the index (git-hook use).
#[test]
fn summary_failure_is_reported_but_index_still_succeeds() {
    let project = TempDir::new().expect("temp project dir");
    write_fixture(project.path(), "summary-failure-fixture");
    let db_tmp = TempDir::new().expect("temp db dir");
    let db_path = db_tmp.path().join("spelunk.db");

    let rt = tokio::runtime::Runtime::new().expect("build test runtime");
    let mock_server = rt.block_on(async {
        let server = MockServer::start().await;
        plumbing_helpers::mount_health(&server).await;
        plumbing_helpers::mount_index_embed(&server).await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/llm/complete$"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        server
    });

    let mock_url = mock_server.uri();
    let config_path = plumbing_helpers::write_config_with_server(
        project.path(),
        &db_path,
        &mock_url,
        &mock_url,
        project.path(),
    );

    let output = index_command(project.path(), &config_path, &db_path, project.path())
        .output()
        .expect("run spelunk index");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "summaries are best-effort: a dead LLM must not fail the index ({}):\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains("Summarised 0 batch(es)."),
        "a failed batch must not be counted as summarised:\n{stderr}"
    );
    assert!(
        stderr.contains("1 of 1 summary batch(es) produced no summary"),
        "the failure must be reported on stderr, not only under RUST_LOG:\n{stderr}"
    );
    assert!(
        stderr.contains("--force"),
        "the warning must name the remedy that actually retries:\n{stderr}"
    );

    // Why the remedy is `--force`: failed chunks are stored as "" rather than
    // left NULL, and `chunks_without_summaries` only matches NULL.
    let conn = rusqlite::Connection::open(&db_path).expect("open db");
    let empty: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks WHERE summary = ''", [], |r| {
            r.get(0)
        })
        .expect("count empty summaries");
    assert_eq!(
        empty, 1,
        "a failed chunk must be marked attempted, not left NULL"
    );
}

/// Phases 3-5 also run in the spawned background-phases process.
#[test]
fn background_phases_mode_completes_summaries() {
    let project = TempDir::new().expect("temp project dir");
    write_fixture(project.path(), "summary-bgphases-fixture");
    let db_tmp = TempDir::new().expect("temp db dir");
    let db_path = db_tmp.path().join("spelunk.db");

    let rt = tokio::runtime::Runtime::new().expect("build test runtime");
    let mock_server = rt.block_on(async {
        let server = MockServer::start().await;
        plumbing_helpers::mount_health(&server).await;
        plumbing_helpers::mount_index_embed(&server).await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/llm/complete$"))
            .respond_with(move |_: &wiremock::Request| {
                let body = serde_json::json!([{"id": 1, "summary": "Adds one to x."}]).to_string();
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(plumbing_helpers::sse_token_response(&body))
            })
            .mount(&server)
            .await;
        server
    });

    let mock_url = mock_server.uri();
    let config_path = plumbing_helpers::write_config_with_server(
        project.path(),
        &db_path,
        &mock_url,
        &mock_url,
        project.path(),
    );

    // Populate chunks first: background-phases mode skips parse/embed.
    let seed = index_command(project.path(), &config_path, &db_path, project.path())
        .arg("--no-summaries")
        .output()
        .expect("run seeding index");
    assert!(seed.status.success(), "seeding index failed");

    let output = index_command(project.path(), &config_path, &db_path, project.path())
        .arg("--_background-phases")
        .output()
        .expect("run background-phases index");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "background-phases index failed ({}):\n{stderr}",
        output.status
    );
    assert_summary_precedes_conventions(&stderr);

    let conn = rusqlite::Connection::open(&db_path).expect("open db");
    let stored: String = conn
        .query_row("SELECT summary FROM chunks WHERE id = 1", [], |r| r.get(0))
        .expect("read stored summary");
    assert_eq!(
        stored, "Adds one to x.",
        "background-phases mode must run the summary pass to completion too"
    );
}
