use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;

mod edges;
mod notes;
mod search;
mod sync;

pub use sync::SyncRow;

#[cfg(test)]
mod tests;

pub struct MemoryStore {
    pub(super) conn: Connection,
}

#[derive(Debug, Serialize)]
pub struct MemoryEdge {
    pub from_id: i64,
    pub to_id: i64,
    pub kind: String,
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
pub struct Note {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub linked_files: Vec<String>,
    pub created_at: i64,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<i64>,
    /// Git commit SHA for harvested entries; NULL for manually created entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    /// When this entry became valid (unix epoch). None = treat as created_at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_at: Option<i64>,
    /// When this entry was invalidated/superseded (unix epoch). None = still valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_at: Option<i64>,
    /// Semantic distance — only populated by search(), None otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f64>,
    /// Fused relevance score — only populated by hybrid search, None otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Set only for notes returned via cross-project dep pass. None for local notes.
    /// Contains the dep project's display name (final path component of root_path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_project: Option<String>,
    /// Set alongside source_project: the dep project's root path, for disambiguation
    /// when two linked projects share a display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_project_path: Option<String>,
}

impl MemoryStore {
    /// Execute a raw SQL batch statement on the connection.
    ///
    /// Exposed for transaction management in callers that need BEGIN/COMMIT/ROLLBACK
    /// without access to the private `conn` field (e.g. `memory reconcile`).
    pub fn execute_batch(&self, sql: &str) -> rusqlite::Result<()> {
        self.conn.execute_batch(sql)
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening memory DB at {}", path.display()))?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../../migrations/004_memory.sql"))
            .context("running memory migrations")?;
        // Migration 005: lifecycle columns — ALTER TABLE doesn't support IF NOT EXISTS,
        // so we ignore "duplicate column name" errors (idempotent re-open).
        for stmt in [
            "ALTER TABLE notes ADD COLUMN status TEXT NOT NULL DEFAULT 'active'",
            "ALTER TABLE notes ADD COLUMN superseded_by INTEGER REFERENCES notes(id)",
            // Migration 012: commit provenance
            "ALTER TABLE notes ADD COLUMN source_ref TEXT",
        ] {
            match self.conn.execute_batch(stmt) {
                Ok(_) => {}
                Err(e) if e.to_string().contains("duplicate column name") => {}
                Err(e) => return Err(e).context("running memory lifecycle migration"),
            }
        }
        // Migration 012: FTS5 full-text index for memory notes.
        self.conn
            .execute_batch(include_str!("../../../migrations/012_memory_fts.sql"))
            .context("running memory FTS migration")?;
        // Migration 014: temporal fields — valid_at and invalid_at.
        for stmt in [
            "ALTER TABLE notes ADD COLUMN valid_at INTEGER",
            "ALTER TABLE notes ADD COLUMN invalid_at INTEGER",
            "CREATE INDEX IF NOT EXISTS idx_memory_invalid_at ON notes(invalid_at)",
        ] {
            match self.conn.execute_batch(stmt) {
                Ok(_) => {}
                Err(e) if e.to_string().contains("duplicate column name") => {}
                Err(e) => return Err(e).context("running memory temporal migration"),
            }
        }
        // Migration 015: memory edge table.
        self.conn
            .execute_batch(include_str!("../../../migrations/015_memory_edges.sql"))
            .context("running memory edges migration")?;
        // Migration 020 (ADR-037): UUID identity columns (`uuid`, `remote_id`).
        // ALTER TABLE can't be IF NOT EXISTS; guard the duplicate-column case so
        // re-opening an already-migrated store is a no-op. The unique indexes are
        // CREATE … IF NOT EXISTS and safe to run unconditionally.
        for stmt in [
            "ALTER TABLE notes ADD COLUMN uuid TEXT",
            "ALTER TABLE notes ADD COLUMN remote_id TEXT",
        ] {
            match self.conn.execute_batch(stmt) {
                Ok(_) => {}
                Err(e) if e.to_string().contains("duplicate column name") => {}
                Err(e) => return Err(e).context("running memory uuid migration"),
            }
        }
        self.conn
            .execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_uuid \
                 ON notes(uuid) WHERE uuid IS NOT NULL; \
                 CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_remote_id \
                 ON notes(remote_id) WHERE remote_id IS NOT NULL;",
            )
            .context("creating memory uuid indexes")?;
        // No sync-state watermark table (decision #183): the pull cursor is
        // derived from `MAX(remote_id)` over `notes`, so there is no separate
        // watermark to persist. The unique `idx_notes_remote_id` above is what
        // makes that cursor lookup and the `remote_id` dedupe cheap.

        // Upgrade note_embeddings from 768-dim (Nomic) to 896-dim (F2LLM-v2-330M).
        // Guarded by a marker table so re-opening an already-upgraded store is a no-op.
        // Fresh stores get FLOAT[896] directly from 004_memory.sql above.
        let already_v896: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master \
                 WHERE type='table' AND name='schema_v896_note_embeddings'",
                [],
                |_| Ok(true),
            )
            .optional()
            .context("checking v896 note_embeddings marker")?
            .is_some();
        if !already_v896 {
            let needs_upgrade: bool = self
                .conn
                .query_row(
                    "SELECT sql FROM sqlite_master \
                     WHERE type='table' AND name='note_embeddings'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .context("querying note_embeddings schema")?
                .map(|sql| sql.contains("FLOAT[768]"))
                .unwrap_or(false);
            if needs_upgrade {
                self.conn
                    .execute_batch(
                        "DROP TABLE IF EXISTS note_embeddings; \
                         CREATE VIRTUAL TABLE note_embeddings USING vec0(\
                             note_id INTEGER PRIMARY KEY, embedding FLOAT[896]\
                         );",
                    )
                    .context("upgrading note_embeddings to 896-dim")?;
                tracing::info!(
                    "memory note_embeddings dim upgraded 768→896; \
                     re-run `spelunk memory harvest` to rebuild embeddings"
                );
            }
            self.conn
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS schema_v896_note_embeddings \
                     (sentinel INTEGER PRIMARY KEY);",
                )
                .context("creating v896 note_embeddings marker")?;
        }
        Ok(())
    }
}
