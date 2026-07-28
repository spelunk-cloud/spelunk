// Real-process coverage for the removed --embedding-url/--embedding-model
// options, complementing the `Args::try_parse_from` unit tests in main.rs.
// A synthetic clap-only test proves the `Args` struct definition rejects the
// flag; it does not prove the compiled `spelunk-server` binary a user
// actually runs behaves the same way end to end (argv parsing, process exit
// code, stderr). This spawns the real binary via `CARGO_BIN_EXE_spelunk-server`.

use std::process::Command;

// `--print-openapi` exits before binding a socket or touching the embedder
// slot (see main.rs `run`), so it's a safe, fast, side-effect-free path for
// exercising real argv/env parsing without standing up a server.

#[test]
fn embedding_url_flag_rejected_by_real_binary() {
    let output = Command::new(env!("CARGO_BIN_EXE_spelunk-server"))
        .args([
            "--embedding-url",
            "http://127.0.0.1:1234",
            "--print-openapi",
        ])
        .output()
        .expect("spawning spelunk-server");

    assert!(
        !output.status.success(),
        "a real invocation with --embedding-url must fail, not silently accept it"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("embedding-url"),
        "stderr should name the rejected flag: {stderr}"
    );
}

#[test]
fn embedding_model_flag_rejected_by_real_binary() {
    let output = Command::new(env!("CARGO_BIN_EXE_spelunk-server"))
        .args(["--embedding-model", "some-model", "--print-openapi"])
        .output()
        .expect("spawning spelunk-server");

    assert!(
        !output.status.success(),
        "a real invocation with --embedding-model must fail, not silently accept it"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("embedding-model"),
        "stderr should name the rejected flag: {stderr}"
    );
}

#[test]
fn embedding_env_vars_are_inert_in_real_binary() {
    let output = Command::new(env!("CARGO_BIN_EXE_spelunk-server"))
        .env("SPELUNK_EMBEDDING_URL", "http://127.0.0.1:1234")
        .env("SPELUNK_EMBEDDING_MODEL", "some-model")
        .arg("--print-openapi")
        .output()
        .expect("spawning spelunk-server");

    assert!(
        output.status.success(),
        "env vars with no matching flag must not break a real invocation: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let _: serde_json::Value =
        serde_json::from_str(&stdout).expect("--print-openapi must still emit valid JSON");
}
