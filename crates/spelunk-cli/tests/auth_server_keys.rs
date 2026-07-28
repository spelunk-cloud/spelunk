//! `spelunk auth set-key` / `spelunk auth list-servers` / the `spelunk logout`
//! server-key scoping correction (ADR-071 D1/D3).
//!
//! Drives the real binary end to end against an isolated `HOME` (via
//! `spelunk_bin_in`, `SPELUNK_SECRET_STORE=file`) so these tests never touch
//! the developer's real `~/.config/spelunk` or the OS keychain, and so
//! `auth set-key`'s persisted key survives across the separate process spawns
//! each assertion below makes.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin_in;

use predicates::prelude::*;
use tempfile::TempDir;

/// Pipe `key` to `spelunk auth set-key --server <server>` over stdin: the
/// only supported way to set a key (never argv).
fn set_key(home: &std::path::Path, server: &str, key: &str) {
    spelunk_bin_in(home)
        .arg("auth")
        .arg("set-key")
        .arg("--server")
        .arg(server)
        .write_stdin(format!("{key}\n"))
        .assert()
        .success();
}

#[test]
fn set_key_then_list_servers_shows_the_origin_not_the_secret() {
    let home = TempDir::new().unwrap();
    set_key(
        home.path(),
        "https://team.example:7777/ignored/path",
        "sk-team-secret",
    );

    spelunk_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .stdout(predicate::str::contains("https://team.example:7777"))
        .stdout(predicate::str::contains("sk-team-secret").not());
}

#[test]
fn list_servers_with_nothing_stored_says_so() {
    let home = TempDir::new().unwrap();
    spelunk_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .stdout(predicate::str::contains("No server keys stored"));
}

#[test]
fn set_key_rejects_empty_stdin() {
    let home = TempDir::new().unwrap();
    spelunk_bin_in(home.path())
        .arg("auth")
        .arg("set-key")
        .arg("--server")
        .arg("https://team.example:7777")
        .write_stdin("")
        .assert()
        .failure();
}

#[test]
fn set_key_normalizes_origin_so_a_second_call_overwrites_not_duplicates() {
    let home = TempDir::new().unwrap();
    set_key(home.path(), "https://team.example:7777/a/b?x=1", "sk-1");
    set_key(home.path(), "https://team.example:7777/", "sk-2");

    let out = spelunk_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    // Exactly one origin line, not two.
    assert_eq!(
        text.lines().filter(|l| l.contains("team.example")).count(),
        1,
        "two URL forms of the same origin must collapse to one entry, got:\n{text}"
    );
}

// ── `spelunk logout` server-key scoping (D3 founder correction) ────────────

#[test]
fn bare_logout_does_not_clear_stored_server_keys() {
    let home = TempDir::new().unwrap();
    set_key(home.path(), "https://team.example:7777", "sk-team-secret");

    spelunk_bin_in(home.path()).arg("logout").assert().success();

    // The server key must survive a bare logout: only the cloud [auth] pair
    // is an unconditional clear target.
    spelunk_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .stdout(predicate::str::contains("https://team.example:7777"));
}

#[test]
fn logout_servers_flag_clears_all_stored_server_keys() {
    let home = TempDir::new().unwrap();
    set_key(home.path(), "https://a.example:7777", "sk-a");
    set_key(home.path(), "https://b.example:7777", "sk-b");

    spelunk_bin_in(home.path())
        .arg("logout")
        .arg("--servers")
        .assert()
        .success();

    spelunk_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .stdout(predicate::str::contains("No server keys stored"));
}

#[test]
fn logout_server_flag_clears_only_that_one_origin() {
    let home = TempDir::new().unwrap();
    set_key(home.path(), "https://a.example:7777", "sk-a");
    set_key(home.path(), "https://b.example:7777", "sk-b");

    spelunk_bin_in(home.path())
        .arg("logout")
        .arg("--server")
        .arg("https://a.example:7777")
        .assert()
        .success();

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
        !text.contains("a.example"),
        "cleared origin still listed:\n{text}"
    );
    assert!(
        text.contains("b.example"),
        "untouched origin missing:\n{text}"
    );
}

#[test]
fn logout_servers_and_server_flags_are_mutually_exclusive() {
    let home = TempDir::new().unwrap();
    spelunk_bin_in(home.path())
        .arg("logout")
        .arg("--servers")
        .arg("--server")
        .arg("https://a.example")
        .assert()
        .failure();
}
