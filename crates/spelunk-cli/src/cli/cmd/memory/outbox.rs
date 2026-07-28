//! ADR-037 P2: post-write nudge + read/status poll for the local relay
//! (`crate::cli::cmd::server::probe_local_relay_port`, spelunk-server's
//! `relay` module).
//!
//! Two entry points:
//! - [`nudge_after_write`] — called after a `local_first` `memory
//!   add`/`archive`/`supersede` commits, to auto-start (interactive only, D6)
//!   and hand the local server's relay any newly-unpushed rows. Best-effort
//!   only: any failure here must never surface as a write error, a
//!   meaningfully added write latency, or a non-zero exit (items 7/9/10/11).
//! - [`poll_and_apply`] — called from `spelunk status` and from `memory
//!   list`/`search`/`show`/`timeline`/`spelunk context` (items 42-47) to
//!   apply whatever the relay has buffered (push acks, pulled rows) locally,
//!   so both status and reads stay converged without ever printing a
//!   manual-sync call to action. Never triggers `ensure_server_running`
//!   (item 43): only polls an already-running relay.
//!
//! The relay's `GET /local/relay/poll` is a **peek, not a drain**: entries
//! stay buffered until [`poll_and_apply`] explicitly confirms which ones it
//! applied via `POST /local/relay/ack`. This is what makes a failed local
//! apply (a write error, a killed process mid-loop) recoverable on the very
//! next poll instead of silently losing the row (pull side) or stranding it
//! pending forever (push side) — see `relay.rs`'s module docs for the fuller
//! account of the bug this closes.
//!
//! `memory.db` is opened and written **only** by this CLI-side code — never
//! by the server (D5); the relay's own local HTTP surface only ever carries
//! row data and identifiers, never a filesystem path.

use std::io::IsTerminal;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::storage::MemoryStore;

/// Bound on both the nudge and poll HTTP calls to the local relay: high
/// enough for a loopback round trip, low enough that an absent/wedged local
/// server can never make a write feel slow (item 9).
const LOCAL_RELAY_TIMEOUT: Duration = Duration::from_millis(800);

/// Cap on entries offered in a single nudge, mirroring `push_local`'s own
/// batch chunking (`sync.rs`) — the relay forwards them to the team server in
/// its own batches regardless, so this only bounds one loopback request body.
const MAX_NUDGE_ENTRIES: usize = 200;

#[derive(Debug, Serialize)]
struct RelayPushEntryWire {
    kind: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    external_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_commit: Option<String>,
}

#[derive(Debug, Serialize)]
struct RelayPushRequestWire {
    server_url: String,
    project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    bearer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    since_cursor: Option<String>,
    entries: Vec<RelayPushEntryWire>,
}

#[derive(Debug, Default, Deserialize)]
struct RelayPushResultWire {
    external_id: String,
    remote_id: Option<String>,
    status: String,
}

#[derive(Debug, Default, Deserialize)]
struct RelayPulledEntryWire {
    remote_id: String,
    kind: String,
    title: String,
    body: Option<String>,
    source_commit: Option<String>,
    created_at: String,
    archived: bool,
}

#[derive(Debug, Default, Deserialize)]
struct RelayPollResponseWire {
    #[serde(default)]
    push_results: Vec<RelayPushResultWire>,
    #[serde(default)]
    pulled: Vec<RelayPulledEntryWire>,
    #[serde(default)]
    last_synced_at: Option<i64>,
    #[serde(default)]
    last_error: Option<String>,
}

/// Body of `POST /local/relay/ack`: names exactly the entries this call
/// confirmed applying to `memory.db`, so a failed/skipped one (still
/// unstamped or unapplied) stays buffered on the relay and is offered again
/// on the next poll, rather than being lost — see `relay.rs`'s module docs
/// for the destructive-drain bug this closes.
#[derive(Debug, Default, Serialize)]
struct RelayAckRequestWire {
    server_url: String,
    project_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    applied_push_external_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    applied_pull_remote_ids: Vec<String>,
}

/// Resolve `(server_url, project_id)` for the relay, or `None` when this
/// project has nothing to relay: not `local_first`, no `server_url`, or no
/// `project_id` (mirrors `spelunk sync`'s own requirement — there is no
/// `--project` override on a write command to fall back to, so a missing
/// `project_id` here just means the background nudge quietly does nothing,
/// same as it always has).
fn relay_target(cfg: &Config) -> Option<(String, String)> {
    if cfg.resolve_mode() != spelunk_core::config::SyncMode::LocalFirst {
        return None;
    }
    let server_url = cfg.server_url.clone()?;
    let project_id = cfg.project_id.clone()?;
    Some((server_url, project_id))
}

/// Auto-start (interactive only, D6) and nudge the local relay after a
/// `local_first` write. See module docs for the non-blocking contract.
pub(super) async fn nudge_after_write(cfg: &Config, mem_path: &std::path::Path) {
    let Some((server_url, project_id)) = relay_target(cfg) else {
        return;
    };

    if std::io::stdin().is_terminal() {
        let _ = super::super::server::ensure_server_running(7777).await;
    }

    let Some(port) = super::super::server::probe_local_relay_port().await else {
        return;
    };
    register_and_push(cfg, mem_path, &server_url, &project_id, port).await;
}

/// Register this project's relay session (creating it and starting its pull
/// loop on first sight — item 12/18/20) and hand over any currently-pending
/// outbox rows. An empty outbox still registers: the server-side push
/// handler starts the session's pull task regardless of whether `entries` is
/// empty, which is what lets a purely-read instance (never writing locally)
/// still receive live pulls (item 20's two-instance scenario needs exactly
/// this — instance B may never call `memory add` at all).
async fn register_and_push(
    cfg: &Config,
    mem_path: &std::path::Path,
    server_url: &str,
    project_id: &str,
    port: u16,
) {
    let Ok(local) = MemoryStore::open(mem_path) else {
        return;
    };
    let Ok(rows) = local.rows_for_sync(false) else {
        return;
    };
    let entries: Vec<RelayPushEntryWire> = rows
        .iter()
        .filter(|r| !r.archived && r.remote_id.is_none())
        .take(MAX_NUDGE_ENTRIES)
        .map(|r| RelayPushEntryWire {
            kind: r.kind.clone(),
            title: r.title.clone(),
            body: if r.body.is_empty() {
                None
            } else {
                Some(r.body.clone())
            },
            external_id: r.uuid.clone(),
            source_commit: r.source_ref.clone(),
        })
        .collect();
    let since_cursor = local.max_remote_id().ok().flatten();
    let bearer = super::super::auth_api::ensure_fresh_server_key(cfg, server_url)
        .await
        .ok()
        .flatten();

    let Ok(client) = reqwest::Client::builder()
        .timeout(LOCAL_RELAY_TIMEOUT)
        .build()
    else {
        return;
    };
    let body = RelayPushRequestWire {
        server_url: server_url.to_string(),
        project_id: project_id.to_string(),
        bearer,
        since_cursor,
        entries,
    };
    let _ = client
        .post(format!("http://127.0.0.1:{port}/local/relay/push"))
        .json(&body)
        .send()
        .await;
}

/// What [`poll_and_apply`] applied, for `spelunk status`'s pending/last-synced
/// line.
pub(crate) struct PollOutcome {
    pub applied_pushes: usize,
    pub applied_pulls: usize,
    pub last_synced_at: Option<i64>,
    pub last_error: Option<String>,
}

/// Poll the local relay (if reachable) for a project's buffered push-acks and
/// pulled rows, apply them via the CLI-side storage layer, and return what
/// happened. Returns `None` when there is nothing to poll (not `local_first`,
/// no server configured, or no local relay reachable) — `spelunk status`
/// falls back to a purely local pending-count in that case.
///
/// Also registers the relay session (via [`register_and_push`]) before
/// polling, same as a write's nudge: a purely-read instance that never calls
/// `memory add` still needs its session registered at some point for live
/// pull to reach it at all (item 20 — this is the mechanism that lets
/// instance B in the two-instance scenario pick up instance A's write
/// without ever writing locally itself).
pub(crate) async fn poll_and_apply(
    cfg: &Config,
    mem_path: &std::path::Path,
) -> Option<PollOutcome> {
    let (server_url, project_id) = relay_target(cfg)?;
    let port = super::super::server::probe_local_relay_port().await?;
    register_and_push(cfg, mem_path, &server_url, &project_id, port).await;
    let local = MemoryStore::open(mem_path).ok()?;

    let client = reqwest::Client::builder()
        .timeout(LOCAL_RELAY_TIMEOUT)
        .build()
        .ok()?;
    let resp = client
        .get(format!("http://127.0.0.1:{port}/local/relay/poll"))
        .query(&[("server_url", &server_url), ("project_id", &project_id)])
        .send()
        .await
        .ok()?;
    let body: RelayPollResponseWire = resp.json().await.ok()?;

    // The relay's poll is a peek, not a drain (see `relay.rs`'s module docs):
    // every entry applied here is named explicitly in a follow-up `ack` call
    // below, and only those names are retired from the relay's buffer. An
    // entry this loop fails to apply (a local write error, a killed process
    // mid-loop) is simply never named, so it stays buffered and is offered
    // again on the CLI's next poll — closing the "poll succeeds, apply fails,
    // row is gone forever" gap the old destructive `drain`-on-poll had.
    let mut applied_pushes = 0usize;
    let mut acked_push_ids: Vec<String> = Vec::new();
    for r in &body.push_results {
        let durably_persisted = r.status == "created" || r.status == "skipped";
        if !durably_persisted {
            continue;
        }
        if let (Some(remote_id), Ok(Some(local_id))) =
            (&r.remote_id, local.note_id_for_uuid(&r.external_id))
            && local.set_remote_id(local_id, remote_id).is_ok()
        {
            applied_pushes += 1;
            acked_push_ids.push(r.external_id.clone());
        }
    }

    let mut applied_pulls = 0usize;
    let mut acked_pull_ids: Vec<String> = Vec::new();
    for e in &body.pulled {
        let created_secs = super::sync::parse_iso_to_secs(&e.created_at);
        if local
            .apply_remote_note(
                &e.remote_id,
                &e.kind,
                &e.title,
                e.body.as_deref().unwrap_or(""),
                e.source_commit.as_deref(),
                created_secs,
                e.archived,
            )
            .is_ok()
        {
            applied_pulls += 1;
            acked_pull_ids.push(e.remote_id.clone());
        }
    }

    if !acked_push_ids.is_empty() || !acked_pull_ids.is_empty() {
        let ack_body = RelayAckRequestWire {
            server_url: server_url.clone(),
            project_id: project_id.clone(),
            applied_push_external_ids: acked_push_ids,
            applied_pull_remote_ids: acked_pull_ids,
        };
        // Best-effort: a failed/dropped ack just means these already-applied
        // (and locally idempotent to re-apply) entries are offered again on
        // the next poll instead of being retired promptly. Never surfaced as
        // an error here.
        let _ = client
            .post(format!("http://127.0.0.1:{port}/local/relay/ack"))
            .json(&ack_body)
            .send()
            .await;
    }

    let outcome = PollOutcome {
        applied_pushes,
        applied_pulls,
        last_synced_at: body.last_synced_at,
        last_error: body.last_error,
    };
    if outcome.applied_pushes > 0 || outcome.applied_pulls > 0 {
        tracing::debug!(
            applied_pushes = outcome.applied_pushes,
            applied_pulls = outcome.applied_pulls,
            "applied relay poll results to local memory.db"
        );
    }
    Some(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::OnceLock;

    use serial_test::serial;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn register_sqlite_vec() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )));
            }
        });
    }

    fn open_store(path: &std::path::Path) -> MemoryStore {
        register_sqlite_vec();
        MemoryStore::open(path).expect("open memory.db")
    }

    /// Spin up a real `spelunk-server` axum router (the actual production
    /// router, not a hand-rolled stand-in) on an ephemeral loopback port and
    /// return its address. Serves BOTH roles the same binary can play: the
    /// team-hosting `/v1/projects/*/memory*` routes (a stand-in for a real
    /// team server) and the local-only `/local/relay/*` routes (a stand-in
    /// for a real `spelunk server start`-ed daemon) — callers pick which
    /// role they're using it for by whether they write a state-dir port file
    /// (see [`spawn_local_relay`]) or pass the address as `server_url`.
    async fn spawn_spelunk_server() -> SocketAddr {
        register_sqlite_vec();
        let db_dir = TempDir::new().unwrap();
        let db =
            spelunk_server::db::ServerDb::open(&db_dir.path().join("server.db"), 4, "test-model")
                .unwrap();
        let instance_id = db.get_or_create_instance_id().unwrap();
        let state = spelunk_server::AppState {
            db: std::sync::Arc::new(tokio::sync::Mutex::new(db)),
            auth: std::sync::Arc::new(spelunk_server::auth::ApiKeyAuth::new(None)),
            conflict_threshold: spelunk_server::default_conflict_threshold(),
            embedder: spelunk_server::EmbedderSlot::disabled(),
            embed_admission: spelunk_server::EmbedAdmission::new(
                spelunk_server::EMBED_QUEUE_CAPACITY,
                spelunk_server::EMBED_BUSY_RETRY_AFTER_SECS,
            ),
            llm: None,
            max_tokens_ceiling: 8192,
            rate_limiter: std::sync::Arc::new(spelunk_server::rate_limiter::RateLimiter::new(
                1000, 60,
            )),
            instance_id,
            started_by: None,
            relay: spelunk_server::relay::RelayRegistry::new(),
        };
        let app = spelunk_server::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        addr
    }

    /// Like [`spawn_spelunk_server`], but also writes the port into
    /// `state_dir/server.port` so `server::probe_local_relay_port`
    /// discovers it exactly the way it would discover a real
    /// `spelunk server start`-ed daemon — the *local relay* role.
    async fn spawn_local_relay(state_dir: &std::path::Path) -> SocketAddr {
        let addr = spawn_spelunk_server().await;
        std::fs::create_dir_all(state_dir).unwrap();
        std::fs::write(state_dir.join("server.port"), format!("{}\n", addr.port())).unwrap();
        addr
    }

    /// Sets `SPELUNK_STATE_DIR` to a fresh temp dir for the test's duration,
    /// restoring the previous value on drop. Mirrors the guard in
    /// `server.rs`'s own tests.
    struct StateDirGuard(Option<std::ffi::OsString>, TempDir);
    impl StateDirGuard {
        fn new() -> Self {
            let prev = std::env::var_os("SPELUNK_STATE_DIR");
            let tmp = TempDir::new().unwrap();
            unsafe { std::env::set_var("SPELUNK_STATE_DIR", tmp.path()) };
            Self(prev, tmp)
        }
        fn path(&self) -> &std::path::Path {
            self.1.path()
        }
    }
    impl Drop for StateDirGuard {
        fn drop(&mut self) {
            // SAFETY: `#[serial(server_state_dir_env)]` on every test using
            // this guard serialises against every other test touching the var
            // (this crate's `server.rs` tests use the same group name).
            unsafe {
                match &self.0 {
                    Some(v) => std::env::set_var("SPELUNK_STATE_DIR", v),
                    None => std::env::remove_var("SPELUNK_STATE_DIR"),
                }
            }
        }
    }

    fn local_first_cfg(team_server_uri: &str) -> Config {
        Config {
            server_url: Some(team_server_uri.to_string()),
            project_id: Some("proj".to_string()),
            ..Default::default()
        }
    }

    // ── items 7/8/11/12/14: nudge -> relay push -> poll stamps remote_id ────

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn nudge_after_write_relays_pending_rows_and_a_later_poll_stamps_remote_id() {
        let state_guard = StateDirGuard::new();
        spawn_local_relay(state_guard.path()).await;

        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let store = open_store(&mem_path);
        store
            .add_note("decision", "T", "body", &[], &[], None, None)
            .unwrap();
        let uuid = store.rows_for_sync(false).unwrap()[0].uuid.clone();
        drop(store);

        let team_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": uuid, "id": "cloud-1"}]
            })))
            .mount(&team_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"entries": [], "count": 0})),
            )
            .mount(&team_server)
            .await;

        let cfg = local_first_cfg(&team_server.uri());
        nudge_after_write(&cfg, &mem_path).await;

        // The remote push happens in the local relay's own detached task;
        // poll until it lands rather than assuming a fixed sleep.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut applied = 0usize;
        while std::time::Instant::now() < deadline {
            if let Some(outcome) = poll_and_apply(&cfg, &mem_path).await {
                applied = outcome.applied_pushes;
                if applied > 0 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert!(
            applied >= 1,
            "the push-ack must be applied via CLI-side storage (poll_and_apply also \
             re-registers on every call, per item 20, so a still-unstamped row can \
             legitimately be offered more than once before it lands — the row-level \
             assertions below are the authoritative check): got {applied}"
        );

        let store = open_store(&mem_path);
        assert_eq!(
            store.note_id_for_remote_id("cloud-1").unwrap(),
            store.note_id_for_uuid(&uuid).unwrap(),
            "the row must carry the cloud-assigned remote_id after the poll applies it"
        );
        assert_eq!(
            store.pending_sync_count().unwrap(),
            0,
            "a stamped row must no longer count as pending"
        );
    }

    // ── item 26/29: gated on local_first (offline/cloud_first are no-ops) ──

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn nudge_after_write_is_a_noop_when_mode_is_not_local_first() {
        let state_guard = StateDirGuard::new();
        let addr = spawn_local_relay(state_guard.path()).await;

        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let store = open_store(&mem_path);
        store
            .add_note("decision", "T", "body", &[], &[], None, None)
            .unwrap();
        drop(store);

        for mode in [
            spelunk_core::config::SyncMode::Offline,
            spelunk_core::config::SyncMode::CloudFirst,
        ] {
            let cfg = Config {
                server_url: Some(format!("http://127.0.0.1:{}", addr.port())),
                project_id: Some("proj".to_string()),
                mode: Some(mode),
                ..Default::default()
            };
            nudge_after_write(&cfg, &mem_path).await;
        }

        // Direct, race-free check: neither nudge touched the row at all — it
        // is exactly as it was before either call, still pending. (A
        // subsequent `local_first` `poll_and_apply` would itself register
        // and push, per item 20, so it is not used here to avoid conflating
        // "the gated nudges did nothing" with "a later, ungated poll did
        // something".)
        let store = open_store(&mem_path);
        assert_eq!(
            store.pending_sync_count().unwrap(),
            1,
            "offline/cloud_first nudges must never touch the outbox row"
        );
    }

    #[tokio::test]
    async fn nudge_after_write_is_a_noop_without_project_id() {
        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let store = open_store(&mem_path);
        store
            .add_note("decision", "T", "body", &[], &[], None, None)
            .unwrap();
        drop(store);

        let cfg = Config {
            server_url: Some("https://team.example".to_string()),
            project_id: None,
            ..Default::default()
        };
        // No SPELUNK_STATE_DIR override, no local relay reachable either way:
        // this must return promptly without an unbounded wait.
        let start = std::time::Instant::now();
        nudge_after_write(&cfg, &mem_path).await;
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "must not hang absent a project_id"
        );

        let store = open_store(&mem_path);
        assert_eq!(
            store.pending_sync_count().unwrap(),
            1,
            "the row stays queued; nothing to relay to without a project_id"
        );
    }

    // ── item 9/10: absent local relay must not add meaningful latency ──────

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn nudge_after_write_returns_quickly_when_no_local_relay_is_running() {
        let _state_guard = StateDirGuard::new();
        // No `spawn_local_relay` call: the state dir has no port file, so
        // `probe_local_relay_port` must return `None` without any network
        // call (item 10: outbox visibility never depends on a live
        // reconciler; item 9: latency stays offline-shaped).
        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let store = open_store(&mem_path);
        store
            .add_note("decision", "T", "body", &[], &[], None, None)
            .unwrap();
        drop(store);

        let cfg = local_first_cfg("https://team.example");
        let start = std::time::Instant::now();
        nudge_after_write(&cfg, &mem_path).await;
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "no local relay running must be a fast no-op, not a bounded-timeout wait: {:?}",
            start.elapsed()
        );

        let store = open_store(&mem_path);
        assert_eq!(
            store.pending_sync_count().unwrap(),
            1,
            "the write itself is unaffected: the row stays durably queued"
        );
    }

    // ── poll_and_apply gating mirrors nudge_after_write's ───────────────────

    #[tokio::test]
    async fn poll_and_apply_returns_none_when_not_local_first() {
        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let _store = open_store(&mem_path);

        let cfg = Config {
            mode: Some(spelunk_core::config::SyncMode::Offline),
            ..Default::default()
        };
        assert!(poll_and_apply(&cfg, &mem_path).await.is_none());
    }

    // ── item 30 guard: repeated nudges never disturb the running relay ─────
    // No idle-reap logic exists anywhere in this task's scope; this pins that
    // a later change cannot silently smuggle one in via this call path.

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn repeated_nudges_never_stop_the_local_relay() {
        let state_guard = StateDirGuard::new();
        spawn_local_relay(state_guard.path()).await;

        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let _store = open_store(&mem_path);

        let team_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"entries": [], "count": 0})),
            )
            .mount(&team_server)
            .await;
        let cfg = local_first_cfg(&team_server.uri());

        for _ in 0..3 {
            nudge_after_write(&cfg, &mem_path).await;
            assert!(
                crate::cli::cmd::server::probe_local_relay_port()
                    .await
                    .is_some(),
                "the local relay must still be reachable after each nudge"
            );
        }
    }

    /// Point `SPELUNK_STATE_DIR` at `dir` for the remainder of the current
    /// scope. Caller must hold `#[serial(server_state_dir_env)]` AND keep a
    /// [`RestoreStateDirOnDrop`] alive for the test's duration, or the
    /// mutated value leaks into whichever test in the same serial group runs
    /// next.
    fn point_state_dir_at(dir: &std::path::Path) {
        unsafe { std::env::set_var("SPELUNK_STATE_DIR", dir) };
    }

    /// Captures the current `SPELUNK_STATE_DIR` on construction and restores
    /// it on drop. Tests that call [`point_state_dir_at`] more than once (so
    /// [`StateDirGuard`] alone won't do, since it only knows the value it
    /// itself set) must hold one of these for the whole test body.
    struct RestoreStateDirOnDrop(Option<std::ffi::OsString>);
    impl RestoreStateDirOnDrop {
        fn capture() -> Self {
            Self(std::env::var_os("SPELUNK_STATE_DIR"))
        }
    }
    impl Drop for RestoreStateDirOnDrop {
        fn drop(&mut self) {
            // SAFETY: see `StateDirGuard::drop` above; same serial group.
            unsafe {
                match &self.0 {
                    Some(v) => std::env::set_var("SPELUNK_STATE_DIR", v),
                    None => std::env::remove_var("SPELUNK_STATE_DIR"),
                }
            }
        }
    }

    // ── item 20: SSE-driven live pull, two local instances, one team server ─
    //
    // Uses a REAL `spelunk-server` router as the team server (not a wiremock
    // stub): its `/v1/projects/*/memory*` team-hosting routes are the same
    // production handlers a real cloud-api-or-OSS team server would run, so
    // pushing through instance A's relay and having instance B's relay pick
    // it up exercises the actual SSE `/memory/stream` code path this
    // module's pull loop consumes, not just `/memory/since` polling.

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn entry_added_on_instance_a_becomes_visible_on_instance_b_via_live_pull() {
        let _restore_state_dir = RestoreStateDirOnDrop::capture();
        let team_addr = spawn_spelunk_server().await;
        let team_uri = format!("http://{}", team_addr);

        let state_a = TempDir::new().unwrap();
        let state_b = TempDir::new().unwrap();
        spawn_local_relay(state_a.path()).await;
        spawn_local_relay(state_b.path()).await;

        let mem_dir_a = TempDir::new().unwrap();
        let mem_a = mem_dir_a.path().join("memory.db");
        let mem_dir_b = TempDir::new().unwrap();
        let mem_b = mem_dir_b.path().join("memory.db");
        let _store_a = open_store(&mem_a);
        let _store_b = open_store(&mem_b);

        let cfg = local_first_cfg(&team_uri);

        // Register B FIRST, before A ever writes, so B's pull loop is live
        // (holding its SSE connection) with nothing yet to catch up on — the
        // entry it eventually sees can only have arrived via the live SSE
        // wake-up + re-catch-up path, not B's own initial registration
        // catch-up.
        point_state_dir_at(state_b.path());
        assert!(
            poll_and_apply(&cfg, &mem_b).await.is_some(),
            "instance B's relay must be reachable"
        );

        // Now instance A writes and relays it to the team server.
        point_state_dir_at(state_a.path());
        let store_a = open_store(&mem_a);
        store_a
            .add_note(
                "decision",
                "Cross-instance entry",
                "body",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        drop(store_a);
        nudge_after_write(&cfg, &mem_a).await;

        // Instance B: drive a REAL `spelunk memory list` invocation (item 45)
        // until the entry arrives via live pull — not `poll_and_apply`
        // directly. This is the actual gap the founder review found: the
        // item-20 e2e test previously passed by calling `poll_and_apply`
        // itself, so it never exercised the read path item 20's own text
        // names ("visible via `spelunk memory list`/`search`"). Calling the
        // production `memory_list` function here means this test can no
        // longer pass without `memory list` itself draining the buffer.
        point_state_dir_at(state_b.path());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut seen = false;
        while std::time::Instant::now() < deadline {
            crate::cli::cmd::memory::list::memory_list(
                crate::cli::cmd::memory::MemoryListArgs {
                    kind: None,
                    source_ref: None,
                    limit: 20,
                    format: "json".to_string(),
                    archived: false,
                    as_of: None,
                    local_only: true,
                },
                &mem_b,
                &cfg,
                None,
                false,
            )
            .await
            .unwrap();
            let store_b = open_store(&mem_b);
            if store_b
                .rows_for_sync(false)
                .unwrap()
                .iter()
                .any(|n| n.title == "Cross-instance entry")
            {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            seen,
            "instance A's write must become visible via a real `spelunk memory list` \
             invocation on instance B, without any explicit `spelunk sync`/`memory pull`"
        );
    }

    // ── item 16: kill-and-restart mid-drain, no data loss, no duplicates ───
    //
    // Simulates a killed-and-restarted local server with a genuinely fresh
    // process: a second `spawn_local_relay` (a brand new axum router + a
    // brand new, empty `RelayRegistry`) that shares no state at all with the
    // first. Re-registering against it must re-derive the outbox/cursor from
    // `memory.db` alone and reach a correct, duplicate-free end state.

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn kill_and_restart_the_local_relay_mid_drain_loses_nothing_and_dedupes() {
        let _restore_state_dir = RestoreStateDirOnDrop::capture();
        let team_addr = spawn_spelunk_server().await;
        let team_uri = format!("http://{}", team_addr);
        let cfg = local_first_cfg(&team_uri);

        let state_1 = TempDir::new().unwrap();
        spawn_local_relay(state_1.path()).await;

        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let store = open_store(&mem_path);
        store
            .add_note("decision", "A", "body", &[], &[], None, None)
            .unwrap();
        store
            .add_note("decision", "B", "body", &[], &[], None, None)
            .unwrap();
        drop(store);

        point_state_dir_at(state_1.path());
        nudge_after_write(&cfg, &mem_path).await;

        // Wait for both A and B to land on the (real) team server before the
        // "restart".
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            poll_and_apply(&cfg, &mem_path).await;
            let store = open_store(&mem_path);
            if store.pending_sync_count().unwrap() == 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "A and B did not land on the team server before the deadline"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // "Restart": a second, wholly independent local relay process/registry.
        let state_2 = TempDir::new().unwrap();
        spawn_local_relay(state_2.path()).await;

        // A new, never-yet-pushed row, added after the "restart".
        let store = open_store(&mem_path);
        store
            .add_note("decision", "C", "body", &[], &[], None, None)
            .unwrap();
        drop(store);

        point_state_dir_at(state_2.path());
        nudge_after_write(&cfg, &mem_path).await;

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            poll_and_apply(&cfg, &mem_path).await;
            let store = open_store(&mem_path);
            if store.pending_sync_count().unwrap() == 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "C did not land on the team server before the deadline"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let store = open_store(&mem_path);
        assert_eq!(
            store.count().unwrap(),
            3,
            "no data loss and no duplicates across the simulated restart"
        );
        let rows = store.rows_for_sync(false).unwrap();
        assert!(
            rows.iter().all(|r| r.remote_id.is_some()),
            "every row must carry a remote_id after re-deriving through the new relay"
        );
    }

    // ── founder review (PR #728): apply-fails-without-restart, end to end ──
    //
    // The kill/restart test above (item 16) only covers the path where a
    // fresh relay re-derives everything from `memory.db`, which already
    // works because the outbox/cursor are durable. The actual gap the
    // founder review found is narrower and does NOT involve a restart at
    // all: the relay hands the CLI a result, the CLI's own local write to
    // `memory.db` fails (e.g. `SQLITE_BUSY`, no `busy_timeout` is
    // configured), and — before this fix — that result was already gone
    // from the relay's buffer (destructive `drain`-on-poll), so nothing
    // would ever retry it. These two tests force that exact failure with a
    // real competing SQLite writer (same technique as
    // `spelunk_core::storage::db::tests::insert_embeddings_rolls_back_on_a_real_sqlite_error_not_just_bad_dimension`)
    // holding `memory.db`'s write lock across one or more `poll_and_apply`
    // calls, through the real `nudge_after_write`/`poll_and_apply` code path
    // — not a synthetic unit test of the relay's buffer alone. Verification
    // while the lock is held goes through a bare read-only connection: even
    // `MemoryStore::open` itself always attempts a write on every call (the
    // FTS-sync migration's `INSERT OR IGNORE`, unconditional regardless of
    // content), so it is itself lock-contentious and cannot be used to probe
    // state during the locked window without tripping the very contention
    // under test.

    /// Read-only, migration-free probe: whether any local row already
    /// carries `remote_id`. Deliberately bypasses `MemoryStore::open` (see
    /// the comment above) so this itself never contends for the write lock.
    fn raw_has_remote_id(mem_path: &std::path::Path, remote_id: &str) -> bool {
        let conn = rusqlite::Connection::open(mem_path).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE remote_id = ?1",
                rusqlite::params![remote_id],
                |r| r.get(0),
            )
            .unwrap();
        n > 0
    }

    /// Peek the local relay directly (bypassing `poll_and_apply`, so this
    /// never consumes or applies anything — the poll itself is non-
    /// destructive after this fix). Used only to deterministically wait for
    /// the relay's own background catch-up/push to have buffered something,
    /// without racing a blind sleep against it.
    async fn raw_relay_peek(
        port: u16,
        server_url: &str,
        project_id: &str,
    ) -> RelayPollResponseWire {
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{port}/local/relay/poll"))
            .query(&[("server_url", server_url), ("project_id", project_id)])
            .send()
            .await
            .unwrap();
        resp.json().await.unwrap()
    }

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn a_pull_apply_failure_without_a_restart_does_not_lose_the_row() {
        let state_guard = StateDirGuard::new();
        let team_addr = spawn_spelunk_server().await;
        spawn_local_relay(state_guard.path()).await;
        let team_uri = format!("http://{}", team_addr);
        let cfg = local_first_cfg(&team_uri);

        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let _store = open_store(&mem_path); // create + migrate, nothing local yet

        // Register (unlocked) so the relay's background pull loop is alive
        // and holding its own SSE connection to the team server BEFORE the
        // row is seeded — same ordering `entry_added_on_instance_a_...`
        // above uses, so the row is picked up by the relay's own background
        // catch-up, entirely independent of any later `poll_and_apply` call.
        assert!(
            poll_and_apply(&cfg, &mem_path).await.is_some(),
            "the local relay must be reachable"
        );

        // `memory_stream`'s server-side polling loop only yields notes with
        // `created_at` strictly after the SSE connection's own second-
        // granularity start time; without this, a note created in the same
        // wall-clock second as registration can be silently invisible to the
        // stream forever (nothing else ever advances `last_seen` past it in
        // this single-write test). Cross a second boundary first so the seed
        // below is unambiguously "after".
        tokio::time::sleep(Duration::from_millis(1100)).await;

        // Seed the entry directly on the team server, as if pushed by a
        // different instance — this instance only ever sees it via pull.
        // Deliberately NOT capturing `results[0].id` from this response as
        // the row's identity: that field is the server's raw local row id,
        // a different value from the `sync_id` `/memory/since` (and thus
        // this relay's `pulled[].remote_id`) actually keys on. Instead, read
        // the real identity back off the relay's own buffered entry below.
        let http = reqwest::Client::new();
        http.post(format!("{team_uri}/v1/projects/proj/memory/batch"))
            .json(&serde_json::json!({
                "entries": [{
                    "kind": "decision", "title": "Remote", "body": "b",
                    "external_id": "seed-ext-1"
                }]
            }))
            .send()
            .await
            .unwrap();

        // Wait for the relay's own SSE-driven background catch-up to buffer
        // the row on its own — verified via a direct, non-destructive peek
        // at the relay (never through `poll_and_apply`, so this wait itself
        // never applies/consumes anything).
        let port = crate::cli::cmd::server::probe_local_relay_port()
            .await
            .expect("local relay must be reachable");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let remote_id = loop {
            let peek = raw_relay_peek(port, &team_uri, "proj").await;
            if let Some(entry) = peek.pulled.iter().find(|p| p.title == "Remote") {
                break entry.remote_id.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the relay never buffered the seeded row via its background catch-up"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        // Only now hold `memory.db`'s write lock from a second, competing
        // connection — the row is confirmed buffered relay-side; this is the
        // CLI's first attempt to retrieve+apply it.
        let locker = rusqlite::Connection::open(&mem_path).unwrap();
        locker.execute_batch("BEGIN IMMEDIATE;").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            poll_and_apply(&cfg, &mem_path).await;
            assert!(
                !raw_has_remote_id(&mem_path, &remote_id),
                "the row must not appear locally while the competing writer holds the lock"
            );
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        locker.execute_batch("COMMIT;").unwrap();

        // Now that the lock is released, the row must still be recoverable
        // — proving it was never dropped from the relay's buffer despite
        // every earlier apply attempt failing.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            poll_and_apply(&cfg, &mem_path).await;
            if raw_has_remote_id(&mem_path, &remote_id) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the pulled row must still be recoverable once the lock is released \
                 (it must never have been dropped by an earlier failed apply attempt)"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn a_push_stamp_failure_without_a_restart_does_not_strand_the_row_pending_forever() {
        let state_guard = StateDirGuard::new();
        spawn_local_relay(state_guard.path()).await;

        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let uuid = {
            let store = open_store(&mem_path);
            store
                .add_note("decision", "T", "body", &[], &[], None, None)
                .unwrap();
            store.rows_for_sync(false).unwrap()[0].uuid.clone()
        };

        let team_server = MockServer::start().await;
        // The FIRST push lands and creates the row. `register_and_push` is
        // called on every `nudge_after_write`/`poll_and_apply` and always
        // re-offers every still-unstamped row (item 8/11) — including this
        // one, while its earlier `created` ack sits unstamped due to the
        // lock below — so any SUBSEQUENT push of the same `external_id` must
        // behave like a real team server's idempotent dedupe: `skipped`,
        // with **no id** (`handlers.rs`'s pre-fix behavior; the exact trap
        // named in the founder review). If the fix relied on that later
        // push somehow re-minting a fresh id, this mock would silently mask
        // the regression — it must not: recovery has to come from the
        // relay's still-buffered original `created` result, never a re-push.
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": uuid, "id": "cloud-1"}]
            })))
            .up_to_n_times(1)
            .mount(&team_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 0, "skipped": 1, "failed": 0,
                "results": [{"status": "skipped", "external_id": uuid, "id": null}]
            })))
            .mount(&team_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"entries": [], "count": 0})),
            )
            .mount(&team_server)
            .await;

        let cfg = local_first_cfg(&team_server.uri());
        // Unlocked: registers the session and relays the push to the (mock)
        // team server in a detached background task on the relay side.
        nudge_after_write(&cfg, &mem_path).await;

        // Wait for that detached push to land and buffer a `created` ack
        // relay-side — verified via a direct, non-destructive peek at the
        // relay, entirely independent of any `poll_and_apply` call.
        let port = crate::cli::cmd::server::probe_local_relay_port()
            .await
            .expect("local relay must be reachable");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let peek = raw_relay_peek(port, &team_server.uri(), "proj").await;
            if peek.push_results.iter().any(|r| r.external_id == uuid) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the relay never buffered the push ack"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Only now hold `memory.db`'s write lock — this is the CLI's first
        // attempt to retrieve+apply (stamp) the already-buffered ack, even
        // though the push already durably landed remotely.
        let locker = rusqlite::Connection::open(&mem_path).unwrap();
        locker.execute_batch("BEGIN IMMEDIATE;").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            poll_and_apply(&cfg, &mem_path).await;
            assert!(
                !raw_has_remote_id(&mem_path, "cloud-1"),
                "the row must not get stamped while the competing writer holds the lock"
            );
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        locker.execute_batch("COMMIT;").unwrap();

        // Once the lock is released, the SAME buffered `created` ack (never
        // dropped by an earlier failed poll) must still be there to stamp —
        // this is the fix for the "re-push comes back `skipped` with no id"
        // trap: there is no re-push at all here, just a retried apply of the
        // original, still-buffered result.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            poll_and_apply(&cfg, &mem_path).await;
            if raw_has_remote_id(&mem_path, "cloud-1") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the push ack must still be recoverable once the lock is released \
                 (it must never have been dropped by an earlier failed stamp attempt)"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let reader = open_store(&mem_path);
        assert_eq!(
            reader.pending_sync_count().unwrap(),
            0,
            "the row must no longer read as pending once the stamp succeeds"
        );
    }
}
