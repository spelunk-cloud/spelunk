// What a spawned daemon is actually handed, observed through the real CLI.
//
// The unit tests on either side of the process boundary pin what an already
// resolved `LlmSpawn` renders to, and what `spelunk-server` parses. Neither
// can see a call site that stops resolving, nor a variable the child inherits
// behind the CLI's back: both live exactly at the boundary. These run
// `spelunk server start` against a recording stand-in for the daemon binary
// and assert on the argv and environment that stand-in received.

#![cfg(unix)]

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin_in;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tempfile::TempDir;

// argv (as one line) and environment of the process the CLI spawned.
struct Spawned {
    argv: String,
    env: Vec<String>,
}

impl Spawned {
    fn env_value(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .find_map(|l| l.strip_prefix(&format!("{name}=")))
    }
}

// A stand-in for `spelunk-server` that records how it was invoked and exits.
//
// It never binds the port, so the CLI's start path sees the process end
// without serving; that is the fast path and is what the exit-aware wait was
// added for.
fn recording_server(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let record = dir.join("record.txt");
    let bin = dir.join("recording-spelunk-server");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\n{{ echo \"ARGV $*\"; env; }} > '{}'\n",
            record.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, record)
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

// Run `spelunk server start` with `config_toml` as the personal config, and
// return what the daemon stand-in was handed.
fn start_daemon(config_toml: &str, env: &[(&str, &str)], extra_args: &[&str]) -> Spawned {
    let home = TempDir::new().unwrap().keep();
    let (bin, record) = recording_server(&home);
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, config_toml).unwrap();

    let mut cmd = spelunk_bin_in(&home);
    cmd.env("SPELUNK_STATE_DIR", home.join("state"))
        .env_remove("SPELUNK_LLM_URL")
        .env_remove("SPELUNK_LLM_MODEL")
        .env_remove("SPELUNK_LLM_KEY");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd
        .arg("--config")
        .arg(&config_path)
        .args(["server", "start", "--port"])
        .arg(free_port().to_string())
        .arg("--bin")
        .arg(&bin)
        .arg("--db")
        .arg(home.join("server.db"))
        .args(extra_args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let recorded = std::fs::read_to_string(&record)
        .unwrap_or_else(|e| panic!("the daemon stand-in recorded nothing ({e})"));
    let mut lines = recorded.lines().map(str::to_string);
    let argv = lines.next().unwrap_or_default();
    Spawned {
        argv,
        env: lines.collect(),
    }
}

// The whole point of the story: a value in the personal config has to arrive
// at a daemon this CLI starts. Nothing else in the suite crosses that gap.
#[test]
fn a_configured_endpoint_reaches_the_spawned_daemon() {
    let spawned = start_daemon(
        "llm_url = \"http://endpoint.invalid:1234\"\nllm_model = \"from-config\"\n",
        &[],
        &[],
    );

    assert!(
        spawned
            .argv
            .contains("--llm-url http://endpoint.invalid:1234"),
        "the configured endpoint never reached the daemon: {}",
        spawned.argv
    );
    assert!(
        spawned.argv.contains("--llm-model from-config"),
        "the configured model never reached the daemon: {}",
        spawned.argv
    );
}

#[test]
fn an_environment_endpoint_reaches_the_spawned_daemon() {
    let spawned = start_daemon(
        "",
        &[
            ("SPELUNK_LLM_URL", "http://from-env.invalid:1234"),
            ("SPELUNK_LLM_MODEL", "from-env"),
        ],
        &[],
    );

    assert!(
        spawned
            .argv
            .contains("--llm-url http://from-env.invalid:1234"),
        "got {}",
        spawned.argv
    );
    assert!(
        spawned.argv.contains("--llm-model from-env"),
        "got {}",
        spawned.argv
    );
}

// The flags are documented as outranking both lower sources for one daemon.
#[test]
fn the_start_flags_outrank_both_the_environment_and_the_config() {
    let spawned = start_daemon(
        "llm_url = \"http://from-config.invalid:1234\"\nllm_model = \"from-config\"\n",
        &[
            ("SPELUNK_LLM_URL", "http://from-env.invalid:1234"),
            ("SPELUNK_LLM_MODEL", "from-env"),
        ],
        &[
            "--llm-url",
            "http://from-flag.invalid:1234",
            "--llm-model",
            "from-flag",
        ],
    );

    assert!(
        spawned
            .argv
            .contains("--llm-url http://from-flag.invalid:1234"),
        "the flag must win: {}",
        spawned.argv
    );
    assert!(
        spawned.argv.contains("--llm-model from-flag"),
        "the flag must win: {}",
        spawned.argv
    );
    assert!(
        !spawned.argv.contains("from-env") && !spawned.argv.contains("from-config"),
        "an outranked value must not reach the daemon at all: {}",
        spawned.argv
    );
    assert_eq!(
        spawned.env_value("SPELUNK_LLM_URL"),
        Some("http://from-flag.invalid:1234"),
        "the child's inherited variable must be replaced, not left as the parent's"
    );
    assert_eq!(spawned.env_value("SPELUNK_LLM_MODEL"), Some("from-flag"));
}

// An exported empty value is an override that blanks the configured endpoint,
// so the daemon must start with no LLM at all. The CLI omitting the argument
// is not enough: `spelunk-server` reads `SPELUNK_LLM_URL` through clap `env`,
// so an inherited empty value arrives as a present-but-empty endpoint, which
// is either a daemon advertising an LLM it cannot reach or, with a credential
// configured, a daemon that refuses to start.
#[test]
fn an_exported_empty_endpoint_leaves_the_daemon_with_no_llm_at_all() {
    let spawned = start_daemon(
        "llm_url = \"http://from-config.invalid:1234\"\nllm_model = \"from-config\"\n",
        &[("SPELUNK_LLM_URL", ""), ("SPELUNK_LLM_MODEL", "")],
        &[],
    );

    assert!(
        !spawned.argv.contains("--llm-url"),
        "a blanked endpoint must emit no argument: {}",
        spawned.argv
    );
    assert_eq!(
        spawned.env_value("SPELUNK_LLM_URL"),
        None,
        "the child inherited the blank endpoint, which its own clap env binding \
         then reads as a configured one"
    );
    assert_eq!(spawned.env_value("SPELUNK_LLM_MODEL"), None);
}

// A model with no endpoint is not a configuration, on either channel.
#[test]
fn a_model_without_an_endpoint_reaches_the_daemon_on_neither_channel() {
    let spawned = start_daemon("llm_model = \"orphan\"\n", &[], &[]);

    assert!(
        !spawned.argv.contains("--llm-model"),
        "got {}",
        spawned.argv
    );
    assert_eq!(spawned.env_value("SPELUNK_LLM_MODEL"), None);
    assert_eq!(spawned.env_value("SPELUNK_LLM_URL"), None);
}

// The credential travels in the environment and only there, whatever else is
// configured. Asserted on the whole argv, not on a flag name.
#[test]
fn the_credential_reaches_the_child_environment_and_never_its_argv() {
    let spawned = start_daemon(
        "llm_url = \"http://endpoint.invalid:1234\"\n",
        &[("SPELUNK_LLM_KEY", "sk-endpoint-credential")],
        &[],
    );

    assert_eq!(
        spawned.env_value("SPELUNK_LLM_KEY"),
        Some("sk-endpoint-credential")
    );
    assert!(
        !spawned.argv.contains("sk-endpoint-credential"),
        "the credential must never reach the process table: {}",
        spawned.argv
    );
}

// A daemon that exits immediately rejected its own configuration. Blaming a
// firewall for that sends the user to the wrong place, and waiting out the
// full liveness timeout first makes it worse.
#[test]
fn a_daemon_that_exits_immediately_is_not_reported_as_a_firewall_problem() {
    let home = TempDir::new().unwrap().keep();
    let (bin, _record) = recording_server(&home);
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, "").unwrap();

    let started = std::time::Instant::now();
    let out = spelunk_bin_in(&home)
        .env("SPELUNK_STATE_DIR", home.join("state"))
        .arg("--config")
        .arg(&config_path)
        .args(["server", "start", "--port"])
        .arg(free_port().to_string())
        .arg("--bin")
        .arg(&bin)
        .arg("--db")
        .arg(home.join("server.db"))
        .output()
        .unwrap();
    let elapsed = started.elapsed();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("firewall"),
        "the process is gone, so the network is not the diagnosis: {stderr}"
    );
    assert!(
        stderr.contains("exited immediately"),
        "the user needs to be sent to the log, not to their firewall settings: {stderr}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "waiting out the full liveness timeout for a process already gone: {elapsed:?}"
    );
}
