// `spelunk auth set-key --llm`: storing the credential for the configured LLM
// endpoint.
//
// Drives the real binary against an isolated HOME (`spelunk_bin_in` forces
// `SPELUNK_SECRET_STORE=file`), so nothing here reaches the developer's real
// config dir or the OS keychain.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin_in;

use predicates::prelude::*;
use tempfile::TempDir;

// Read the file-backed secret store directly: there is deliberately no
// command that prints key material back out.
fn stored_secret(home: &std::path::Path, key: &str) -> Option<String> {
    use spelunk_core::config::secret_store::{FileStore, SecretStore};
    let path = home.join(".config").join("spelunk").join("secrets.toml");
    FileStore::new(path).get(key).unwrap()
}

fn set_llm_key(home: &std::path::Path, key: &str) -> assert_cmd::assert::Assert {
    spelunk_bin_in(home)
        .arg("auth")
        .arg("set-key")
        .arg("--llm")
        .write_stdin(format!("{key}\n"))
        .assert()
}

#[test]
fn set_key_llm_stores_the_key_verbatim() {
    let home = TempDir::new().unwrap();
    set_llm_key(home.path(), "sk-llm-secret").success();

    assert_eq!(
        stored_secret(home.path(), "llm_key").as_deref(),
        Some("sk-llm-secret")
    );
}

#[test]
fn set_key_llm_rejects_blank_stdin_and_stores_nothing() {
    let home = TempDir::new().unwrap();
    spelunk_bin_in(home.path())
        .arg("auth")
        .arg("set-key")
        .arg("--llm")
        .write_stdin("   \n")
        .assert()
        .failure();

    assert_eq!(stored_secret(home.path(), "llm_key"), None);
}

#[test]
fn set_key_llm_never_echoes_the_key() {
    let home = TempDir::new().unwrap();
    set_llm_key(home.path(), "sk-llm-secret")
        .success()
        .stdout(predicate::str::contains("sk-llm-secret").not())
        .stderr(predicate::str::contains("sk-llm-secret").not());
}

#[test]
fn set_key_llm_leaves_the_server_key_map_untouched() {
    let home = TempDir::new().unwrap();
    spelunk_bin_in(home.path())
        .arg("auth")
        .arg("set-key")
        .arg("--server")
        .arg("https://team.example:7777")
        .write_stdin("sk-team\n")
        .assert()
        .success();
    let before = stored_secret(home.path(), "server_keys");

    set_llm_key(home.path(), "sk-llm-secret").success();

    assert_eq!(
        stored_secret(home.path(), "server_keys"),
        before,
        "storing an LLM key must not rewrite the per-origin server-key map"
    );
    assert!(before.is_some(), "the server key should have been stored");
}

#[test]
fn set_key_server_does_not_write_an_llm_key() {
    let home = TempDir::new().unwrap();
    spelunk_bin_in(home.path())
        .arg("auth")
        .arg("set-key")
        .arg("--server")
        .arg("https://team.example:7777")
        .write_stdin("sk-team\n")
        .assert()
        .success();

    assert_eq!(stored_secret(home.path(), "llm_key"), None);
}

#[test]
fn set_key_rejects_both_llm_and_server() {
    let home = TempDir::new().unwrap();
    spelunk_bin_in(home.path())
        .arg("auth")
        .arg("set-key")
        .arg("--llm")
        .arg("--server")
        .arg("https://team.example:7777")
        .write_stdin("sk-team\n")
        .assert()
        .failure();
}

#[test]
fn set_key_rejects_neither_llm_nor_server() {
    let home = TempDir::new().unwrap();
    spelunk_bin_in(home.path())
        .arg("auth")
        .arg("set-key")
        .write_stdin("sk-team\n")
        .assert()
        .failure();
}

// The whole point of the stdin-only channel is that argv is world-readable
// through the process table and lands in shell history. A user who reaches for
// the obvious `--llm <key>` must be refused, not quietly obeyed.
#[test]
fn set_key_llm_refuses_a_key_passed_as_an_argument() {
    let home = TempDir::new().unwrap();
    for form in ["sk-llm-secret", "--llm=sk-llm-secret"] {
        spelunk_bin_in(home.path())
            .arg("auth")
            .arg("set-key")
            .arg("--llm")
            .arg(form)
            .write_stdin("")
            .assert()
            .failure();
    }

    assert_eq!(stored_secret(home.path(), "llm_key"), None);
}

// The credential belongs in the secret store alone. `config.toml` is the file
// users copy into dotfiles repos, which is the leak this store exists to close.
#[test]
fn set_key_llm_writes_nothing_into_the_config_file() {
    let home = TempDir::new().unwrap();
    let config = home
        .path()
        .join(".config")
        .join("spelunk")
        .join("config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "llm_url = \"http://127.0.0.1:1234\"\n").unwrap();

    set_llm_key(home.path(), "sk-llm-secret").success();

    let after = std::fs::read_to_string(&config).unwrap();
    assert!(
        !after.contains("sk-llm-secret"),
        "the credential must never reach config.toml: {after}"
    );
    assert!(
        after.contains("llm_url"),
        "the rest of the config must survive: {after}"
    );
}

#[test]
fn list_servers_ignores_a_stored_llm_key() {
    let home = TempDir::new().unwrap();
    set_llm_key(home.path(), "sk-llm-secret").success();

    spelunk_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .stdout(predicate::str::contains("No server keys stored"))
        .stdout(predicate::str::contains("sk-llm-secret").not());
}
