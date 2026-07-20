pub mod backend;
pub mod db;
pub mod entity_id;
pub mod git_notes;
pub mod memory;
pub mod note_record;
pub mod remote;

// Storage sub-modules: each holds impl blocks for Database or standalone types.
mod chunks;
mod conventions;
mod files;
mod graph;
mod search;
mod specs;
mod sql;
mod stats;

pub use backend::{LocalMemoryBackend, MemoryBackend, NoteInput};
pub use conventions::{ConventionRow, RawChunkRow, has_doc_prefix};
pub use db::Database;
pub use entity_id::{entity_id, note_entity_id};
pub use files::FileRecord;
pub use git_notes::{
    AppendOutcome, GitNotesBackend, LOCK_WAIT_BUDGET, LockAttempt, NotesLock, NotesMergeOutcome,
    PublishOutcome, RewriteRefStatus, SkipReason, append_state_update, append_to_git_notes,
    ensure_notes_rewrite_ref, lock_notes, merge_tracking_notes, publish_notes,
};
pub use graph::GraphEdge;
pub use memory::{DedupeSummary, MemoryEdge, MemoryStore, SyncRow};
pub use note_record::{NoteRecord, now_millis, now_secs};
pub use remote::{
    BatchItemResult, BatchPushItem, BatchPushResult, CloudSyncClient, RemoteEntry,
    RemoteMemoryBackend, resolve_cloud_project_uuid,
};
pub use specs::{SpecRecord, StaleSpec};
pub use stats::{
    DriftCandidate, EmbedTokenStats, IndexStats, LanguageStat, StalenessReport, record_usage_at,
};

use anyhow::Result;
use std::path::Path;

/// Escape a user-supplied string for use in a SQLite LIKE pattern.
///
/// SQLite's LIKE operator treats `%`, `_`, and the chosen escape character as
/// special. If the caller appends or prepends wildcards around an
/// otherwise-literal value (e.g. `'%' || ?1` for suffix matching), any `%` or
/// `_` that appears inside the user's string would be misinterpreted as
/// additional wildcards, causing over-matching.
///
/// This function escapes `\`, `%`, and `_` with a backslash so that
/// `LIKE … ESCAPE '\'` treats them as literal characters.
///
/// # Example
/// ```ignore
/// let pat = format!("%{}", escape_like(user_path));
/// stmt.query(rusqlite::params![pat])?;
/// // SQL: WHERE path LIKE ?1 ESCAPE '\'
/// ```
pub(super) fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Open the appropriate memory backend.
///
/// Selection rule (ADR-004 — one canonical store per project; the
/// resolved sync mode replaces the implicit "is `server_url` set" branch):
/// 1. `backend_override = Some("git-notes")` → `GitNotesBackend`.
/// 2. [`SyncMode::CloudFirst`](crate::config::SyncMode::CloudFirst) **and** an
///    explicit `server_url` → `RemoteMemoryBackend`. `cloud_first` is the
///    server-authoritative tier: reads/writes go straight to the cloud, and an
///    unreachable server surfaces as an error (never a silent local read).
/// 3. Otherwise → local SQLite `memory.db` at `mem_path`. This covers
///    [`SyncMode::Offline`] (provable no-cloud, even when `server_url` is set;
///    the `SPELUNK_NO_SERVER=1` kill-switch resolves here) and the default
///    [`SyncMode::LocalFirst`], where reads and writes stay local and the cloud
///    replica is converged explicitly by `spelunk sync`.
///
/// This function keys on the resolved mode plus `cfg.server_url`. An
/// auto-discovered loopback server is inference-only and routes through
/// `cfg.inference_url` instead (see `Tier::effective_config`), so it never
/// diverts memory CRUD away from the project's local `memory.db`.
pub async fn open_memory_backend(
    cfg: &crate::config::Config,
    mem_path: &Path,
    backend_override: Option<&str>,
) -> Result<Box<dyn MemoryBackend + Send>> {
    use crate::config::SyncMode;

    if backend_override == Some("git-notes") {
        return Ok(Box::new(GitNotesBackend::new()));
    }
    // Only `cloud_first` (server-authoritative) routes memory CRUD straight to
    // the cloud; `offline` and `local_first` resolve to the local store.
    let route_remote = cfg.resolve_mode() == SyncMode::CloudFirst;
    if let Some(url) = cfg.server_url.as_ref().filter(|_| route_remote) {
        // The cloud-routing path attaches
        // `Authorization: Bearer {server_key}` to every memory request
        // (`RemoteMemoryBackend::authed`) and to the slug→UUID `GET
        // /v1/projects` lookup. A non-loopback plaintext `http://` `server_url`
        // would send that bearer in the clear. Reject it here — the single
        // production choke point that dominates both exposures — before any
        // request is built, mirroring `ServerInferenceClient::from_config`.
        // Loopback `http://` and any `https://` still pass; there is no opt-out
        // (Johan, 2026-07-02). A library returns the error rather than
        // `process::exit`; the CLI surfaces it and exits non-zero.
        crate::config::validate_transport_url(url).map_err(anyhow::Error::msg)?;
        return open_remote_memory_backend(cfg, mem_path, url).await;
    }
    Ok(Box::new(LocalMemoryBackend::new(MemoryStore::open(
        mem_path,
    )?)))
}

/// Build the cloud-routing memory backend (slug→UUID resolution + REST client)
/// for an already **transport-validated** `url`, using the host's default
/// secret store to resolve the bearer (see [`open_remote_memory_backend_with_store`]
/// for the store-injectable seam tests use).
///
/// Split out of [`open_memory_backend`] as the test seam for the cloud-routing
/// branch. Production reaches it only after `open_memory_backend` has enforced
/// [`crate::config::validate_transport_url`], so a non-loopback plaintext
/// `http://` url is rejected before any bearer is sent.
async fn open_remote_memory_backend(
    cfg: &crate::config::Config,
    mem_path: &Path,
    url: &str,
) -> Result<Box<dyn MemoryBackend + Send>> {
    // Bearer resolved per-origin (ADR-071 D2): `url` may be a self-hosted team
    // server (`cloud_first` mode routes any configured `server_url`, not only
    // the cloud one), so `cfg.server_key` (cloud-kind only) is not the right
    // credential here: a cloud login must never leak to a self-hosted server.
    let bearer = cfg.bearer_for(url)?;
    open_remote_memory_backend_with_bearer(cfg, mem_path, url, bearer).await
}

/// Same as [`open_remote_memory_backend`] but with an injected
/// [`SecretStore`](crate::config::secret_store::SecretStore), so tests can
/// drive the cloud-routing seam without touching the real default secret
/// store. Integration tests that must drive resolution against a
/// plaintext-http mock addressed as a non-loopback host (wiremock via
/// `0.0.0.0`) call this directly, the same reason
/// [`remote::resolve_cloud_project_uuid`]'s `_inner` half exists, to bypass a
/// guard the production entry point enforces.
#[cfg(test)]
async fn open_remote_memory_backend_with_store(
    cfg: &crate::config::Config,
    mem_path: &Path,
    url: &str,
    store: &dyn crate::config::secret_store::SecretStore,
) -> Result<Box<dyn MemoryBackend + Send>> {
    let bearer = cfg.bearer_for_with_store(url, store)?;
    open_remote_memory_backend_with_bearer(cfg, mem_path, url, bearer).await
}

async fn open_remote_memory_backend_with_bearer(
    cfg: &crate::config::Config,
    mem_path: &Path,
    url: &str,
    bearer: Option<String>,
) -> Result<Box<dyn MemoryBackend + Send>> {
    let project_id = cfg.project_id.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "server_url is set ({url}) but project_id is missing.\n\
             Set `project_id` in your spelunk config (e.g. ~/.config/spelunk/config.toml \
             or .spelunk/config.toml), or set the SPELUNK_PROJECT_ID environment variable, \
             so memory operations can be keyed to a project on the server."
        )
    })?;
    // ADR-005: cloud-api routes are scoped by `/v1/projects/{uuid}` (the
    // `{project_id}` path param is a `Path<Uuid>`), so a human slug must be
    // resolved to its UUID before it is used as the backend's project key.
    //
    // - D5: a `project_id` that is already a UUID is used directly (no lookup).
    // - D6: an unset/loopback `server_url` is the OSS spelunk-server path,
    //   which accepts arbitrary slugs as the project key — no resolution.
    //   (This branch is only reached when `server_url` is set, but we still
    //   guard on loopback so a loopback team server keeps using the slug.)
    let project_id =
        if crate::config::looks_like_uuid(&project_id) || crate::config::is_loopback_url(url) {
            project_id
        } else {
            // The cache file lives next to `memory.db`/`config.toml`, i.e. the
            // project's `.spelunk/` directory. `mem_path` is `<.spelunk>/memory.db`.
            let spelunk_dir = mem_path.parent().unwrap_or(mem_path);
            let uuid = remote::resolve_cloud_project_uuid(
                &project_id,
                url,
                bearer.as_deref(),
                cfg.server_ca.as_deref().map(std::path::Path::new),
                spelunk_dir,
            )
            .await?;
            uuid.to_string()
        };
    let client = crate::config::apply_server_ca(
        reqwest::Client::builder(),
        cfg.server_ca.as_deref().map(std::path::Path::new),
    )?
    .timeout(std::time::Duration::from_secs(30))
    .build()?;
    Ok(Box::new(RemoteMemoryBackend {
        client,
        base_url: url.to_string(),
        project_id,
        api_key: bearer,
    }))
}

// ── Tests for escape_like ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::escape_like;

    // Bug #406 — unit tests for the LIKE-metacharacter escape helper.

    #[test]
    fn percent_is_escaped() {
        assert_eq!(escape_like("foo%bar"), "foo\\%bar");
    }

    #[test]
    fn underscore_is_escaped() {
        assert_eq!(escape_like("foo_bar"), "foo\\_bar");
    }

    #[test]
    fn backslash_is_escaped_first() {
        // The backslash escape character itself must be doubled.
        assert_eq!(escape_like("foo\\bar"), "foo\\\\bar");
    }

    #[test]
    fn plain_path_is_unchanged() {
        assert_eq!(escape_like("normal/path/file.rs"), "normal/path/file.rs");
    }

    #[test]
    fn all_three_metacharacters_combined() {
        // "a%b_c\d" → "a\%b\_c\\d"
        assert_eq!(escape_like("a%b_c\\d"), "a\\%b\\_c\\\\d");
    }

    #[test]
    fn empty_string_stays_empty() {
        assert_eq!(escape_like(""), "");
    }
}

// ── Backend selection honours the resolved sync mode ──────────────────────────

#[cfg(test)]
mod backend_selection_tests {
    use super::open_memory_backend;
    use crate::config::{Config, SyncMode};
    use std::sync::OnceLock;

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

    fn clear_env() {
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn offline_mode_routes_local_even_with_server_url() {
        clear_env();
        register_sqlite_vec();
        let cfg = Config {
            server_url: Some("http://team.example.com:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: Some(SyncMode::Offline),
            ..Default::default()
        };
        let be = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None)
            .await
            .unwrap();
        assert_eq!(
            be.backend_kind(),
            "sqlite",
            "offline must keep memory local even when server_url is set"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn local_first_mode_routes_local() {
        clear_env();
        register_sqlite_vec();
        let cfg = Config {
            server_url: Some("http://team.example.com:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: Some(SyncMode::LocalFirst),
            ..Default::default()
        };
        let be = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None)
            .await
            .unwrap();
        assert_eq!(be.backend_kind(), "sqlite");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn cloud_first_mode_routes_remote() {
        clear_env();
        // This goes through the PUBLIC `open_memory_backend`, which resolves
        // the bearer via `Config::bearer_for`: the host's *default* secret
        // store. Isolate `HOME` + force the file backend so this never reads
        // or writes the developer's real `~/.config/spelunk`.
        let home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::set_var("SPELUNK_SECRET_STORE", "file");
        }

        // ADR-005 D5: a `project_id` that is already a UUID is used directly,
        // so the remote backend is constructed without any slug→UUID lookup
        // (no network call against the unreachable team.example.com host).
        // A non-loopback host must be `https://` to clear the transport guard;
        // the scheme is irrelevant to the raw-UUID path otherwise, since
        // nothing is sent here.
        let cfg = Config {
            server_url: Some("https://team.example.com:7777".to_string()),
            project_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
            mode: Some(SyncMode::CloudFirst),
            ..Default::default()
        };
        let be = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None)
            .await
            .unwrap();

        unsafe {
            match original_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            std::env::remove_var("SPELUNK_SECRET_STORE");
        }

        assert_eq!(
            be.backend_kind(),
            "remote",
            "cloud_first (server-authoritative) routes memory CRUD to the cloud"
        );
    }

    /// ADR-005 wiring (D6 happy path): when `project_id` is a *slug* and the
    /// resolved mode is CloudFirst against a non-loopback `server_url`,
    /// `open_memory_backend` must resolve the slug→UUID via `GET /v1/projects`
    /// and construct a `RemoteMemoryBackend` keyed by the **resolved UUID**, not
    /// the slug. cloud-api routes are `Path<Uuid>`, so a slug-keyed backend would
    /// 422 on every call.
    ///
    /// The boxed `MemoryBackend` exposes no downcast seam, so we prove the
    /// resolved key indirectly: drive a backend call (`count()` → `GET
    /// .../stats`) through a mock server and assert it arrived at the
    /// resolved-UUID path. The slug never appears on the wire.
    ///
    /// Non-loopback seam: wiremock binds to `127.0.0.1`, which
    /// `is_loopback_url` would (correctly) treat as loopback and short-circuit
    /// resolution (D6). We address the same listener via `0.0.0.0:<port>`, which
    /// the OS routes to the loopback listener but `is_loopback_url` classifies as
    /// non-loopback — so the cloud-routing branch is exercised without a live
    /// network or DNS.
    ///
    /// That same non-loopback plaintext `http://` url is now rejected by
    /// `open_memory_backend`'s transport guard, so this
    /// test enters at the `open_remote_memory_backend` seam directly.
    /// Production reaches that seam only *after* the guard has passed; the guard
    /// itself is covered by `cloud_first_rejects_non_loopback_http` below.
    ///
    /// Ignored on Windows: connecting to `0.0.0.0` raises `WSAEADDRNOTAVAIL`
    /// (os error 10049). Slug-resolution unit coverage lives in
    /// `storage::remote::tests`; the integration seam here is Linux/macOS-only.
    #[tokio::test]
    #[serial_test::serial]
    #[cfg_attr(windows, ignore)]
    async fn cloud_first_slug_resolves_to_uuid_in_backend() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        clear_env();
        unsafe { std::env::remove_var("SPELUNK_NO_SLUG_CACHE") };
        register_sqlite_vec();

        const RESOLVED_UUID: &str = "018f4e2a-1234-7abc-8def-0000000000aa";

        let server = MockServer::start().await;
        // Slug → UUID resolution.
        Mock::given(method("GET"))
            .and(path("/v1/projects"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "projects": [ { "id": RESOLVED_UUID, "slug": "spelunk" } ]
            })))
            .mount(&server)
            .await;
        // The backend's `count()` must hit the *UUID*-scoped stats path. If the
        // backend were (wrongly) keyed by the slug, this mock would never match
        // and `count()` would 404 → error.
        Mock::given(method("GET"))
            .and(path(format!("/v1/projects/{RESOLVED_UUID}/stats")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "count": 7 })),
            )
            .mount(&server)
            .await;

        // Rewrite the loopback `127.0.0.1` host to the non-loopback `0.0.0.0`
        // alias so the cloud-routing (non-loopback) branch is taken.
        let url = server.uri().replace("127.0.0.1", "0.0.0.0");
        assert!(
            !crate::config::is_loopback_url(&url),
            "test seam precondition: {url} must be classified non-loopback"
        );

        // Cache lives in mem_path.parent(); use a temp dir as the .spelunk dir.
        let tmp = tempfile::TempDir::new().unwrap();
        let mem_path = tmp.path().join("memory.db");

        let cfg = Config {
            server_url: Some(url),
            project_id: Some("spelunk".to_string()),
            mode: Some(SyncMode::CloudFirst),
            ..Default::default()
        };

        // Enter at the seam: production reaches `open_remote_memory_backend`
        // only after `open_memory_backend`'s transport guard passes, which this
        // non-loopback `http://` url would not (that guard is covered
        // separately). The seam exercises the same resolution + keying logic.
        // The store-injecting variant keeps this hermetic (no real secret
        // store touch); the mocks below don't assert on the bearer.
        let url = cfg.server_url.clone().unwrap();
        let store = crate::config::secret_store::MemoryStore::default();
        let be = super::open_remote_memory_backend_with_store(&cfg, &mem_path, &url, &store)
            .await
            .unwrap();
        assert_eq!(be.backend_kind(), "remote");

        // Drive a call: it succeeds only if the backend is keyed by the resolved
        // UUID (the only stats path the mock serves).
        let count = be.count().await.expect(
            "count() must reach the UUID-scoped stats endpoint; a slug-keyed \
             backend would miss the mock and fail",
        );
        assert_eq!(count, 7);

        // The resolution result must also have been cached next to memory.db
        // (the .spelunk dir is mem_path.parent()). Read the lock file directly
        // rather than reaching into private resolver internals.
        let lock = std::fs::read_to_string(tmp.path().join("cloud-project-id.lock"))
            .expect("cloud-project-id.lock should have been written next to memory.db");
        assert!(
            lock.contains("spelunk") && lock.contains(RESOLVED_UUID),
            "lock file must record slug→UUID; got: {lock}"
        );
    }

    /// A `cloud_first` config with a non-loopback
    /// plaintext `http://` `server_url` must be rejected by `open_memory_backend`
    /// before any bearer token is sent — over the memory REST calls
    /// (`RemoteMemoryBackend::authed`) or the slug→UUID `GET /v1/projects`
    /// lookup. Mirrors `server_client::transport_validator_rejects_non_loopback_http`.
    #[tokio::test]
    #[serial_test::serial]
    async fn cloud_first_rejects_non_loopback_http() {
        clear_env();
        // A raw-UUID project_id proves the rejection is the transport check, not
        // a failed lookup: D5 short-circuits resolution, so absent the guard
        // nothing would reach the network at all — yet the bearer would still be
        // attached to every subsequent memory call over plaintext.
        let cfg = Config {
            server_url: Some("http://team-server:7777".to_string()),
            project_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
            mode: Some(SyncMode::CloudFirst),
            server_key: Some("secret".to_string()),
            ..Default::default()
        };
        // `map(|_| ())` discards the non-`Debug` `Box<dyn MemoryBackend>` so
        // `expect_err` can format the (never-taken) Ok arm.
        let err = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None)
            .await
            .map(|_| ())
            .expect_err(
                "non-loopback http:// server_url must be rejected before any bearer is sent",
            );
        let msg = err.to_string();
        assert!(
            msg.contains("loopback"),
            "error must name the fix; got: {msg}"
        );
        assert!(msg.contains("https"), "error must name the fix; got: {msg}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn no_server_kill_switch_forces_local() {
        register_sqlite_vec();
        let cfg = Config {
            server_url: Some("http://team.example.com:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: Some(SyncMode::CloudFirst),
            ..Default::default()
        };
        unsafe { std::env::set_var("SPELUNK_NO_SERVER", "1") };
        let be = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None)
            .await
            .unwrap();
        assert_eq!(
            be.backend_kind(),
            "sqlite",
            "SPELUNK_NO_SERVER=1 forces offline → local backend"
        );
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };
    }
}
