//! Subprocess-level regression coverage: a total-failure `spelunk memory push`
//! / `spelunk sync` batch must exit non-zero and never print success framing.
//!
//! `memory_push`/`memory_sync` (`crates/spelunk-cli/src/cli/cmd/memory/{push,sync}.rs`)
//! treat `attempted > 0 && created == 0 && skipped == 0` as a hard failure:
//! the message leads with "Push failed"/"Sync failed" and the command returns
//! `Err`, which `main`'s `#[tokio::main] fn -> Result<()>` maps to a non-zero
//! exit. A prior version of this coverage exercised that predicate as a
//! tautology, or called `push_local` (a function this behaviour doesn't live
//! in) directly: neither would fail if the `bail!` blocks driving the actual
//! exit code were reverted. These tests spawn the real compiled `spelunk`
//! binary (`assert_cmd`, following `fail_closed_no_project.rs`'s pattern)
//! against a mock team server that returns an all-failed batch result, so a
//! regression in the command-layer `bail!` itself is what fails here.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin_in;

use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Project slug with no characters `encode_project_id` would percent-encode,
/// so the mocked route paths below can be matched literally.
const PROJECT_SLUG: &str = "acme-widget";

/// Mount `GET /v1/health` advertising a minimal Tier 1 server. `require_tier1`
/// only checks `tier.is_server()` (any 200 response), so a bare `memory`
/// capability is enough to unlock `memory push`/`sync`.
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

/// Mount `POST /v1/projects/{slug}/memory/batch` returning a batch result
/// where nothing durably landed: `created: 0, skipped: 0`, and an empty
/// `results[]` so `push_local` falls back to the aggregate ints instead of
/// per-item reconciliation (see `sync.rs`'s `res.results.is_empty()` branch).
/// This is the exact wire shape the command layer must read as a hard
/// failure rather than success.
async fn mount_batch_total_failure(server: &MockServer, failed: u32) {
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT_SLUG}/memory/batch")))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 0, "skipped": 0, "failed": failed, "results": []
        })))
        .mount(server)
        .await;
}

/// Mount `GET /v1/projects/{slug}/memory/since` returning no entries, for the
/// pull half of `spelunk sync` (which runs independently of the push outcome).
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

// Write a config with no `server_key`/`[auth]`, so `ensure_fresh_server_key`
// takes its no-op path (`auth_api.rs`) and push/sync never needs a real
// WorkOS login against a keyless plaintext loopback server. `server_url`/
// `project_id` point at the mock server via `<dir>/.spelunk/config.toml`
// instead of this global file: `Config::load` only honors those two fields
// from a project-level config (or env). Every caller already sets
// `.current_dir(dir)`.
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

/// Create a `.spelunk/` marker dir so ADR-067's fail-closed project gate
/// resolves `proj` as a real local project, mirroring
/// `fail_closed_no_project.rs::memory_add_works_with_local_dot_spelunk`.
fn init_project(proj: &Path) {
    std::fs::create_dir_all(proj.join(".spelunk")).expect("create .spelunk");
}

/// Seed one local memory entry via a real `spelunk memory add` subprocess run,
/// so the subsequent push/sync has something `attempted > 0` to push.
fn seed_one_note(home: &Path, proj: &Path, config_path: &Path) {
    spelunk_bin_in(home)
        .current_dir(proj)
        .arg("--config")
        .arg(config_path)
        .args([
            "memory", "add", "--kind", "note", "--title", "T", "--body", "B",
        ])
        .assert()
        .success();
}

#[tokio::test]
async fn memory_push_total_failure_exits_nonzero_and_does_not_print_done() {
    let server = MockServer::start().await;
    mount_health(&server).await;
    mount_batch_total_failure(&server, 1).await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_project(proj.path());
    let config_path = write_config(proj.path(), &server.uri());
    seed_one_note(home.path(), proj.path(), &config_path);

    let assert = spelunk_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "push"])
        .assert()
        .failure();

    let out = assert.get_output();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Push failed"),
        "must surface the failure message; stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        !stdout.contains("Done.") && !stderr.contains("Done."),
        "a total-failure push must never read as success; stdout={stdout:?} stderr={stderr:?}"
    );
}

#[tokio::test]
async fn memory_sync_total_failure_exits_nonzero_and_does_not_print_sync_complete() {
    let server = MockServer::start().await;
    mount_health(&server).await;
    mount_batch_total_failure(&server, 1).await;
    mount_since_empty(&server).await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_project(proj.path());
    let config_path = write_config(proj.path(), &server.uri());
    seed_one_note(home.path(), proj.path(), &config_path);

    let assert = spelunk_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&config_path)
        .arg("sync")
        .assert()
        .failure();

    let out = assert.get_output();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Sync failed"),
        "must surface the failure message; stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        !stdout.contains("Sync complete.") && !stderr.contains("Sync complete."),
        "a total-failure sync must never read as success; stdout={stdout:?} stderr={stderr:?}"
    );
}

/// A total push failure still runs the full two-phase pull reconciliation
/// (`sync_round`'s pull, push, pull-again sequence), and the failure
/// message's pull count is the honest combined total across both passes,
/// not just the first pass or zero.
///
/// Both pull calls in `sync_round` reuse the same pre-round cursor, so a
/// stateless mock returning one remote entry for `/since` regardless of
/// `since_id` is hit identically by both passes: the first applies it (new),
/// the second re-fetches it but it's already known locally (dedup on
/// `remote_id`), so the reported total is the true, non-doubled count.
#[tokio::test]
async fn memory_sync_total_failure_reports_the_full_two_pass_pull_count() {
    let server = MockServer::start().await;
    mount_health(&server).await;
    mount_batch_total_failure(&server, 1).await;
    Mock::given(method("GET"))
        .and(path_regex(format!(
            r"^/v1/projects/{PROJECT_SLUG}/memory/since$"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [{
                "id": "01890000-0000-7000-8000-000000000abc",
                "kind": "decision",
                "title": "Teammate",
                "body": "already on the server",
                "created_at": "2026-06-19T01:00:00Z"
            }]
        })))
        .mount(&server)
        .await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_project(proj.path());
    let config_path = write_config(proj.path(), &server.uri());
    seed_one_note(home.path(), proj.path(), &config_path);

    let assert = spelunk_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&config_path)
        .arg("sync")
        .assert()
        .failure();

    let out = assert.get_output();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Sync failed"),
        "must still surface the push failure; stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("pull still applied 1 new remote entries"),
        "the pull count must reflect the one genuinely new entry across both \
         reconciliation passes, not zero and not double-counted; stderr={stderr:?}"
    );
}

/// Regression guard for the fix's OTHER side: a real success must still exit
/// zero and print the "Done." success framing. Without this, a broken change
/// that made every push exit non-zero unconditionally would still pass the
/// two tests above.
#[tokio::test]
async fn memory_push_success_still_exits_zero_and_prints_done() {
    let server = MockServer::start().await;
    mount_health(&server).await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT_SLUG}/memory/batch")))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0, "results": []
        })))
        .mount(&server)
        .await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_project(proj.path());
    let config_path = write_config(proj.path(), &server.uri());
    seed_one_note(home.path(), proj.path(), &config_path);

    spelunk_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "push"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Done."));
}
