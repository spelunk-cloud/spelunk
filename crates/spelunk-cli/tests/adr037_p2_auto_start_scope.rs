//! ADR-037 P2 D6: auto-start scope for the local relay nudge
//! (`crate::cli::cmd::memory::outbox::nudge_after_write`).
//!
//! `spelunk memory add`/`archive`/`supersede` may opportunistically
//! auto-start the local `spelunk-server` daemon so the outbox drains
//! promptly, but ONLY for an interactive (TTY) `local_first` write (item 24).
//! Every invocation here runs through `assert_cmd`, whose child process gets
//! piped (non-TTY) stdin — the same mechanism `test_init_non_tty_prints_skip_notice`
//! in `e2e_cli.rs` already relies on for `init`'s identical gate — so these
//! tests exercise the non-interactive side of the gate (items 25/26/27/29)
//! directly and truthfully, not simulated.
//!
//! Detection signal: `ensure_server_running` calls `create_state_dir` (which
//! creates `~/.local/state/spelunk/`, `0700`) before it does anything else,
//! including before it even looks for the `spelunk-server` binary. So "the
//! state dir was never created" is a direct, positive proxy for "auto-start
//! was never attempted" — stronger than merely checking for a `server.pid`
//! file, which would also be absent if the gate were broken but the binary
//! lookup happened to fail first.

mod plumbing_helpers;
use plumbing_helpers::{init_git_repo, spelunk_bin_in, write_project_server_config};

use std::path::Path;
use tempfile::TempDir;

fn state_dir_under(home: &Path) -> std::path::PathBuf {
    home.join(".local").join("state").join("spelunk")
}

/// Seed one local memory entry via a config with the given `mode` (or the
/// `local_first` default when `mode` is `None`), then assert the state dir
/// was never created — i.e. the auto-start path was never even attempted.
fn assert_write_never_auto_starts(mode_toml: &str) {
    let home = TempDir::new().unwrap().keep();
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mem_path = project.join("memory.db");
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, "").unwrap();

    write_project_server_config(&project, "https://team.invalid:7777", "team/proj");
    if !mode_toml.is_empty() {
        let cfg_path = project.join(".spelunk").join("config.toml");
        let mut existing = std::fs::read_to_string(&cfg_path).unwrap();
        existing.push_str(mode_toml);
        std::fs::write(&cfg_path, existing).unwrap();
    }

    assert!(
        !state_dir_under(&home).exists(),
        "precondition: state dir must not exist before the write"
    );

    let out = spelunk_bin_in(&home)
        .current_dir(&project)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["add", "--kind", "note", "--title", "T", "--body", "b"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !state_dir_under(&home).exists(),
        "a non-interactive write must never auto-start the local server \
         (state dir would exist if it had): {}",
        state_dir_under(&home).display()
    );
}

// ── item 25: non-interactive (piped stdin, as every subprocess test here is) ──

#[test]
fn non_interactive_local_first_write_never_auto_starts() {
    assert_write_never_auto_starts("");
}

// ── item 26: mode = offline never auto-starts, regardless of TTY ───────────

#[test]
fn offline_mode_write_never_auto_starts() {
    assert_write_never_auto_starts("mode = \"offline\"\n");
}

// ── item 29: cloud_first never triggers the new interactive auto-start path ─
//
// A `cloud_first` write contacts the server synchronously per-invocation by
// definition (a different, pre-existing code path via `RemoteMemoryBackend`),
// so it must not ALSO trigger the new local-relay auto-start.

#[test]
fn cloud_first_mode_write_never_auto_starts() {
    let home = TempDir::new().unwrap().keep();
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mem_path = project.join("memory.db");
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, "").unwrap();

    // Loopback http passes the transport guard so the write reaches the wire
    // (and fails there, since nothing listens on port 1) rather than being
    // rejected at config validation.
    write_project_server_config(&project, "http://127.0.0.1:1", "team/proj");
    let cfg_path = project.join(".spelunk").join("config.toml");
    let mut existing = std::fs::read_to_string(&cfg_path).unwrap();
    existing.push_str("mode = \"cloud_first\"\n");
    std::fs::write(&cfg_path, existing).unwrap();

    // The write itself is expected to fail (unreachable server, cloud_first
    // has no local fallback) — irrelevant to this test, which only checks
    // that no auto-start was attempted either way.
    let _ = spelunk_bin_in(&home)
        .current_dir(&project)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["add", "--kind", "note", "--title", "T", "--body", "b"])
        .output()
        .unwrap();

    assert!(
        !state_dir_under(&home).exists(),
        "cloud_first must never trigger the local_first-only auto-start path"
    );
}

// ── item 27: SPELUNK_NO_SERVER=1 is a hard kill-switch regardless of TTY ────

#[test]
fn spelunk_no_server_env_write_never_auto_starts() {
    let home = TempDir::new().unwrap().keep();
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mem_path = project.join("memory.db");
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, "").unwrap();
    write_project_server_config(&project, "https://team.invalid:7777", "team/proj");

    let out = spelunk_bin_in(&home)
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(&project)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["add", "--kind", "note", "--title", "T", "--body", "b"])
        .output()
        .unwrap();
    assert!(out.status.success());

    assert!(
        !state_dir_under(&home).exists(),
        "SPELUNK_NO_SERVER=1 must never auto-start, matching the existing hard kill-switch"
    );
}

// ── item 7: no SYNC network call to the team server_url in the write's own
// stack, even when server_url is reachable ─────────────────────────────────
//
// Pins today's baseline (`memory_add` never contacts the network for SYNC
// under `local_first`) directly against a real mock server standing in for
// `server_url`, so P2's background machinery (the local relay, the
// interactive auto-start probe) provably never creeps into the write path
// itself: the write's own call stack only ever reaches the LOCAL loopback
// relay (absent here, so even that is a no-op), never the team server's sync
// endpoints (`/memory/batch`, `/memory/since`).
//
// Not asserting *zero* requests overall: `memory add` under `local_first`
// with a reachable `server_url` legitimately calls `/v1/health` and
// `/index/embed` today, pre-P2 and unrelated to sync — ADR-004's inference
// routing (`capability::get_tier`/`try_embed_via_server`), a documented,
// orthogonal concern ("Inference vs. memory storage are separate concerns").
// This test's job is to prove P2 added no *sync* traffic to that stack, not
// to relitigate the pre-existing inference call.

#[tokio::test]
async fn write_never_makes_a_sync_call_to_server_url_even_when_it_is_reachable() {
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let team_server = MockServer::start().await;
    // Answer everything generically (health probe, embed) so the write
    // completes normally; only the sync paths are asserted against below.
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok", "version": "test", "capabilities": ["memory"]
        })))
        .mount(&team_server)
        .await;

    let home = TempDir::new().unwrap().keep();
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mem_path = project.join("memory.db");
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, "").unwrap();
    write_project_server_config(&project, &team_server.uri(), "team/proj");

    let out = spelunk_bin_in(&home)
        .current_dir(&project)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["add", "--kind", "note", "--title", "T", "--body", "b"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let received = team_server.received_requests().await.unwrap();
    let sync_reqs: Vec<_> = received
        .iter()
        .filter(|r| {
            r.url.path().contains("/memory/batch") || r.url.path().contains("/memory/since")
        })
        .collect();
    assert!(
        sync_reqs.is_empty(),
        "the write's own call stack must never reach the team server's sync \
         endpoints directly (no local relay was running to hand off to): {:?}",
        received.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );
}

// ── nudge must never open a placeholder mem_path ────────────────────────────
//
// `memory add --backend git-notes` in a git repo with no local `.spelunk/`
// project resolves `mem_path` to a placeholder that `resolve_memory_store`'s
// own doc comment says "pre-init callers never open"
// (`crates/spelunk-cli/src/cli/cmd/memory/mod.rs`). The write itself correctly
// goes to git notes, not that path. Before this test, the post-write nudge
// gate only checked `pre_init_notes`, not `placeholder_path`, so under a
// `local_first` + `server_url` config, with a local relay already running
// (`probe_local_relay_port` must find one reachable before `register_and_push`
// is ever reached), it would still call `MemoryStore::open(mem_path)` on the
// placeholder, which unconditionally creates the parent directory and an
// empty `memory.db` file there, a phantom SQLite store for a project that
// deliberately has none. A real running relay is required to actually reach
// that call: without one, `probe_local_relay_port` returns `None` and the
// nudge is already a no-op regardless of this gate, which would make the test
// pass vacuously.

// `multi_thread`: the test blocks its own thread on a synchronous
// `Command::output()` for the CLI subprocess below, so the in-process relay
// server spawned via `tokio::spawn` needs a separate worker thread to keep
// actually servicing the CLI's health-probe HTTP request while that call
// blocks; on the default single-threaded flavor the probe would starve and
// `probe_local_relay_port` would spuriously return `None`, making this test
// pass vacuously regardless of the gate under test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_git_notes_backend_pre_init_never_creates_a_phantom_memory_db() {
    use std::sync::Arc;

    #[allow(clippy::missing_transmute_annotations)]
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    }

    let home = TempDir::new().unwrap().keep();
    let repo = home.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    // A real local relay (spelunk-server's production router), the same
    // instance `probe_local_relay_port`/`register_and_push` would talk to.
    let db_dir = TempDir::new().unwrap();
    let db = spelunk_server::db::ServerDb::open(&db_dir.path().join("server.db"), 4, "test-model")
        .unwrap();
    let instance_id = db.get_or_create_instance_id().unwrap();
    let state = spelunk_server::AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        auth: Arc::new(spelunk_server::auth::ApiKeyAuth::new(None)),
        conflict_threshold: spelunk_server::default_conflict_threshold(),
        embedder: spelunk_server::EmbedderSlot::disabled(),
        embed_admission: spelunk_server::EmbedAdmission::new(
            spelunk_server::EMBED_QUEUE_CAPACITY,
            spelunk_server::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        llm: None,
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(spelunk_server::rate_limiter::RateLimiter::new(1000, 60)),
        instance_id,
        started_by: None,
        relay: spelunk_server::relay::RelayRegistry::new(),
    };
    let app = spelunk_server::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let state_dir = home.join(".local").join("state").join("spelunk");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(state_dir.join("server.port"), format!("{relay_port}\n")).unwrap();

    let out = spelunk_bin_in(&home)
        .current_dir(&repo)
        .env("SPELUNK_SERVER_URL", "https://team.invalid:7777")
        .env("SPELUNK_PROJECT_ID", "team/proj")
        .env("SPELUNK_MODE", "local_first")
        .args([
            "memory",
            "add",
            "--backend",
            "git-notes",
            "--kind",
            "note",
            "--title",
            "T",
            "--body",
            "b",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Give the (best-effort, fire-and-forget) nudge a moment to reach
    // `MemoryStore::open` if the gate under test is broken, before asserting
    // its absence — otherwise a broken gate could race this check and still
    // pass.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    assert!(
        !repo.join(".spelunk").exists(),
        "explicit --backend git-notes with no local project must never create \
         a .spelunk/ project as a side effect of the post-write relay nudge"
    );
    // The placeholder `mem_path` resolves to `cfg.db_path.with_file_name(...)`
    // (the global, no-project default under `SPELUNK_CONFIG_DIR`), not
    // anywhere under `repo`: this is the actual file `MemoryStore::open`
    // would create if the nudge gate let a placeholder path through.
    assert!(
        !home
            .join(".config")
            .join("spelunk")
            .join("memory.db")
            .exists(),
        "must never create a phantom global memory.db as a side effect of an \
         explicit --backend git-notes write with no local project"
    );
}

// ── item 8/10: the write itself is unaffected either way ───────────────────

#[test]
fn write_still_commits_and_stays_outbox_pending_when_no_auto_start_happens() {
    let home = TempDir::new().unwrap().keep();
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mem_path = project.join("memory.db");
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, "").unwrap();
    write_project_server_config(&project, "https://team.invalid:7777", "team/proj");

    let out = spelunk_bin_in(&home)
        .current_dir(&project)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["add", "--kind", "note", "--title", "T", "--body", "b"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Stored"),
        "the write must commit locally regardless of relay reachability"
    );

    let out = spelunk_bin_in(&home)
        .current_dir(&project)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["list", "--format", "json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert!(parsed.as_array().is_some_and(|a| a.len() == 1));
}

// ── item 43: read-command convergence (`memory list`) never auto-starts ────
//
// items 42-47 extend the ADR-037 P2 relay poll to `memory list`/`search`/
// `show`/`timeline`/`spelunk context` so live-pulled entries converge on
// reads, not just `spelunk status`. Item 43 requires this to be strictly
// poll-if-already-running: it must reuse `probe_local_relay_port` (a passive
// check) exactly like `spelunk status` already does, never
// `ensure_server_running`. `memory list` stands in for all five call sites
// here since they all route through the same `outbox::poll_and_apply`.

#[test]
fn memory_list_never_auto_starts_the_local_server() {
    let home = TempDir::new().unwrap().keep();
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mem_path = project.join("memory.db");
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, "").unwrap();
    write_project_server_config(&project, "https://team.invalid:7777", "team/proj");

    assert!(
        !state_dir_under(&home).exists(),
        "precondition: state dir must not exist before the read"
    );

    let out = spelunk_bin_in(&home)
        .current_dir(&project)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["list", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !state_dir_under(&home).exists(),
        "`memory list`'s relay poll (items 42-47) must never auto-start the local \
         server — it may only poll one that is already running: {}",
        state_dir_under(&home).display()
    );
}
