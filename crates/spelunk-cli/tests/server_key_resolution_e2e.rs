//! Test-engineer coverage: drive per-origin bearer resolution (ADR-071 D1/D2)
//! through the real binary, over the real wire, against two independent mock
//! servers standing in for the motivating multi-server case: two projects,
//! two `server_url`s, two keys, resolving correctly with no env-juggling.
//!
//! The Engineer's own suite (`crates/spelunk-core/src/config/server_keys.rs`,
//! `crates/spelunk-cli/tests/auth_server_keys.rs`) verifies resolution at the
//! unit level and the command surface (`set-key`/`list-servers`/`logout`)
//! end to end, but nothing exercises the actual `Authorization` header a real
//! request carries to a real (mocked) origin. That is the one place a
//! same-string-comparison or map-mixup bug would actually manifest as a
//! credential going to the wrong server, so this file inspects the header
//! wiremock received rather than trusting the CLI's own stdout/exit code.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin_in;

use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROJECT_ID: &str = "test-org/test-project";

async fn mount_health_and_since(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path_regex(r"^/v1/health$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory"],
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/v1/projects/.+/memory/since$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(server)
        .await;
}

// `server_url`/`project_id` are set via `SPELUNK_SERVER_URL`/`SPELUNK_PROJECT_ID`
// env on each command below, not this file: `Config::load` only honors those
// two fields from a project-level `.spelunk/config.toml` (discovered by
// walking up from CWD) or env, never from the `--config` file this test
// swaps per origin. Env is the natural fit here since this file's whole
// point is bearer-per-origin resolution, not config-file precedence.
fn write_server_config(dir: &Path, name: &str) -> std::path::PathBuf {
    let config_path = dir.join(format!("{name}.toml"));
    std::fs::write(&config_path, "").unwrap();
    config_path
}

fn set_key(home: &Path, server: &str, key: &str) {
    spelunk_bin_in(home)
        .arg("auth")
        .arg("set-key")
        .arg("--server")
        .arg(server)
        .write_stdin(format!("{key}\n"))
        .assert()
        .success();
}

/// The multi-server acceptance case, driven for real: two `server_url`s
/// under the *same* HOME (so they share one secret-store map, D1's whole
/// point), each with its own key set via the real `auth set-key` command,
/// then two separate `spelunk memory since` invocations, one per origin,
/// each inspected for the literal `Authorization` header wiremock received.
/// Each origin must get exactly its own key, never the other's, and never
/// an env var (none is set at any point in this test).
#[tokio::test]
async fn two_servers_two_keys_each_gets_only_its_own_bearer_over_the_wire() {
    let server_a = MockServer::start().await;
    let server_b = MockServer::start().await;
    mount_health_and_since(&server_a).await;
    mount_health_and_since(&server_b).await;

    let home = TempDir::new().unwrap();
    let cfg_dir = TempDir::new().unwrap();

    set_key(home.path(), &server_a.uri(), "sk-project-a-secret");
    set_key(home.path(), &server_b.uri(), "sk-project-b-secret");

    let config_a = write_server_config(cfg_dir.path(), "a");
    let config_b = write_server_config(cfg_dir.path(), "b");

    let mem_db = cfg_dir.path().join("memory.db");

    spelunk_bin_in(home.path())
        .env_remove("SPELUNK_SERVER_KEY")
        .env("SPELUNK_SERVER_URL", server_a.uri())
        .env("SPELUNK_PROJECT_ID", PROJECT_ID)
        .arg("--config")
        .arg(&config_a)
        .arg("memory")
        .arg("--db")
        .arg(&mem_db)
        .arg("since")
        .arg("0")
        .assert()
        .success();

    spelunk_bin_in(home.path())
        .env_remove("SPELUNK_SERVER_KEY")
        .env("SPELUNK_SERVER_URL", server_b.uri())
        .env("SPELUNK_PROJECT_ID", PROJECT_ID)
        .arg("--config")
        .arg(&config_b)
        .arg("memory")
        .arg("--db")
        .arg(&mem_db)
        .arg("since")
        .arg("0")
        .assert()
        .success();

    let requests_a = server_a.received_requests().await.unwrap();
    let since_req_a = requests_a
        .iter()
        .find(|r| r.url.path().ends_with("/memory/since"))
        .expect("server A received a /memory/since request");
    assert_eq!(
        since_req_a
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap()),
        Some("Bearer sk-project-a-secret"),
        "server A must receive exactly its own key"
    );

    let requests_b = server_b.received_requests().await.unwrap();
    let since_req_b = requests_b
        .iter()
        .find(|r| r.url.path().ends_with("/memory/since"))
        .expect("server B received a /memory/since request");
    assert_eq!(
        since_req_b
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap()),
        Some("Bearer sk-project-b-secret"),
        "server B must receive exactly its own key, never A's"
    );
}

/// Migration end-to-end through the real binary and the real (file-backed)
/// secret store: a legacy flat key planted the way a pre-ADR-071 client
/// would have left it (raw `KEY_SERVER_KEY` entry, no map) is picked up
/// transparently on first use against a self-hosted `server_url`: the
/// request still succeeds, and `auth list-servers` afterward shows it
/// migrated into the per-origin map with the legacy tier gone. This is the
/// "continues to work transparently on first use" claim from the ADR,
/// verified by actually running the binary against a live (mock) server
/// rather than only asserting on `Config::bearer_for`'s return value.
#[tokio::test]
async fn legacy_flat_key_migrates_transparently_on_first_real_request() {
    let server = MockServer::start().await;
    mount_health_and_since(&server).await;

    let home = TempDir::new().unwrap();
    let cfg_dir = TempDir::new().unwrap();

    // Plant a legacy flat key exactly where a pre-ADR-071 `spelunk login`/
    // plaintext-config migration would have left it: the file-backed
    // secret-store entry named by `KEY_SERVER_KEY` ("server_key"), under the
    // same isolated HOME `spelunk_bin_in` points every child process at.
    // `auth set-key` itself only ever writes the new per-origin map, so
    // there is no CLI surface to plant this pre-migration state: writing
    // the secrets.toml file directly is the only way to simulate an
    // upgrading (not fresh) install.
    let secrets_path = home.path().join(".config").join("spelunk");
    std::fs::create_dir_all(&secrets_path).unwrap();
    std::fs::write(
        secrets_path.join("secrets.toml"),
        "server_key = \"sk-legacy-preupgrade\"\n",
    )
    .unwrap();

    let config_path = write_server_config(cfg_dir.path(), "legacy");
    let mem_db = cfg_dir.path().join("memory.db");

    spelunk_bin_in(home.path())
        .env_remove("SPELUNK_SERVER_KEY")
        .env("SPELUNK_SERVER_URL", server.uri())
        .env("SPELUNK_PROJECT_ID", PROJECT_ID)
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_db)
        .arg("since")
        .arg("0")
        .assert()
        .success();

    let requests = server.received_requests().await.unwrap();
    let since_req = requests
        .iter()
        .find(|r| r.url.path().ends_with("/memory/since"))
        .expect("server received a /memory/since request");
    assert_eq!(
        since_req
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap()),
        Some("Bearer sk-legacy-preupgrade"),
        "the legacy key must be sent transparently on first use"
    );

    // And it must now be visible as a migrated per-origin entry, with the
    // legacy tier gone (D3/D1's "migrate, don't dual-read forever").
    let out = spelunk_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains(server.uri().as_str()) || text.contains(&server.address().to_string()),
        "migrated origin must be listed:\n{text}"
    );
    assert!(
        !text.contains("a legacy server key is also stored"),
        "legacy tier must be gone after migration:\n{text}"
    );
}
