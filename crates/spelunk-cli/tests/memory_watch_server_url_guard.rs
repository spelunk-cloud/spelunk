//! Regression coverage for `spelunk memory watch` with an auto-discovered
//! loopback server but no explicit `server_url`.
//!
//! `require_tier1` passes as soon as ANY server answers `/v1/health` —
//! including a loopback server found via auto-discovery, whose
//! `cfg.server_url` stays `None` (that field is only set for an explicit team
//! server). Watching a team stream needs the explicit server, so the tier
//! check alone is not enough: this drives that exact combination end to end
//! through a real subprocess so the fix is verified against the process
//! actually crashing versus returning a clean error, not just against the
//! guard's return type.
//!
//! A mock server backs the loopback probe. It is pointed at via
//! `SPELUNK_STATE_DIR`'s `server.port` file (the same file `spelunk server
//! start` writes), so `capability::get_tier` classifies it as `Tier::Server`
//! with `server_url` still unset in config.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin;

use predicates::prelude::*;
use std::fs;
use std::path::Path;
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Write a config with no `server_url`, matching a solo user who has never
/// set one; the loopback server is found purely via auto-discovery.
fn write_no_server_url_config(dir: &Path) -> std::path::PathBuf {
    let db_path = dir.join("index.db");
    let config_path = dir.join("config.toml");
    fs::write(&config_path, format!("db_path = {db_path:?}\n")).expect("write config.toml");
    config_path
}

/// Point auto-discovery at `server`'s port via the `server.port` state file.
fn write_server_port_file(state_dir: &Path, server: &MockServer) {
    let port = server.address().port();
    fs::write(state_dir.join("server.port"), format!("{port}\n")).expect("write server.port");
}

#[tokio::test]
async fn watch_with_loopback_server_and_no_server_url_errors_instead_of_panicking() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    let temp = tempdir().unwrap();
    let config_path = write_no_server_url_config(temp.path());
    let mem_db = temp.path().join("memory.db");
    let state_dir = temp.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    write_server_port_file(&state_dir, &server);

    let assert = spelunk_bin()
        .env("SPELUNK_STATE_DIR", &state_dir)
        .env_remove("SPELUNK_NO_SERVER")
        .current_dir(temp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_db)
        .arg("watch")
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    // The regression: this used to be `.expect("require_tier1 passed")`,
    // which panics (aborts with a backtrace, not a clean error) once the
    // loopback probe makes `require_tier1` pass.
    assert!(
        !stderr.contains("panicked at"),
        "must not panic once the loopback server makes require_tier1 pass; got: {stderr}"
    );
    assert!(
        stderr.contains("spelunk memory watch") && stderr.contains("server_url"),
        "must return the actionable server_url guidance; got: {stderr}"
    );
}

/// Guard the negative-match idiom above is not vacuously true because the
/// command produced no stderr at all.
#[tokio::test]
async fn watch_with_loopback_server_and_no_server_url_message_is_nonempty() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    let temp = tempdir().unwrap();
    let config_path = write_no_server_url_config(temp.path());
    let mem_db = temp.path().join("memory.db");
    let state_dir = temp.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    write_server_port_file(&state_dir, &server);

    spelunk_bin()
        .env("SPELUNK_STATE_DIR", &state_dir)
        .env_remove("SPELUNK_NO_SERVER")
        .current_dir(temp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_db)
        .arg("watch")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "requires `server_url` to be configured",
        ));
}
