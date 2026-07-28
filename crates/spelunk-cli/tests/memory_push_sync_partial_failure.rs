// Subprocess-level coverage for a mid-push (partial) failure: a multi-chunk
// `spelunk memory push` / `spelunk sync` whose later chunk fails must exit
// non-zero, print honest partial progress (`Pushed X of Y`) plus a resume hint,
// and never print success framing (`Done.` / `Sync complete.`).
//
// `push_local` (`crates/spelunk-cli/src/cli/cmd/memory/sync.rs`) stops at the
// first failed chunk, keeps the chunks that already landed durably stamped, and
// returns a summary marked `interrupted` rather than `?`-propagating; the
// command layer (`push.rs` / `sync.rs`) turns that into the partial-progress
// message and a non-zero exit via `bail!`. These tests spawn the real compiled
// `spelunk` binary (`assert_cmd`, following `memory_push_sync_total_failure.rs`)
// against a mock team server that serves the first chunk then 500s, so a
// regression in the command-layer framing or exit code is what fails here.

mod plumbing_helpers;
use plumbing_helpers::{register_sqlite_vec, spelunk_bin_in};

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use spelunk_core::storage::MemoryStore;

// Project slug with no characters `encode_project_id` would percent-encode,
// so the mocked route paths below can be matched literally.
const PROJECT_SLUG: &str = "acme-widget";

// Enough entries to span more than one push chunk (chunk size is 50), so a
// later chunk can fail after an earlier one has already landed.
const SEED_COUNT: usize = 60;

// The first `POST /memory/batch` (chunk 1) lands `created: 50`; every later
// request 500s (chunk 2 fails). Keyed on call count so it does not depend on
// wiremock's ordering of same-path mocks.
struct FirstChunkThenFail {
    calls: AtomicUsize,
}
impl Respond for FirstChunkThenFail {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 50, "skipped": 0, "failed": 0, "results": []
            }))
        } else {
            ResponseTemplate::new(500).set_body_string("overloaded")
        }
    }
}

async fn mount_health(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["memory"],
        })))
        .mount(server)
        .await;
}

async fn mount_batch_first_ok_then_fail(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT_SLUG}/memory/batch")))
        .respond_with(FirstChunkThenFail {
            calls: AtomicUsize::new(0),
        })
        .mount(server)
        .await;
}

// The pull half of `spelunk sync` runs independently of the push outcome.
async fn mount_since_empty(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path_regex(format!(
            r"^/v1/projects/{PROJECT_SLUG}/memory/since$"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": []
        })))
        .mount(server)
        .await;
}

// See `memory_push_sync_total_failure.rs::write_config` for why `server_url` /
// `project_id` live in the project-level `.spelunk/config.toml` rather than the
// `--config` file.
fn write_config(dir: &Path, server_url: &str) -> std::path::PathBuf {
    let db_path = dir.join(".spelunk").join("index.db");
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "db_path = {db_path:?}\n\
             llm_model = \"test-chat\"\n"
        ),
    )
    .expect("write config.toml");
    plumbing_helpers::write_project_server_config(dir, server_url, PROJECT_SLUG);
    config_path
}

fn init_project(proj: &Path) {
    std::fs::create_dir_all(proj.join(".spelunk")).expect("create .spelunk");
}

// Seed a standalone memory.db with `SEED_COUNT` distinct live notes, directly
// via the library, so the push has a multi-chunk live set without spawning one
// `memory add` subprocess per note.
fn seed_source_store(mem_path: &Path) {
    register_sqlite_vec();
    std::fs::create_dir_all(mem_path.parent().unwrap()).expect("create source dir");
    let store = MemoryStore::open(mem_path).expect("open source memory.db");
    for i in 0..SEED_COUNT {
        store
            .add_note("note", &format!("T{i}"), "body", &[], &[], None, None)
            .expect("seed note");
    }
}

#[tokio::test]
async fn memory_push_mid_chunk_failure_exits_nonzero_with_resume_hint() {
    let server = MockServer::start().await;
    mount_health(&server).await;
    mount_batch_first_ok_then_fail(&server).await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_project(proj.path());
    let config_path = write_config(proj.path(), &server.uri());
    let mem_path = proj.path().join("seed-memory.db");
    seed_source_store(&mem_path);

    let assert = spelunk_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "push", "--source"])
        .arg(&mem_path)
        .assert()
        .failure();

    let out = assert.get_output();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("Pushed 50 of {SEED_COUNT}")),
        "must report honest partial progress; stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("Re-run to resume"),
        "must give a resume hint; stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        !stdout.contains("Done.") && !stderr.contains("Done."),
        "an interrupted push must never read as success; stdout={stdout:?} stderr={stderr:?}"
    );
}

#[tokio::test]
async fn memory_sync_mid_chunk_failure_exits_nonzero_with_resume_hint() {
    let server = MockServer::start().await;
    mount_health(&server).await;
    mount_batch_first_ok_then_fail(&server).await;
    mount_since_empty(&server).await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_project(proj.path());
    let config_path = write_config(proj.path(), &server.uri());
    let mem_path = proj.path().join("seed-memory.db");
    seed_source_store(&mem_path);

    let assert = spelunk_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&config_path)
        .args(["sync", "--source"])
        .arg(&mem_path)
        .assert()
        .failure();

    let out = assert.get_output();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("Pushed 50 of {SEED_COUNT}")),
        "must report honest partial progress; stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("Re-run to resume"),
        "must give a resume hint; stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        !stdout.contains("Sync complete.") && !stderr.contains("Sync complete."),
        "an interrupted sync must never read as success; stdout={stdout:?} stderr={stderr:?}"
    );
}
