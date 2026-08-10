// End-to-end routing for `spelunk index`'s LLM summary pass.
//
// The reported symptom: an index with `server_url` set skipped every summary.
// Two separate gates caused it, and a loopback-only project (no `server_url`
// at all) hit the earlier of the two, so both are covered here.
//
// LLM routing and embed routing are separate rules and can land on different
// servers, so several tests assert on which mock received which route rather
// than only on the outcome. The loopback mock is wired in through
// `SPELUNK_STATE_DIR`/`server.port` (real auto-discovery), never `server_url`,
// so a routing regression shows up as a request landing on the wrong mock.

mod plumbing_helpers;
use plumbing_helpers::{FIXTURE_PROJECT_ID, mount_index_embed, spelunk_bin_in, sse_token_response};

use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── fixtures ──────────────────────────────────────────────────────────────

// One function, so exactly one chunk and one summary batch.
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
        "pub fn greet(name: &str) -> String {\n    format!(\"hello, {name}\")\n}\n",
    )
    .expect("write lib.rs");
}

fn write_server_config(project_dir: &Path, server_url: &str) {
    let spelunk_dir = project_dir.join(".spelunk");
    std::fs::create_dir_all(&spelunk_dir).expect("create .spelunk dir");
    std::fs::write(
        spelunk_dir.join("config.toml"),
        format!("server_url = {server_url:?}\nproject_id = {FIXTURE_PROJECT_ID:?}\n"),
    )
    .expect("write project config");
}

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

fn health_body(llm: bool) -> serde_json::Value {
    let mut caps = vec!["memory", "index.embed", "search.semantic", "explore"];
    if llm {
        caps.push("llm.complete");
    }
    serde_json::json!({
        "status": "ok",
        "version": "test",
        "capabilities": caps,
        "embedding_dim": 896,
        "embedder": { "state": "ready", "detail": null },
    })
}

async fn mount_health_with_llm(server: &MockServer, llm: bool) {
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(health_body(llm)))
        .mount(server)
        .await;
}

async fn mount_llm_complete(server: &MockServer, summary: &str) {
    let body = serde_json::json!([{"id": 1, "summary": summary}]).to_string();
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/llm/complete$"))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_token_response(&body))
        })
        .mount(server)
        .await;
}

// A fully-featured spelunk-server mock: health, embed and LLM.
async fn full_server(llm: bool, summary: &str) -> MockServer {
    let server = MockServer::start().await;
    mount_health_with_llm(&server, llm).await;
    mount_index_embed(&server).await;
    if llm {
        mount_llm_complete(&server, summary).await;
    }
    server
}

// `spelunk index .`, scrubbed of every ambient `SPELUNK_*` these tests
// isolate: a value in the developer's shell must never change which server
// gets probed.
fn index_cmd(home: &Path, project: &Path, db: &Path) -> assert_cmd::Command {
    let mut cmd = spelunk_bin_in(home);
    cmd.current_dir(project)
        .env_remove("SPELUNK_SERVER_URL")
        .env_remove("SPELUNK_MODE")
        .env_remove("SPELUNK_PROJECT_ID")
        .env_remove("SPELUNK_NO_SERVER")
        .env_remove("SPELUNK_STATE_DIR")
        .env_remove("SPELUNK_LLM_URL")
        .env_remove("SPELUNK_LLM_MODEL")
        .arg("index")
        .arg("--db")
        .arg(db)
        .arg(".");
    cmd
}

fn ensure_sqlite_vec() {
    plumbing_helpers::register_sqlite_vec();
}

fn stored_summaries(db_path: &Path) -> Vec<String> {
    ensure_sqlite_vec();
    let conn = rusqlite::Connection::open(db_path).expect("open db");
    let mut stmt = conn
        .prepare("SELECT summary FROM chunks WHERE summary IS NOT NULL AND summary != ''")
        .expect("prepare");
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query summaries");
    rows.map(|r| r.expect("read summary")).collect()
}

fn count_where(db_path: &Path, sql: &str) -> i64 {
    ensure_sqlite_vec();
    let conn = rusqlite::Connection::open(db_path).expect("open db");
    conn.query_row(sql, [], |r| r.get(0)).expect("count")
}

async fn llm_request_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("requests recorded")
        .iter()
        .filter(|r| r.url.path().contains("/llm/complete"))
        .count()
}

// The whole point of the replacement message: a user must never be shown an
// internal name they cannot act on.
fn assert_no_internal_names(output: &str) {
    for jargon in [
        "ServerInferenceClient",
        "ServerLlmClient",
        "ServerLlmAdapter",
        "ServerEmbedAdapter",
        "Capabilities",
        "inference_url",
        "llm.complete",
    ] {
        assert!(
            !output.contains(jargon),
            "user-facing output leaked {jargon:?}:\n{output}"
        );
    }
}

// ── the two gates that skipped summaries ──────────────────────────────────

// The gate the reported symptom never even reached: a loopback-only project
// with no `server_url` at all was skipped before any client was built.
#[tokio::test]
async fn loopback_llm_with_no_server_url_generates_summaries() {
    let loopback = full_server(true, "Greets a name.").await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    // Deliberately no `.spelunk/config.toml`: no server_url anywhere.
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    let db = project.path().join("index.db");
    let assert = index_cmd(home.path(), project.path(), &db)
        .env("SPELUNK_STATE_DIR", &state_dir)
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    assert_eq!(
        stored_summaries(&db),
        vec!["Greets a name.".to_string()],
        "a loopback daemon with an LLM must summarise even with no server_url:\n{stderr}"
    );
}

// The reported scenario exactly: `server_url` set, loopback serving the LLM.
// Summaries must be generated, and via the loopback, never the remote.
#[tokio::test]
async fn local_first_with_server_url_summarises_via_loopback_not_the_remote() {
    let loopback = full_server(true, "Greets a name.").await;
    let remote = full_server(true, "SHOULD NOT BE USED").await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    write_server_config(project.path(), &remote.uri());
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    let db = project.path().join("index.db");
    let assert = index_cmd(home.path(), project.path(), &db)
        .env("SPELUNK_STATE_DIR", &state_dir)
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    assert_eq!(
        stored_summaries(&db),
        vec!["Greets a name.".to_string()],
        "the reported scenario must now summarise:\n{stderr}"
    );
    assert_eq!(
        llm_request_count(&loopback).await,
        1,
        "the local LLM must serve the summary pass:\n{stderr}"
    );
    assert_eq!(
        llm_request_count(&remote).await,
        0,
        "a usable local LLM must not send code to the remote:\n{stderr}"
    );
}

// ── remote fallback ───────────────────────────────────────────────────────

// Loopback reachable but without an LLM, no `llm_url` configured: the remote
// is the only LLM available, so it is used. Embedding still goes local.
#[tokio::test]
async fn loopback_without_an_llm_falls_back_to_the_llm_capable_remote() {
    let loopback = full_server(false, "").await;
    let remote = full_server(true, "Greets a name.").await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    write_server_config(project.path(), &remote.uri());
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    let db = project.path().join("index.db");
    let assert = index_cmd(home.path(), project.path(), &db)
        .env("SPELUNK_STATE_DIR", &state_dir)
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    assert_eq!(
        stored_summaries(&db),
        vec!["Greets a name.".to_string()],
        "the remote must serve the LLM when the local server has none:\n{stderr}"
    );
    assert_eq!(llm_request_count(&remote).await, 1, "{stderr}");

    // Embedding routing is a separate rule and must not follow the LLM.
    let loopback_embeds = loopback
        .received_requests()
        .await
        .expect("recorded")
        .iter()
        .filter(|r| r.url.path().contains("/index/embed"))
        .count();
    let remote_embeds = remote
        .received_requests()
        .await
        .expect("recorded")
        .iter()
        .filter(|r| r.url.path().contains("/index/embed"))
        .count();
    assert!(
        loopback_embeds > 0,
        "embedding must stay on the loopback:\n{stderr}"
    );
    assert_eq!(
        remote_embeds, 0,
        "routing the LLM to the remote must not divert embedding there:\n{stderr}"
    );
}

// ── the privacy guard ─────────────────────────────────────────────────────

// `llm_url` configured but the running daemon does not serve an LLM: the user
// asked for a local LLM, so their code must not go to the remote instead. The
// index still succeeds, because summaries are optional.
#[tokio::test]
async fn configured_local_llm_not_served_skips_and_never_reaches_the_remote() {
    let loopback = full_server(false, "").await;
    let remote = full_server(true, "SHOULD NOT BE USED").await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    write_server_config(project.path(), &remote.uri());
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    let db = project.path().join("index.db");
    let assert = index_cmd(home.path(), project.path(), &db)
        .env("SPELUNK_STATE_DIR", &state_dir)
        .env("SPELUNK_LLM_URL", "http://127.0.0.1:1234")
        .assert()
        .success();
    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        llm_request_count(&remote).await,
        0,
        "code must never reach a remote LLM the user did not choose:\n{combined}"
    );
    assert!(
        stored_summaries(&db).is_empty(),
        "no summary can exist when no LLM ran:\n{combined}"
    );
    assert!(
        combined.contains("spelunk server stop") && combined.contains("spelunk server start"),
        "the restart is the only useful instruction here:\n{combined}"
    );
    assert!(
        combined.contains("llm_url"),
        "the message must name the setting that is being ignored:\n{combined}"
    );
    // The privacy guard rendered as prose: a user who asked for a local LLM
    // must not be nudged toward the remote one this run deliberately avoided.
    assert!(
        !combined.contains("server_url"),
        "the notice must not offer the remote as a way out:\n{combined}"
    );
    assert_no_internal_names(&combined);
}

// ── no LLM at all ─────────────────────────────────────────────────────────

#[tokio::test]
async fn no_llm_anywhere_skips_with_both_routes_and_the_opt_out_flag() {
    let loopback = full_server(false, "").await;

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
    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        combined.contains("llm_url"),
        "must offer the local route:\n{combined}"
    );
    assert!(
        combined.contains("server_url"),
        "must offer the remote route:\n{combined}"
    );
    assert!(
        combined.contains("--no-summaries"),
        "must say how to silence the notice:\n{combined}"
    );
    assert_no_internal_names(&combined);

    // Everything except summaries must still be complete.
    assert!(
        count_where(&db, "SELECT COUNT(*) FROM files") > 0,
        "no files"
    );
    assert!(
        count_where(&db, "SELECT COUNT(*) FROM chunks") > 0,
        "no chunks"
    );
    assert!(
        count_where(&db, "SELECT COUNT(*) FROM embeddings") > 0,
        "no embeddings"
    );
    // A skipped pass must leave chunks retryable rather than marking them
    // attempted with an empty summary.
    assert_eq!(
        count_where(&db, "SELECT COUNT(*) FROM chunks WHERE summary IS NOT NULL"),
        0,
        "a skip must not mark chunks as summary-attempted"
    );
}

#[tokio::test]
async fn offline_mode_skips_summaries_with_the_offline_message() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());

    let db = project.path().join("index.db");
    let assert = index_cmd(home.path(), project.path(), &db)
        .env("SPELUNK_NO_SERVER", "1")
        .assert()
        .success();
    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        combined.contains("offline mode is on"),
        "must name offline mode as the cause:\n{combined}"
    );
    assert_no_internal_names(&combined);
}

// ── --no-summaries short-circuits before any routing ──────────────────────

// The flag must be honoured before anything is probed, in every configuration.
// The loopback mock is fully capable, so a routed run would summarise; nothing
// may reach it, and no notice may be printed.
#[tokio::test]
async fn no_summaries_flag_short_circuits_before_any_routing() {
    for llm_url in [None, Some("http://127.0.0.1:1234")] {
        let loopback = full_server(true, "Greets a name.").await;
        let remote = full_server(true, "SHOULD NOT BE USED").await;

        let home = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        write_project(project.path());
        write_server_config(project.path(), &remote.uri());
        let state_dir = home.path().join("state");
        write_loopback_state(&state_dir, &loopback.uri());

        let db = project.path().join("index.db");
        let mut cmd = index_cmd(home.path(), project.path(), &db);
        cmd.env("SPELUNK_STATE_DIR", &state_dir)
            .arg("--no-summaries");
        if let Some(url) = llm_url {
            cmd.env("SPELUNK_LLM_URL", url);
        }
        let assert = cmd.assert().success();
        let output = assert.get_output();
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        assert_eq!(
            llm_request_count(&loopback).await,
            0,
            "--no-summaries (llm_url={llm_url:?}) must not call any LLM:\n{combined}"
        );
        assert_eq!(llm_request_count(&remote).await, 0, "{combined}");
        assert!(
            !combined.contains("Skipping chunk summaries"),
            "--no-summaries is a deliberate choice, not something to explain:\n{combined}"
        );
    }
}
