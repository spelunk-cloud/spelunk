pub mod backend;
pub mod db;
pub mod entity_id;
pub mod git_notes;
pub mod memory;
pub mod note_kind;
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
    NotesRefs, PublishOutcome, RewriteRefStatus, SkipReason, append_state_update,
    append_to_git_notes, ensure_notes_rewrite_ref, lock_notes, merge_tracking_notes, publish_notes,
};
pub use graph::GraphEdge;
pub use memory::{
    DedupeSummary, MemoryEdge, MemoryStore, NoteId, NotesImportMarker, SyncEdge, SyncRow,
};
pub use note_kind::{NOTE_KINDS, is_valid_note_kind, parse_note_kind};
pub use note_record::{NoteRecord, now_millis, now_secs};
pub use remote::{
    BatchItemResult, BatchPushItem, BatchPushResult, CloudSyncClient, EdgePushResult, RemoteEntry,
    RemoteMemoryBackend, SyncEdgePush,
};
pub use specs::{SpecRecord, StaleSpec};
pub use stats::{
    DriftCandidate, EmbedTokenStats, IndexStats, LanguageStat, StalenessReport, record_usage_at,
};

use anyhow::Result;
use std::path::Path;

/// Cap a freshly-opened connection's page count when
/// `SPELUNK_TEST_MAX_PAGE_COUNT` is set, so the crash-safety integration
/// suite can force a deterministic `SQLITE_FULL` on the next write without a
/// size-capped filesystem or a custom VFS. `max_page_count` is a
/// per-connection setting (SQLite does not persist it to the file), so this
/// must run on every `open`, not once ever. A no-op for every real user: the
/// var is never set outside the test harness.
pub(crate) fn apply_test_page_cap(conn: &rusqlite::Connection) -> Result<()> {
    if let Ok(raw) = std::env::var("SPELUNK_TEST_MAX_PAGE_COUNT")
        && let Ok(n) = raw.parse::<i64>()
    {
        conn.execute_batch(&format!("PRAGMA max_page_count = {n};"))?;
    }
    Ok(())
}

/// Block until killed, iff `SPELUNK_TEST_CRASH_POINT` names this exact point.
/// Used by the crash-safety integration suite to land a real `SIGKILL` inside
/// a chosen write window instead of racing wall-clock timing: the child
/// prints a marker then blocks on a stdin read the harness never satisfies,
/// so the harness can block on the marker line and then kill with the
/// process provably parked at that window. A no-op for every real user.
pub(crate) fn pause_for_crash_test(point: &str) {
    let Ok(target) = std::env::var("SPELUNK_TEST_CRASH_POINT") else {
        return;
    };
    if target != point {
        return;
    }
    println!("SPELUNK_TEST_CRASH_POINT_REACHED:{point}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut buf = [0u8; 1];
    let _ = std::io::Read::read(&mut std::io::stdin(), &mut buf);
}

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
        // (`RemoteMemoryBackend::authed`). A non-loopback plaintext `http://`
        // `server_url` would send that bearer in the clear. Reject it here: the
        // single production choke point that dominates that exposure, before any
        // request is built, mirroring `ServerInferenceClient::from_config`.
        // Loopback `http://` and any `https://` still pass; there is no opt-out
        // (Johan, 2026-07-02). A library returns the error rather than
        // `process::exit`; the CLI surfaces it and exits non-zero.
        crate::config::validate_transport_url(url).map_err(anyhow::Error::msg)?;
        return open_remote_memory_backend(cfg, url).await;
    }
    Ok(Box::new(LocalMemoryBackend::new(MemoryStore::open(
        mem_path,
    )?)))
}

/// Build the cloud-routing memory backend (REST client) for an already
/// **transport-validated** `url`, using the host's default secret store to
/// resolve the bearer (see [`open_remote_memory_backend_with_store`] for the
/// store-injectable seam tests use).
///
/// Split out of [`open_memory_backend`] as the test seam for the cloud-routing
/// branch. Production reaches it only after `open_memory_backend` has enforced
/// [`crate::config::validate_transport_url`], so a non-loopback plaintext
/// `http://` url is rejected before any bearer is sent.
async fn open_remote_memory_backend(
    cfg: &crate::config::Config,
    url: &str,
) -> Result<Box<dyn MemoryBackend + Send>> {
    // Bearer resolved per-origin (ADR-071 D2): `url` may be a self-hosted team
    // server (`cloud_first` mode routes any configured `server_url`, not only
    // the cloud one), so `cfg.server_key` (cloud-kind only) is not the right
    // credential here: a cloud login must never leak to a self-hosted server.
    let bearer = cfg.bearer_for(url)?;
    open_remote_memory_backend_with_bearer(cfg, url, bearer).await
}

/// Same as [`open_remote_memory_backend`] but with an injected
/// [`SecretStore`](crate::config::secret_store::SecretStore), so tests can
/// drive the cloud-routing seam without touching the real default secret
/// store. Tests that must drive the non-loopback branch against a
/// plaintext-http mock (wiremock addressed via `0.0.0.0`) call this directly to
/// bypass the transport guard the production entry point enforces.
#[cfg(test)]
async fn open_remote_memory_backend_with_store(
    cfg: &crate::config::Config,
    url: &str,
    store: &dyn crate::config::secret_store::SecretStore,
) -> Result<Box<dyn MemoryBackend + Send>> {
    let bearer = cfg.bearer_for_with_store(url, store)?;
    open_remote_memory_backend_with_bearer(cfg, url, bearer).await
}

async fn open_remote_memory_backend_with_bearer(
    cfg: &crate::config::Config,
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
    let client = crate::config::apply_server_ca(
        reqwest::Client::builder(),
        cfg.server_ca.as_deref().map(std::path::Path::new),
    )?
    .timeout(std::time::Duration::from_secs(30))
    .build()?;

    // `project_id` goes on the wire exactly as configured, slug or UUID.
    // Both peers accept either: the OSS team server stores whatever string it
    // is given as `projects.slug`, and cloud-api's project path params resolve
    // a slug or a UUID (cloud-api 66fd265). `CloudSyncClient` has always passed
    // it through this way.
    //
    // Which memory dialect that peer speaks is settled once, here, rather than
    // branched on inside every CRUD method. Any uncertain probe resolves to the
    // team-server dialect, which is what this function returned unconditionally
    // before the probe existed.
    match remote::detect_dialect(&client, url).await {
        remote::PeerDialect::CloudApi => Ok(Box::new(remote::CloudApiMemoryBackend {
            client,
            base_url: url.to_string(),
            project_id,
            api_key: bearer,
        })),
        remote::PeerDialect::TeamServer => Ok(Box::new(RemoteMemoryBackend {
            client,
            base_url: url.to_string(),
            project_id,
            api_key: bearer,
        })),
    }
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
mod backend_selection_tests;
