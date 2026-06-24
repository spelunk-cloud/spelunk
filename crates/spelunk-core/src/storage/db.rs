use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;

/// Wraps the SQLite connection and provides typed access to the schema.
/// Methods are implemented across sub-modules in the `storage` package.
pub struct Database {
    pub(super) conn: Connection,
}

impl Database {
    /// Open (or create) the database at `path` and run all migrations.
    ///
    /// Assumes `sqlite3_auto_extension` has already been called in `main` to
    /// load the sqlite-vec extension into every new connection.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating db directory {}", parent.display()))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("opening database at {}", path.display()))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;

        let db = Self { conn };
        db.migrate()?;
        db.apply_vector_migration()?;
        db.apply_graph_migration()?;
        db.apply_spec_migration()?;
        db.apply_fts_migration()?;
        db.apply_token_count_migration()?;
        db.apply_graph_rank_migration()?;
        db.apply_summary_migration()?;
        db.apply_usage_migration()?;
        db.apply_snapshot_migration()?;
        db.apply_snapshot_vector_migration()?;
        db.apply_compound_graph_idx_migration()?;
        db.apply_conventions_migration()?;
        db.apply_dim_upgrade_migration()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/001_initial.sql"))
            .context("running base migrations")?;
        Ok(())
    }

    /// Create the sqlite-vec virtual table. Idempotent (`IF NOT EXISTS`).
    pub fn apply_vector_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/002_vectors.sql"))
            .context("running vector migration (is the sqlite-vec extension loaded?)")?;
        Ok(())
    }

    /// Create the graph_edges table. Idempotent (`IF NOT EXISTS`).
    pub fn apply_graph_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/003_graph.sql"))
            .context("running graph migration")?;
        Ok(())
    }

    /// Create the specs and spec_links tables. Idempotent (`IF NOT EXISTS`).
    pub fn apply_spec_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/006_specs.sql"))
            .context("running spec migration")?;
        Ok(())
    }

    /// Create the FTS5 virtual table and sync triggers. Idempotent (`IF NOT EXISTS`).
    /// Also backfills any existing chunks not yet in the FTS index.
    pub fn apply_fts_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/007_fts.sql"))
            .context("running FTS migration")?;
        self.conn
            .execute_batch(
                "INSERT INTO chunks_fts(rowid, name, content, node_type)
                 SELECT id, name, content, node_type FROM chunks
                 WHERE id NOT IN (SELECT rowid FROM chunks_fts);",
            )
            .context("backfilling FTS index")?;
        Ok(())
    }

    /// Add token_count column to chunks table. Idempotent (column has DEFAULT 0).
    pub fn apply_token_count_migration(&self) -> Result<()> {
        let _ = self
            .conn
            .execute_batch(include_str!("../../migrations/008_token_counts.sql"));
        Ok(())
    }

    /// Add graph_rank column to chunks table. Idempotent (column has DEFAULT 0.0).
    pub fn apply_graph_rank_migration(&self) -> Result<()> {
        let _ = self
            .conn
            .execute_batch(include_str!("../../migrations/009_graph_rank.sql"));
        Ok(())
    }

    /// Add summary column to chunks table. Idempotent.
    pub fn apply_summary_migration(&self) -> Result<()> {
        let _ = self
            .conn
            .execute_batch(include_str!("../../migrations/010_summaries.sql"));
        Ok(())
    }

    /// Create the usage table. Idempotent (`IF NOT EXISTS`).
    pub fn apply_usage_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/011_usage.sql"))
            .context("running usage migration")?;
        Ok(())
    }

    /// Create the snapshots, snapshot_files, and snapshot_chunks tables. Idempotent.
    pub fn apply_snapshot_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/016_snapshots.sql"))
            .context("running snapshot migration")?;
        Ok(())
    }

    /// Create the snapshot_embeddings vec0 virtual table. Idempotent.
    /// Must be called after sqlite-vec extension is loaded.
    pub fn apply_snapshot_vector_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/017_snapshot_vectors.sql"))
            .context("running snapshot vector migration")?;
        Ok(())
    }

    /// Upgrade the sqlite-vec embedding tables from 768-dim (Nomic) to 896-dim (F2LLM-v2-330M).
    ///
    /// Idempotent — guarded by the `schema_v896_embeddings` marker table. On
    /// fresh databases the tables are already created at 896-dim by
    /// `apply_vector_migration` / `apply_snapshot_vector_migration`, so this
    /// is a fast no-op. On existing 768-dim databases the tables are dropped
    /// and recreated; a full `spelunk index` re-run is required afterwards.
    pub fn apply_dim_upgrade_migration(&self) -> Result<()> {
        let already: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_int8_embeddings'",
                [],
                |_| Ok(true),
            )
            .optional()
            .context("checking v896 migration marker")?
            .is_some();
        if already {
            return Ok(());
        }

        // Detect whether existing vec0 tables were created with FLOAT[768].
        let upgrade_needed = |table: &str| -> Result<bool> {
            Ok(self
                .conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                    rusqlite::params![table],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .context("querying sqlite_master")?
                // Any float-typed vector table (768 or 896 dim) is rebuilt as
                // int8[896]; F2LLM embeddings are L2-normalised so int8 is
                // lossless enough for ranking and 4× smaller on disk.
                .map(|sql| sql.contains("FLOAT["))
                .unwrap_or(false))
        };

        if upgrade_needed("embeddings")? {
            self.conn
                .execute_batch(
                    "DROP TABLE IF EXISTS embeddings; \
                     CREATE VIRTUAL TABLE embeddings USING vec0(\
                         chunk_id INTEGER PRIMARY KEY, embedding INT8[896]\
                     );",
                )
                .context("upgrading embeddings table to int8[896]")?;
            tracing::info!(
                "embedding storage upgraded to int8[896] (F2LLM-v2-330M); \
                 re-run `spelunk index` to rebuild"
            );
        }
        if upgrade_needed("snapshot_embeddings")? {
            self.conn
                .execute_batch(
                    "DROP TABLE IF EXISTS snapshot_embeddings; \
                     CREATE VIRTUAL TABLE snapshot_embeddings USING vec0(\
                         chunk_id INTEGER PRIMARY KEY, embedding INT8[896]\
                     );",
                )
                .context("upgrading snapshot_embeddings table to int8[896]")?;
        }

        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_int8_embeddings \
                 (sentinel INTEGER PRIMARY KEY);",
            )
            .context("creating int8 migration marker")?;
        Ok(())
    }

    /// Create compound indexes on graph_edges for LinearRAG mention lookups. Idempotent.
    pub fn apply_compound_graph_idx_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!(
                "../../migrations/018_graph_edges_compound_idx.sql"
            ))
            .context("running compound graph index migration")?;
        Ok(())
    }

    /// Insert or replace an embedding for a chunk.
    ///
    /// Takes the raw float vector; it is int8-quantised here for the
    /// `embeddings` `int8[896]` column (see `embeddings::vec_to_int8_blob`).
    pub fn insert_embedding(&self, chunk_id: i64, vector: &[f32]) -> Result<()> {
        let blob = crate::embeddings::vec_to_int8_blob(vector);
        // sqlite-vec treats a raw BLOB as float32; vec_int8() reinterprets the
        // bytes as the int8 vector the column expects.
        self.conn.execute(
            "INSERT OR REPLACE INTO embeddings (chunk_id, embedding) VALUES (?1, vec_int8(?2))",
            rusqlite::params![chunk_id, blob],
        )?;
        Ok(())
    }

    /// Delete all embeddings associated with chunks of a given file.
    pub fn delete_embeddings_for_file(&self, file_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM embeddings WHERE chunk_id IN (
                 SELECT id FROM chunks WHERE file_id = ?1
             )",
            rusqlite::params![file_id],
        )?;
        Ok(())
    }
}
