// Real-process coverage for the startup guard that refuses to send a
// configured LLM credential over a plaintext non-loopback hop.
//
// The unit tests in `server_llm.rs` pin the decision itself; these prove the
// compiled binary a user actually runs enforces it, with a non-zero exit and
// an error naming the endpoint.
//
// The guard runs before the DB is opened, so each case points `--db` at a
// path inside a directory that does not exist. Every invocation therefore
// fails, and it is the stderr that says which check fired: the transport
// error means the guard tripped, the db error means the guard let the
// configuration through. Nothing binds a socket or warms the embedder, so
// these stay fast and offline.

use std::process::{Command, Output};

fn start_with(llm_url: &str, key: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spelunk-server"));
    cmd.args([
        "--host",
        "127.0.0.1",
        "--port",
        "7777",
        "--db",
        "/nonexistent-spelunk-test-dir/server.db",
        "--llm-url",
        llm_url,
    ]);
    match key {
        Some(k) => cmd.env("SPELUNK_LLM_KEY", k),
        None => cmd.env_remove("SPELUNK_LLM_KEY"),
    };
    cmd.output().expect("spawning spelunk-server")
}

fn start_with_inline_key(llm_url: &str, key: &str) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spelunk-server"));
    cmd.args([
        "--host",
        "127.0.0.1",
        "--port",
        "7777",
        "--db",
        "/nonexistent-spelunk-test-dir/server.db",
        "--llm-url",
        llm_url,
        "--llm-key",
        key,
    ]);
    cmd.env_remove("SPELUNK_LLM_KEY");
    cmd.output().expect("spawning spelunk-server")
}

fn assert_reached_the_db(out: &Output, case: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("server db"),
        "{case}: startup should have got past the transport guard to the db open, \
         but stderr was: {stderr}"
    );
}

#[test]
fn a_key_over_plaintext_to_a_non_loopback_host_refuses_to_start() {
    let out = start_with("http://192.168.1.10:1234", Some("sk-llm-secret"));

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("192.168.1.10"),
        "the error must name the offending endpoint: {stderr}"
    );
    assert!(
        !stderr.contains("sk-llm-secret"),
        "the credential must never appear in output: {stderr}"
    );
}

#[test]
fn a_key_over_https_starts_normally() {
    let out = start_with("https://gateway.example", Some("sk-llm-secret"));
    assert_reached_the_db(&out, "https with a key");
}

#[test]
fn a_key_over_plaintext_loopback_starts_normally() {
    let out = start_with("http://127.0.0.1:1234", Some("sk-llm-secret"));
    assert_reached_the_db(&out, "loopback with a key");
}

#[test]
fn a_keyless_plaintext_non_loopback_endpoint_starts_normally() {
    let out = start_with("http://192.168.1.10:1234", None);
    assert_reached_the_db(&out, "non-loopback LAN endpoint without a key");
}

// The refusal path renders the offending URL, and the credential is in scope
// right beside it. An inline `--llm-key` is the one source that also sits in
// argv, so this covers both the message and anything that might echo the args.
#[test]
fn the_refusal_never_echoes_an_inline_key() {
    let out = start_with_inline_key("http://192.168.1.10:1234", "sk-inline-secret");

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stderr.contains("192.168.1.10"),
        "the error must name the offending endpoint: {stderr}"
    );
    assert!(
        !stderr.contains("sk-inline-secret") && !stdout.contains("sk-inline-secret"),
        "the credential must never appear in output: {stderr}{stdout}"
    );
}

// KNOWN GAP, reproducer only, tracked separately. `spelunk_core`'s loopback
// predicate matches the `127.` prefix on the raw authority and does not split
// off userinfo, so both hosts below read as loopback and the guard lets a
// configured credential leave in cleartext to a host the operator does not
// control. The flaw is in the shared predicate (it governs `server_url` the
// same way), not in anything this LLM work introduced, so it is filed rather
// than patched here. Ignored so the suite stays honest about being green:
// un-ignore it with the fix.
#[test]
#[ignore = "reproduces a known fail-open in the shared loopback predicate"]
fn a_key_over_a_host_that_merely_looks_like_loopback_is_refused() {
    for url in [
        "http://127.0.0.1.example.invalid",
        "http://127.0.0.1@example.invalid",
    ] {
        let out = start_with(url, Some("sk-llm-secret"));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("server db"),
            "{url} is not loopback, so a configured key must not be allowed over it: {stderr}"
        );
    }
}

// A scheme the guard does not recognise fails closed rather than being waved
// through as "not http, so not plaintext".
#[test]
fn a_key_over_an_unrecognised_scheme_is_refused() {
    for url in ["192.168.1.10:1234", "ws://192.168.1.10:1234"] {
        let out = start_with(url, Some("sk-llm-secret"));
        assert!(
            !out.status.success(),
            "{url} should not have been accepted with a key configured"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("server db"),
            "{url}: the guard should have fired before the db open, got: {stderr}"
        );
    }
}
