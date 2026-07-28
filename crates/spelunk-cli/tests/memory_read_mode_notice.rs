//! Read commands under a configured team `server_url`.
//!
//! With `server_url` set, the default `local_first` mode serves reads from the
//! local store. That is by design (offline-resilient; the background reconciler
//! converges the server replica), and the read commands must stay quiet about
//! it: no per-read manual-sync nag on stderr. These tests pin that silence and
//! the `cloud_first` counterpart: reads route to the server and an unreachable
//! server is a hard error, never a silent local read. They also cover the
//! neutral `spelunk status` mode word and its scope-aware offline hints.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin;

use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Title of the locally seeded entry; must never appear on stdout when reads
/// route to the server.
const LOCAL_TITLE: &str = "local only entry";

fn write_cfg(dir: &Path, name: &str, db_path: &Path, extra: &str) -> PathBuf {
    let cfg = format!(
        "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1\"\n\
         llm_model = \"test-chat\"\n{extra}",
        db_path
    );
    let path = dir.join(name);
    std::fs::write(&path, cfg).expect("write config");
    path
}

/// Seed one local memory entry (no `server_url`: solo/local write path) and
/// return `(tmp, mem_path, id)`.
fn seeded_project() -> (TempDir, PathBuf, i64) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let mem_path = db_path.with_file_name("memory.db");
    let cfg = write_cfg(tmp.path(), "config-seed.toml", &db_path, "");
    let out = spelunk_bin()
        // Not a git repo: the git-notes write-through is a no-op, so the entry
        // lands only in the local memory.db.
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args([
            "add",
            "--kind",
            "note",
            "--title",
            LOCAL_TITLE,
            "--body",
            "b",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // "Stored [note] #<id>: <title>"
    let id: i64 = stdout
        .split('#')
        .nth(1)
        .and_then(|s| s.split(':').next())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| panic!("could not parse stored id from: {stdout}"));
    (tmp, mem_path, id)
}

fn memory_list(tmp: &TempDir, mem_path: &Path, cfg: &Path) -> std::process::Output {
    spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(cfg)
        .args(["memory", "--db"])
        .arg(mem_path)
        .args(["list", "--format", "json"])
        .output()
        .unwrap()
}

/// stderr must never carry a manual-sync nag on a read; the background
/// reconciler owns convergence.
fn assert_no_sync_nag(stderr: &str) {
    assert!(
        !stderr.contains("spelunk sync"),
        "read must not nag about manual sync: {stderr}"
    );
    assert!(
        !stderr.contains("showing local data"),
        "read must not label local data on stderr: {stderr}"
    );
}

// ── local_first: data served, stdout machine-clean, stderr free of any nag ────

#[test]
fn local_first_read_serves_data_without_sync_nag() {
    let (tmp, mem_path, _id) = seeded_project();
    // Non-loopback https passes the transport guard; local_first never contacts
    // it, so the host being unresolvable is irrelevant (and proves no probe).
    let cfg = write_cfg(
        tmp.path(),
        "config-local-first.toml",
        &tmp.path().join("spelunk.db"),
        "",
    );
    // `server_url`/`project_id` only take effect from project-level
    // `.spelunk/config.toml` (`memory_list` sets `.current_dir(tmp.path())`).
    plumbing_helpers::write_project_server_config(
        tmp.path(),
        "https://team.invalid:7777",
        "team/proj",
    );

    let out = memory_list(&tmp, &mem_path, &cfg);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "expected exit 0; stderr: {stderr}");
    assert_no_sync_nag(&stderr);
    // Local data is still served, as pure JSON on stdout.
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be pure JSON");
    assert!(
        parsed.as_array().is_some_and(|a| !a.is_empty()),
        "expected the seeded entry on stdout: {stdout}"
    );
    assert!(stdout.contains(LOCAL_TITLE), "got: {stdout}");
}

/// ADR-037 P2 item 34: the new pending/last-synced clause belongs on `spelunk
/// status` only. No per-read banner is reintroduced on
/// `list`/`search`/`show`/`timeline`/`context` — extends this file's existing
/// `assert_no_sync_nag` coverage (recorded in commit `a44279e26`) to also
/// guard against the new content, not just the old removed nag.
#[test]
fn read_commands_never_print_pending_or_last_synced_banner() {
    let (tmp, mem_path, id) = seeded_project();
    let cfg = write_cfg(
        tmp.path(),
        "config-read-commands.toml",
        &tmp.path().join("spelunk.db"),
        "",
    );
    plumbing_helpers::write_project_server_config(
        tmp.path(),
        "https://team.invalid:7777",
        "team/proj",
    );

    let assert_clean = |out: &std::process::Output, label: &str| {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        for needle in ["pending", "last synced", "sync error", "spelunk sync"] {
            assert!(
                !stdout.contains(needle) && !stderr.contains(needle),
                "{label} must never mention {needle:?} (status-only content): \
                 stdout={stdout} stderr={stderr}"
            );
        }
    };

    let list = memory_list(&tmp, &mem_path, &cfg);
    assert_clean(&list, "memory list");

    let show = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["show", &id.to_string()])
        .output()
        .unwrap();
    assert_clean(&show, "memory show");

    let search = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["search", LOCAL_TITLE, "--mode", "text"])
        .output()
        .unwrap();
    assert_clean(&search, "memory search");

    let timeline = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["timeline", LOCAL_TITLE])
        .output()
        .unwrap();
    assert_clean(&timeline, "memory timeline");

    let context = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["context", "--db"])
        .arg(&mem_path)
        .output()
        .unwrap();
    assert_clean(&context, "context");
}

// ── cloud_first: unreachable server = hard error, local data never printed ────

#[test]
fn cloud_first_read_unreachable_server_errors_without_local_data() {
    let (tmp, mem_path, _id) = seeded_project();
    // Loopback http passes the transport guard; nothing listens on port 1, so
    // the read must fail. A raw-UUID project_id skips slug resolution, proving
    // the failure is the memory read itself. `mode` isn't a `ProjectConfig`
    // field, so it stays in the global file; `server_url`/`project_id` only
    // take effect from project-level `.spelunk/config.toml`.
    let cfg = write_cfg(
        tmp.path(),
        "config-cloud-first.toml",
        &tmp.path().join("spelunk.db"),
        "mode = \"cloud_first\"\n",
    );
    plumbing_helpers::write_project_server_config(
        tmp.path(),
        "http://127.0.0.1:1",
        "11111111-1111-1111-1111-111111111111",
    );

    let out = memory_list(&tmp, &mem_path, &cfg);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !out.status.success(),
        "cloud_first read against an unreachable server must exit non-zero; \
         stdout: {stdout}"
    );
    // The one unacceptable outcome: silently substituting local data.
    assert!(
        !stdout.contains(LOCAL_TITLE),
        "local data must never be printed when reads route to the server: {stdout}"
    );
    // The error names the failed operation and carries the source chain
    // (anyhow context + reqwest cause).
    assert!(
        stderr.contains("GET /memory"),
        "error must name the failed server read: {stderr}"
    );
    assert!(
        stderr.contains("Caused by"),
        "error must carry the cause chain: {stderr}"
    );
}

// ── spelunk status: neutral mode word + scope-aware offline hints ─────────────

/// Minimal indexed project so `spelunk status` passes the ADR-067 project
/// gate. Indexed with SPELUNK_NO_SERVER=1 (no embed phase, no probes).
fn indexed_project() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("lib.rs"), "pub fn hello() {}").unwrap();
    let db_path = tmp.path().join("index.db");
    let cfg = write_cfg(tmp.path(), "config-index.toml", &db_path, "");
    spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&cfg)
        .arg("index")
        .arg(&project)
        .assert()
        .success();
    (tmp, project)
}

#[test]
fn status_shows_neutral_mode_and_truthful_hints_with_unreachable_server_url() {
    let (tmp, project) = indexed_project();
    // Loopback https passes the transport guard; nothing listens on port 1, so
    // the tier probe fails fast and the tier is Offline with server_url SET.
    let cfg = write_cfg(
        tmp.path(),
        "config-team.toml",
        &tmp.path().join("index.db"),
        "",
    );
    // `server_url`/`project_id` only take effect from project-level
    // `.spelunk/config.toml`; the `status` command below runs with
    // `.current_dir(&project)`, so it must land there, not under `tmp.path()`.
    plumbing_helpers::write_project_server_config(&project, "https://127.0.0.1:1", "team/proj");

    let out = spelunk_bin()
        .current_dir(&project)
        .arg("--config")
        .arg(&cfg)
        .arg("status")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout: {stdout}");

    // A neutral one-word mode indicator, with no manual-sync call to action.
    assert!(stdout.contains("mode"), "got: {stdout}");
    assert!(stdout.contains("local_first"), "got: {stdout}");
    assert!(
        !stdout.contains("spelunk sync"),
        "status must not pre-teach a manual sync workflow: {stdout}"
    );
    // Explore's hint must not tell the operator to set an already-set server_url.
    assert!(
        stdout.contains("configured server unreachable"),
        "got: {stdout}"
    );
    assert!(
        !stdout.contains("set server_url to enable]"),
        "explore hint must not suggest setting an already-set server_url: {stdout}"
    );
}

#[test]
fn status_has_no_mode_line_on_solo_default() {
    let (tmp, project) = indexed_project();
    let cfg = write_cfg(
        tmp.path(),
        "config-solo-status.toml",
        &tmp.path().join("index.db"),
        "",
    );

    let out = spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1") // hermetic: no loopback auto-discovery
        .current_dir(&project)
        .arg("--config")
        .arg(&cfg)
        .arg("status")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout: {stdout}");

    // Solo default: no sync configuration, no mode line.
    assert!(!stdout.contains("\n  mode"), "got: {stdout}");
    assert!(!stdout.contains("local_first"), "got: {stdout}");
    // And the set-server_url hints ARE correct here.
    assert!(stdout.contains("set server_url to enable"), "got: {stdout}");
}
