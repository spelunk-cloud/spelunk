use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use std::cell::Cell;
use std::path::Path;

mod dedupe;
mod edges;
mod entity_id_migration;
mod notes;
mod search;
mod sync;

pub use dedupe::DedupeSummary;
pub use sync::SyncRow;

#[cfg(test)]
mod tests;

/// Latest local memory schema version. Append-only: never renumber an
/// existing step. The runner in `MemoryStore::run_migrations` gates each
/// migration on this via `PRAGMA user_version`; steps are numbered in the
/// order they run (the field order), not filename order. Mirrors
/// `storage::db::CURRENT_SCHEMA_VERSION` for `index.db`, kept as a distinct
/// constant/name because the two DBs version independently.
pub(super) const MEMORY_SCHEMA_VERSION: i32 = 9;

/// One entry in the migration runner: (target version, migration body).
type MemoryMigrationStep = (i32, fn(&MemoryStore) -> Result<()>);

pub struct MemoryStore {
    pub(super) conn: Connection,
    /// Set by [`MemoryStore::open`] to the count of active notes that need
    /// re-embedding when the 768→896 migration dropped their vectors on THIS
    /// open; `None` on every other open. Lets the CLI surface a one-line
    /// `memory reindex` hint without `RUST_LOG` while keeping this library
    /// side-effect-free (nothing is printed here).
    pub reembed_needed: Option<usize>,
    /// Set inside `apply_dim_upgrade_migration` only when THIS call actually
    /// dropped a 768-dim `note_embeddings` table (not merely when the marker
    /// was absent), so `open` can compute the one-time re-embed count. A
    /// `Cell` because migration steps take `&self`, not `&mut self` (they
    /// share the `MemoryMigrationStep` signature with every other step,
    /// which only needs shared access to `conn`).
    dropped_768: Cell<bool>,
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
    /// Canonical cross-machine id (uuid) when synced to a remote; None for
    /// never-synced local rows. Carried from the remote wire (ADR-059 D2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
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
        super::apply_test_page_cap(&conn)?;
        let mut store = Self {
            conn,
            reembed_needed: None,
            dropped_768: Cell::new(false),
        };
        store.run_migrations()?;
        // ADR-068 third amendment: Step A (unconditional backfill) then Step B
        // (conditional UNIQUE-index promotion). Neither ever hard-aborts open;
        // see `entity_id_migration.rs`. Both run on every open rather than
        // being gated to a single schema-version step: a later insert path
        // can still leave `entity_id` NULL (Step A heals it), and Step B's
        // duplicate scan can only turn clean after a `spelunk memory dedupe`
        // run on some later open.
        store.backfill_entity_ids()?;
        store.promote_entity_id_unique_index()?;
        if store.dropped_768.get() {
            // The upgrade just discarded every prior note's vector, so every
            // active note now needs re-embedding; count them once so the CLI
            // can point the user at `memory reindex` (there is no catch-up path
            // otherwise).
            let n = store.notes_missing_embeddings(false)?.len();
            if n > 0 {
                store.reembed_needed = Some(n);
            }
        }
        Ok(store)
    }

    /// Forward-only migration runner gated on `PRAGMA user_version`, mirroring
    /// `Database::run_migrations` in `storage/db.rs`.
    ///
    /// A `user_version=0` DB is either brand-new (no user tables) or a
    /// pre-`user_version` field DB: every `memory.db` on disk before this
    /// runner existed, since `migrate()` never stamped a version. New DBs run
    /// every step from 1. Field DBs have their true version inferred from
    /// table/column shape, stamped, and only later steps run: blindly
    /// re-running every step would still be safe (each step is idempotent),
    /// but inference avoids redundant ALTER-then-catch round trips.
    fn run_migrations(&self) -> Result<()> {
        let mut version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .context("reading user_version")?;

        if version > MEMORY_SCHEMA_VERSION {
            anyhow::bail!(
                "memory.db schema version {version} is newer than this build of spelunk \
                 supports (max {MEMORY_SCHEMA_VERSION}); upgrade spelunk to open this store."
            );
        }

        if version == 0 && !self.is_fresh_db()? {
            version = self.infer_legacy_version()?;
        }

        // Each entry: (target_version, migration body). Ordered by call order.
        // Append new steps at the end; never renumber.
        let steps: &[MemoryMigrationStep] = &[
            (1, Self::apply_base_migration),
            (2, Self::apply_lifecycle_migration),
            (3, Self::apply_source_ref_migration),
            (4, Self::apply_fts_migration),
            (5, Self::apply_temporal_migration),
            (6, Self::apply_edges_migration),
            (7, Self::apply_uuid_migration),
            (8, Self::apply_entity_id_column_migration),
            (9, Self::apply_dim_upgrade_migration),
        ];
        debug_assert_eq!(
            steps.last().map(|(v, _)| *v),
            Some(MEMORY_SCHEMA_VERSION),
            "steps table must end at MEMORY_SCHEMA_VERSION"
        );

        for (target, body) in steps {
            if *target > version {
                body(self)?;
            }
        }
        // user_version is a header i32; the value is a code-controlled constant.
        self.conn
            .execute_batch(&format!("PRAGMA user_version = {MEMORY_SCHEMA_VERSION}"))
            .context("stamping user_version")?;
        Ok(())
    }

    /// True when the file has no user tables: a freshly created DB that must
    /// run every migration from step 1.
    fn is_fresh_db(&self) -> Result<bool> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master \
                 WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .context("counting user tables")?;
        Ok(n == 0)
    }

    /// Infer the schema version of a pre-`user_version` field DB from its
    /// table/column shapes. Walks the ladder top-down; the first unmet
    /// predicate fixes the version. A conservative (one-low) result is safe:
    /// the re-run step is a no-op guard (or a tolerated duplicate-column
    /// error) that then advances the version. Each predicate must therefore
    /// cover every column/object its step adds, not just one: a partial
    /// match would infer the step "done" and skip it forever.
    fn infer_legacy_version(&self) -> Result<i32> {
        let has_table = |name: &str| -> Result<bool> {
            Ok(self
                .conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                    rusqlite::params![name],
                    |_| Ok(()),
                )
                .optional()
                .context("probing table")?
                .is_some())
        };
        let notes_has_column = |col: &str| -> Result<bool> {
            let mut stmt = self.conn.prepare("PRAGMA table_info(notes)")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                if row.get::<_, String>(1)? == col {
                    return Ok(true);
                }
            }
            Ok(false)
        };

        // Each predicate must cover every column/object its step adds, not just
        // the first: steps 2, 5 and 7 each `ALTER TABLE ADD COLUMN` twice in
        // one loop, and SQLite auto-commits each ALTER independently, so a
        // process killed between the two statements is a real field state.
        // Checking only the first column would infer that step "done" from a
        // half-applied ALTER and skip it forever, leaving the second column
        // permanently missing.
        let ladder: [(i32, bool); 9] = [
            (1, has_table("notes")?),
            (
                2,
                notes_has_column("status")? && notes_has_column("superseded_by")?,
            ),
            (3, notes_has_column("source_ref")?),
            (4, has_table("memory_fts")?),
            (
                5,
                notes_has_column("valid_at")? && notes_has_column("invalid_at")?,
            ),
            (6, has_table("memory_edges")?),
            (
                7,
                notes_has_column("uuid")? && notes_has_column("remote_id")?,
            ),
            (8, notes_has_column("entity_id")?),
            (9, has_table("schema_v896_note_embeddings")?),
        ];
        // Highest version whose predicate and all lower ones hold.
        let mut version = 0;
        for (v, satisfied) in ladder {
            if !satisfied {
                break;
            }
            version = v;
        }
        Ok(version)
    }

    /// Create the base `notes` + `note_embeddings` tables. Idempotent
    /// (`CREATE TABLE/VIRTUAL TABLE IF NOT EXISTS`).
    fn apply_base_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../../migrations/004_memory.sql"))
            .context("running base memory migration")?;
        Ok(())
    }

    /// Add lifecycle columns (`status`, `superseded_by`). `ALTER TABLE` has no
    /// `IF NOT EXISTS`, so only the already-applied error is tolerated; a
    /// genuine failure propagates.
    fn apply_lifecycle_migration(&self) -> Result<()> {
        for stmt in [
            "ALTER TABLE notes ADD COLUMN status TEXT NOT NULL DEFAULT 'active'",
            "ALTER TABLE notes ADD COLUMN superseded_by INTEGER REFERENCES notes(id)",
        ] {
            match self.conn.execute_batch(stmt) {
                Ok(_) => {}
                Err(e) if e.to_string().contains("duplicate column name") => {}
                Err(e) => return Err(e).context("running memory lifecycle migration"),
            }
        }
        Ok(())
    }

    /// Add `source_ref` (commit provenance for harvested entries).
    fn apply_source_ref_migration(&self) -> Result<()> {
        match self.conn.execute_batch(include_str!(
            "../../../migrations/013_memory_source_ref.sql"
        )) {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e).context("running memory source_ref migration"),
        }
        Ok(())
    }

    /// Create the FTS5 full-text index over notes (and its sync triggers).
    /// Idempotent (`CREATE VIRTUAL TABLE/TRIGGER IF NOT EXISTS`, `INSERT OR
    /// IGNORE` backfill).
    fn apply_fts_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../../migrations/012_memory_fts.sql"))
            .context("running memory FTS migration")?;
        Ok(())
    }

    /// Add temporal fields (`valid_at`, `invalid_at`) and their index.
    fn apply_temporal_migration(&self) -> Result<()> {
        for stmt in [
            "ALTER TABLE notes ADD COLUMN valid_at INTEGER",
            "ALTER TABLE notes ADD COLUMN invalid_at INTEGER",
        ] {
            match self.conn.execute_batch(stmt) {
                Ok(_) => {}
                Err(e) if e.to_string().contains("duplicate column name") => {}
                Err(e) => return Err(e).context("running memory temporal migration"),
            }
        }
        self.conn
            .execute_batch("CREATE INDEX IF NOT EXISTS idx_memory_invalid_at ON notes(invalid_at)")
            .context("creating memory temporal index")?;
        Ok(())
    }

    /// Create the memory relationship-edges table. Idempotent (`CREATE TABLE/
    /// INDEX IF NOT EXISTS`).
    fn apply_edges_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../../migrations/015_memory_edges.sql"))
            .context("running memory edges migration")?;
        Ok(())
    }

    /// Add cross-store identity columns (`uuid`, `remote_id`) and their
    /// partial unique indexes for cloud sync.
    ///
    /// No sync-state watermark table (decision #183): the pull cursor is
    /// derived from `MAX(remote_id)` over `notes`, so there is no separate
    /// watermark to persist. `idx_notes_remote_id` below is what makes that
    /// cursor lookup and the `remote_id` dedupe cheap.
    fn apply_uuid_migration(&self) -> Result<()> {
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
        Ok(())
    }

    /// Add the content-addressed `entity_id` column (ADR-068) and its
    /// non-unique index: existing stores can hold rows with identical
    /// kind/title/body, so a UNIQUE index here would abort the migration.
    /// `entity_id_migration.rs`'s Step B promotes it to UNIQUE separately,
    /// once a duplicate scan comes back clean; Step A backfills the column's
    /// values, both on every open, not gated to this schema-version step.
    fn apply_entity_id_column_migration(&self) -> Result<()> {
        match self
            .conn
            .execute_batch("ALTER TABLE notes ADD COLUMN entity_id TEXT")
        {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e).context("running memory entity_id migration"),
        }
        self.conn
            .execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_notes_entity_id \
                 ON notes(entity_id) WHERE entity_id IS NOT NULL;",
            )
            .context("creating memory entity_id index")?;
        Ok(())
    }

    /// Upgrade `note_embeddings` from 768-dim (Nomic) to 896-dim
    /// (F2LLM-v2-330M). Idempotent, guarded by the
    /// `schema_v896_note_embeddings` marker table so re-opening an
    /// already-upgraded store is a no-op; fresh stores already get
    /// `FLOAT[896]` directly from `apply_base_migration`. Sets `dropped_768`
    /// only when THIS call actually dropped a 768-dim table (not merely when
    /// the marker was absent), so `open` can compute the re-embed count
    /// exactly once per real drop.
    fn apply_dim_upgrade_migration(&self) -> Result<()> {
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
        if already_v896 {
            return Ok(());
        }

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
                 re-run `spelunk memory reindex` to rebuild embeddings"
            );
            self.dropped_768.set(true);
        }
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_v896_note_embeddings \
                 (sentinel INTEGER PRIMARY KEY);",
            )
            .context("creating v896 note_embeddings marker")?;
        Ok(())
    }
}
