// Server-side credential precedence, observed where it actually matters: on
// the bearer the compiled binary sends upstream.
//
// The unit tests in `server_llm.rs` pin `resolve_llm_key` in isolation, which
// cannot see how clap fills the args in the first place. If `--llm-key` ever
// gained an `env = "SPELUNK_LLM_KEY"` attribute, `resolve_llm_key` would still
// be correct and still be green, while the binary quietly started preferring
// the environment over `--llm-key-file`. Only an end-to-end assertion on the
// wire catches that.
//
// `--model-dir` points at an empty directory so the native embedder fails fast
// locally instead of reaching the Hugging Face Hub: nothing here downloads a
// model or touches a non-loopback network.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Daemon {
    child: Child,
    port: u16,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn wait_for_health(port: u16) {
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if let Ok(r) = client
            .get(format!("http://127.0.0.1:{port}/v1/health"))
            .send()
            .await
            && r.status().is_success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("spelunk-server on port {port} never became healthy");
}

// Answers only a request bearing `expected_bearer`, so a daemon that resolved
// any other credential cannot reach the success assertion.
async fn upstream_requiring(expected_bearer: &str) -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .and(wiremock::matchers::header("authorization", expected_bearer))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    server
}

async fn complete(port: u16) -> String {
    reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/v1/projects/acme%2Fapp/llm/complete"
        ))
        .json(&serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 16,
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

struct Fixture {
    _db_dir: tempfile::TempDir,
    _model_dir: tempfile::TempDir,
    daemon: Daemon,
}

async fn start(llm_url: &str, configure: impl FnOnce(&mut Command)) -> Fixture {
    let db_dir = tempfile::TempDir::new().unwrap();
    let model_dir = tempfile::TempDir::new().unwrap();
    let port = free_port();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spelunk-server"));
    cmd.args([
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "--db",
        db_dir.path().join("server.db").to_str().unwrap(),
        "--model-dir",
        model_dir.path().to_str().unwrap(),
        "--llm-url",
        llm_url,
        "--llm-model",
        "test-model",
    ]);
    cmd.env_remove("SPELUNK_LLM_KEY");
    configure(&mut cmd);

    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning spelunk-server");

    let daemon = Daemon { child, port };
    wait_for_health(port).await;
    Fixture {
        _db_dir: db_dir,
        _model_dir: model_dir,
        daemon,
    }
}

fn key_file(dir: &tempfile::TempDir, contents: &str) -> std::path::PathBuf {
    let path = dir.path().join("llm.key");
    std::fs::write(&path, contents).unwrap();
    path
}

#[tokio::test]
async fn a_key_file_outranks_the_environment_on_the_wire() {
    let upstream = upstream_requiring("Bearer sk-from-file").await;
    let key_dir = tempfile::TempDir::new().unwrap();
    let path = key_file(&key_dir, "sk-from-file\n");

    let fixture = start(&upstream.uri(), |cmd| {
        cmd.args(["--llm-key-file", path.to_str().unwrap()])
            .env("SPELUNK_LLM_KEY", "sk-from-env");
    })
    .await;

    let body = complete(fixture.daemon.port).await;
    assert!(
        body.contains("\"kind\":\"token\""),
        "the daemon should have authenticated with the key file, not the environment: {body}"
    );

    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers.get("authorization").unwrap(),
        "Bearer sk-from-file"
    );
}

#[tokio::test]
async fn the_inline_key_outranks_both_the_key_file_and_the_environment() {
    let upstream = upstream_requiring("Bearer sk-inline").await;
    let key_dir = tempfile::TempDir::new().unwrap();
    let path = key_file(&key_dir, "sk-from-file\n");

    let fixture = start(&upstream.uri(), |cmd| {
        cmd.args(["--llm-key", "sk-inline"])
            .args(["--llm-key-file", path.to_str().unwrap()])
            .env("SPELUNK_LLM_KEY", "sk-from-env");
    })
    .await;

    let body = complete(fixture.daemon.port).await;
    assert!(body.contains("\"kind\":\"token\""), "got {body}");

    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers.get("authorization").unwrap(),
        "Bearer sk-inline"
    );
}

// A blank environment value is what `${SPELUNK_LLM_KEY:-}` expands to with the
// variable unset, and must read as unauthenticated rather than as a real
// empty-string credential that every upstream request then carries.
#[tokio::test]
async fn a_blank_environment_key_sends_no_authorization_header() {
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .mount(&upstream)
        .await;

    let fixture = start(&upstream.uri(), |cmd| {
        cmd.env("SPELUNK_LLM_KEY", "   ");
    })
    .await;

    let _ = complete(fixture.daemon.port).await;

    let requests = upstream.received_requests().await.unwrap();
    assert!(
        requests[0].headers.get("authorization").is_none(),
        "a blank credential must send no header at all"
    );
}

// An unreadable key file names an operator mistake; authenticating with some
// other credential instead would hide it.
//
// `--db` points into a directory that does not exist, so this invocation exits
// however the resolution goes: the stderr says which check fired, and a
// fall-through to the environment would report the db path rather than the
// key file. Without that, a regression here would hang on a daemon that
// started successfully instead of failing.
#[test]
fn a_missing_key_file_refuses_to_start_rather_than_using_the_environment() {
    let key_dir = tempfile::TempDir::new().unwrap();
    let missing = key_dir.path().join("absent.key");

    let out = Command::new(env!("CARGO_BIN_EXE_spelunk-server"))
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            "7777",
            "--db",
            "/nonexistent-spelunk-test-dir/server.db",
            "--llm-url",
            "http://127.0.0.1:1",
            "--llm-key-file",
            missing.to_str().unwrap(),
        ])
        .env("SPELUNK_LLM_KEY", "sk-from-env")
        .output()
        .expect("spawning spelunk-server");

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("absent.key"),
        "the error must name the unreadable file, not fall through to the environment: {stderr}"
    );
    assert!(
        !stderr.contains("sk-from-env"),
        "no credential may appear in the failure output: {stderr}"
    );
}
