// `spelunk logout` credential-store scoping: each of the three forms must
// touch exactly one credential store and leave the other intact.
//
// The cloud token pair lives in the `[auth]` table of
// `~/.config/spelunk/config.toml`; per-origin self-hosted server keys live in
// the secret store (here the file store, pinned via `SPELUNK_SECRET_STORE=file`
// by `spelunk_bin_in`). These tests seed BOTH stores, run one `logout` form,
// and assert which store changed and which survived — the assertion the older
// server-key-only logout tests never made, which let `--server`/`--servers`
// silently wipe the cloud pair.
//
// Each assertion spawns the real binary against an isolated `HOME` /
// `SPELUNK_CONFIG_DIR`, so nothing here reaches the developer's real config or
// the OS keychain.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin_in;

use predicates::prelude::*;
use tempfile::TempDir;

// Pipe `key` to `spelunk auth set-key --server <server>` over stdin (the only
// supported way to set a per-origin key). Writes the secret store, not config.toml.
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

// Seed a complete cloud `[auth]` token pair into `config.toml` directly — the
// same on-disk shape `spelunk login` writes. `SPELUNK_CONFIG_DIR` (set by
// `spelunk_bin_in`) resolves to `<home>/.config/spelunk`, so this is exactly
// where the CLI reads and (on bare logout) rewrites it.
fn seed_cloud_auth(home: &std::path::Path) {
    let dir = home.join(".config").join("spelunk");
    std::fs::create_dir_all(&dir).expect("create config dir");
    std::fs::write(
        dir.join("config.toml"),
        "[auth]\n\
         access_token = \"at-cloud-secret\"\n\
         refresh_token = \"rt-cloud-secret\"\n\
         expires_at = 4000000000\n\
         org_id = \"org_test\"\n",
    )
    .expect("seed [auth] into config.toml");
}

// Current text of the seeded `config.toml` (empty string once the file has been
// rewritten with no `[auth]` table).
fn config_toml(home: &std::path::Path) -> String {
    std::fs::read_to_string(home.join(".config").join("spelunk").join("config.toml"))
        .unwrap_or_default()
}

// `logout --server <url>`: clears only that origin's server key. The cloud
// `[auth]` pair and every other origin's key must survive.
#[test]
fn logout_server_flag_clears_that_origin_only_and_keeps_cloud_pair() {
    let home = TempDir::new().unwrap();
    seed_cloud_auth(home.path());
    set_key(home.path(), "https://a.example:7777", "sk-a");
    set_key(home.path(), "https://b.example:7777", "sk-b");

    spelunk_bin_in(home.path())
        .arg("logout")
        .arg("--server")
        .arg("https://a.example:7777")
        .assert()
        .success();

    // Cloud pair untouched.
    let cfg = config_toml(home.path());
    assert!(
        cfg.contains("access_token") && cfg.contains("refresh_token"),
        "cloud [auth] pair must survive `logout --server`, config.toml was:\n{cfg}"
    );

    // Only the named origin cleared; the other survives.
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

// Bare `logout` (no flags): clears only the cloud `[auth]` pair. Every stored
// server key must survive.
#[test]
fn bare_logout_clears_cloud_pair_only_and_keeps_server_keys() {
    let home = TempDir::new().unwrap();
    seed_cloud_auth(home.path());
    set_key(home.path(), "https://a.example:7777", "sk-a");
    set_key(home.path(), "https://b.example:7777", "sk-b");

    spelunk_bin_in(home.path()).arg("logout").assert().success();

    // Cloud pair removed.
    let cfg = config_toml(home.path());
    assert!(
        !cfg.contains("access_token") && !cfg.contains("refresh_token"),
        "cloud [auth] pair must be cleared by bare `logout`, config.toml was:\n{cfg}"
    );

    // Both server keys survive.
    spelunk_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .stdout(predicate::str::contains("a.example"))
        .stdout(predicate::str::contains("b.example"));
}

// `logout --servers`: clears every stored server key. The cloud `[auth]` pair
// must survive.
#[test]
fn logout_servers_flag_clears_all_server_keys_and_keeps_cloud_pair() {
    let home = TempDir::new().unwrap();
    seed_cloud_auth(home.path());
    set_key(home.path(), "https://a.example:7777", "sk-a");
    set_key(home.path(), "https://b.example:7777", "sk-b");

    spelunk_bin_in(home.path())
        .arg("logout")
        .arg("--servers")
        .assert()
        .success();

    // Cloud pair untouched.
    let cfg = config_toml(home.path());
    assert!(
        cfg.contains("access_token") && cfg.contains("refresh_token"),
        "cloud [auth] pair must survive `logout --servers`, config.toml was:\n{cfg}"
    );

    // No server keys remain.
    spelunk_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .stdout(predicate::str::contains("No server keys stored"));
}
