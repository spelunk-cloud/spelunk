pub mod backend;
pub mod db;
pub mod git_meta;
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
mod snapshots;
mod specs;
mod stats;

pub use backend::{LocalMemoryBackend, MemoryBackend, NoteInput};
pub use conventions::{ConventionRow, RawChunkRow, has_doc_prefix};
pub use db::Database;
pub use files::FileRecord;
pub use git_meta::GitMetaBackend;
pub use git_notes::GitNotesBackend;
pub use graph::GraphEdge;
pub use memory::{MemoryEdge, MemoryStore};
pub use remote::RemoteMemoryBackend;
pub use snapshots::{Snapshot, SymbolVersion};
pub use specs::{SpecRecord, StaleSpec};
pub use stats::{DriftCandidate, IndexStats, LanguageStat, StalenessReport, record_usage_at};

use anyhow::Result;
use std::path::Path;

/// Open the appropriate memory backend.
///
/// Resolution order (decisions #73, #76 — issues #182, #283):
///
/// 1. **Explicit `backend_override`** — an explicit `--backend` flag always
///    wins: `Some("git-meta")` → [`GitMetaBackend`], `Some("git-notes")` →
///    [`GitNotesBackend`], `Some("sqlite")` → local SQLite at `mem_path`
///    (the opt-out for local semantic search).
/// 2. **`server_url` configured** → [`RemoteMemoryBackend`] (the server embeds;
///    the CLI never computes vectors locally).
/// 3. **Local embedder configured** (see [`Config::local_embedder_configured`])
///    → local SQLite at `mem_path`, enabling on-disk semantic search.
/// 4. **Default** → [`GitMetaBackend`]: zero-infra, git-native, text/FTS only.
///    This is the fresh-install default (previously SQLite).
///
/// `backend_override = None` (the clap `--backend auto` default) runs branches
/// 2–4. Pass `Some("sqlite")` to force SQLite regardless of config.
pub fn open_memory_backend(
    cfg: &crate::config::Config,
    mem_path: &Path,
    backend_override: Option<&str>,
) -> Result<Box<dyn MemoryBackend + Send>> {
    // ── Branch 1: explicit --backend flag wins ───────────────────────────────
    match backend_override {
        Some("git-meta") => return Ok(Box::new(GitMetaBackend::new())),
        Some("git-notes") => return Ok(Box::new(GitNotesBackend::new())),
        Some("sqlite") => {
            return Ok(Box::new(LocalMemoryBackend::new(MemoryStore::open(
                mem_path,
            )?)));
        }
        // None ("auto") or any unrecognised value → fall through to config dispatch.
        _ => {}
    }

    // ── Branch 2: remote server configured ───────────────────────────────────
    if let Some(url) = &cfg.server_url {
        let project_id = cfg.project_id.clone().expect(
            "project_id must be set when server_url is configured; \
             call Config::validate() before open_memory_backend()",
        );
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        return Ok(Box::new(RemoteMemoryBackend {
            client,
            base_url: url.clone(),
            project_id,
            api_key: cfg.server_key.clone(),
        }));
    }

    // ── Branch 3: local embedder configured → SQLite (local semantic) ────────
    if cfg.local_embedder_configured() {
        return Ok(Box::new(LocalMemoryBackend::new(MemoryStore::open(
            mem_path,
        )?)));
    }

    // ── Branch 4: default → git-meta (zero-infra) ────────────────────────────
    Ok(Box::new(GitMetaBackend::new()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::OnceLock;

    /// Register the sqlite-vec extension exactly once per test process.
    /// `MemoryStore::open()` migrates a `vec0` virtual table, which requires the
    /// extension to be loaded before any connection is opened (normally done in
    /// the binary's `main`).
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

    /// A config with no server and the default (built-in) api_base_url, i.e. no
    /// local embedder configured. This is the fresh-install shape.
    fn base_cfg() -> Config {
        Config::default()
    }

    fn tmp_mem_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.db");
        (dir, path)
    }

    // ── Branch 1: explicit --backend flag wins ───────────────────────────────

    #[test]
    fn explicit_sqlite_override_selects_sqlite_even_with_server_url() {
        let mut cfg = base_cfg();
        // Even with a server + local embedder configured, the explicit flag wins.
        cfg.server_url = Some("http://example.com:7777".to_string());
        cfg.project_id = Some("acme/app".to_string());
        cfg.api_base_url = "http://127.0.0.1:9999".to_string();
        register_sqlite_vec();
        let (_dir, mem) = tmp_mem_path();

        let be = open_memory_backend(&cfg, &mem, Some("sqlite")).unwrap();
        assert_eq!(be.backend_kind(), "sqlite");
    }

    #[test]
    fn explicit_git_meta_override_selects_git_meta() {
        let mut cfg = base_cfg();
        cfg.server_url = Some("http://example.com:7777".to_string());
        cfg.project_id = Some("acme/app".to_string());
        let (_dir, mem) = tmp_mem_path();

        let be = open_memory_backend(&cfg, &mem, Some("git-meta")).unwrap();
        assert_eq!(be.backend_kind(), "git-meta");
    }

    #[test]
    fn explicit_git_notes_override_selects_git_notes() {
        let cfg = base_cfg();
        let (_dir, mem) = tmp_mem_path();

        let be = open_memory_backend(&cfg, &mem, Some("git-notes")).unwrap();
        assert_eq!(be.backend_kind(), "git-notes");
    }

    // ── Branch 2: server_url configured → remote ─────────────────────────────

    #[test]
    fn server_url_selects_remote_when_no_override() {
        let mut cfg = base_cfg();
        cfg.server_url = Some("http://example.com:7777".to_string());
        cfg.project_id = Some("acme/app".to_string());
        // A local embedder is also configured, but server_url takes precedence.
        cfg.api_base_url = "http://127.0.0.1:9999".to_string();
        let (_dir, mem) = tmp_mem_path();

        let be = open_memory_backend(&cfg, &mem, None).unwrap();
        assert_eq!(be.backend_kind(), "remote");
    }

    // ── Branch 3: local embedder configured → sqlite ─────────────────────────

    #[test]
    fn local_embedder_configured_selects_sqlite_when_no_server() {
        let mut cfg = base_cfg();
        assert!(cfg.server_url.is_none());
        // Non-default api_base_url => a local embedder is configured.
        cfg.api_base_url = "http://127.0.0.1:9999".to_string();
        assert!(cfg.local_embedder_configured());
        register_sqlite_vec();
        let (_dir, mem) = tmp_mem_path();

        let be = open_memory_backend(&cfg, &mem, None).unwrap();
        assert_eq!(be.backend_kind(), "sqlite");
    }

    // ── Branch 4: default → git-meta ─────────────────────────────────────────

    #[test]
    fn default_selects_git_meta_on_fresh_install() {
        let cfg = base_cfg();
        // Fresh install: no server, default api_base_url => no local embedder.
        assert!(cfg.server_url.is_none());
        assert!(!cfg.local_embedder_configured());
        let (_dir, mem) = tmp_mem_path();

        let be = open_memory_backend(&cfg, &mem, None).unwrap();
        assert_eq!(be.backend_kind(), "git-meta");
    }
}
