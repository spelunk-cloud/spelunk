// End to end over the real `spelunk-server` binary: an LLM endpoint supplied
// the way the CLI supplies it (url and model in argv, credential in the child
// environment) gives the daemon LLM capability and an authenticated upstream.
//
// `--model-dir` points at an empty directory so the native embedder fails fast
// locally instead of reaching the Hugging Face Hub: these tests are about the
// LLM slot, and must never download a model or touch the network.

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

async fn start_daemon(
    db_dir: &tempfile::TempDir,
    model_dir: &tempfile::TempDir,
    llm_url: Option<&str>,
    llm_model: Option<&str>,
    llm_key: Option<&str>,
) -> Daemon {
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
    ]);
    if let Some(url) = llm_url {
        cmd.args(["--llm-url", url]);
    }
    if let Some(model) = llm_model {
        cmd.args(["--llm-model", model]);
    }
    match llm_key {
        Some(k) => cmd.env("SPELUNK_LLM_KEY", k),
        None => cmd.env_remove("SPELUNK_LLM_KEY"),
    };
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning spelunk-server");

    let daemon = Daemon { child, port };
    wait_for_health(port).await;
    daemon
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

async fn capabilities(port: u16) -> Vec<String> {
    let body: serde_json::Value = reqwest::get(format!("http://127.0.0.1:{port}/v1/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_daemon_given_an_llm_url_advertises_llm_capability() {
    let db_dir = tempfile::TempDir::new().unwrap();
    let model_dir = tempfile::TempDir::new().unwrap();
    let daemon = start_daemon(&db_dir, &model_dir, Some("http://127.0.0.1:1"), None, None).await;

    let caps = capabilities(daemon.port).await;

    assert!(caps.contains(&"explore".to_string()), "got {caps:?}");
    assert!(caps.contains(&"llm.complete".to_string()), "got {caps:?}");
}

#[tokio::test]
async fn a_daemon_without_an_llm_url_advertises_neither() {
    let db_dir = tempfile::TempDir::new().unwrap();
    let model_dir = tempfile::TempDir::new().unwrap();
    let daemon = start_daemon(&db_dir, &model_dir, None, None, None).await;

    let caps = capabilities(daemon.port).await;

    assert!(!caps.contains(&"explore".to_string()), "got {caps:?}");
    assert!(!caps.contains(&"llm.complete".to_string()), "got {caps:?}");
}

#[tokio::test]
async fn a_daemon_authenticates_upstream_with_the_key_from_its_environment() {
    let upstream = wiremock::MockServer::start().await;
    // Only a correctly-bearered request is answered, so an unauthenticated or
    // wrongly-keyed daemon cannot reach the success assertion below.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer sk-llm-secret",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .mount(&upstream)
        .await;

    let db_dir = tempfile::TempDir::new().unwrap();
    let model_dir = tempfile::TempDir::new().unwrap();
    let daemon = start_daemon(
        &db_dir,
        &model_dir,
        Some(&upstream.uri()),
        Some("test-model"),
        Some("sk-llm-secret"),
    )
    .await;

    let body: String = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{}/v1/projects/acme%2Fapp/llm/complete",
            daemon.port
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
        .unwrap();

    assert!(
        body.contains("\"kind\":\"token\""),
        "the completion should have streamed through the authenticated upstream: {body}"
    );

    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "the daemon should have made exactly one upstream call"
    );
    assert_eq!(
        requests[0].headers.get("authorization").unwrap(),
        "Bearer sk-llm-secret"
    );
}

#[tokio::test]
async fn the_key_never_reaches_the_daemon_log_even_at_trace_level() {
    let db_dir = tempfile::TempDir::new().unwrap();
    let model_dir = tempfile::TempDir::new().unwrap();
    let log_dir = tempfile::TempDir::new().unwrap();
    let log_path = log_dir.path().join("server.log");
    let log = std::fs::File::create(&log_path).unwrap();

    let port = free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_spelunk-server"))
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--db",
            db_dir.path().join("server.db").to_str().unwrap(),
            "--model-dir",
            model_dir.path().to_str().unwrap(),
            "--llm-url",
            "http://127.0.0.1:1",
            "--llm-model",
            "test-model",
        ])
        .env("SPELUNK_LLM_KEY", "sk-llm-secret")
        .env("RUST_LOG", "trace")
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .expect("spawning spelunk-server");

    wait_for_health(port).await;
    // Drive the LLM path too, so a request-time log line would also be caught.
    let _ = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/v1/projects/acme%2Fapp/llm/complete"
        ))
        .json(&serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 16,
        }))
        .send()
        .await;
    let _ = child.kill();
    let _ = child.wait();

    let logged = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        logged.contains("server-side LLM enabled"),
        "the daemon should have logged its LLM configuration: {logged}"
    );
    assert!(
        !logged.contains("sk-llm-secret"),
        "the credential must never be logged: {logged}"
    );
}
