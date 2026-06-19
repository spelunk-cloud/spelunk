use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

/// Wraps the SQLite connection and provides typed access to the schema.
/// Methods are implemented across sub-modules in the `storage` package.
pub struct Database {
    pub(super) conn: Connection,
}

impl Database {
    /// Default embedding dimension (EmbeddingGemma / Nomic Embed Text v1.5).
    pub const DEFAULT_EMBEDDING_DIM: usize = 768;

    /// Open (or create) the database at `path` and run all migrations, creating
    /// the document vector table at the default 768 dimensions.
    ///
    /// Assumes `sqlite3_auto_extension` has already been called in `main` to
    /// load the sqlite-vec extension into every new connection.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_dim(path, Self::DEFAULT_EMBEDDING_DIM)
    }

    /// Like [`Database::open`], but creates the document/snapshot vector tables
    /// with `embedding_dim` dimensions. Used by `index` so that embedding models
    /// whose output dimension differs from 768 index correctly. The vec0 tables
    /// are created `IF NOT EXISTS`, so the dimension is fixed at first creation;
    /// later opens reuse the existing tables regardless of `embedding_dim`.
    pub fn open_with_dim(path: &Path, embedding_dim: usize) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating db directory {}", parent.display()))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("opening database at {}", path.display()))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;

        let db = Self { conn };
        db.migrate()?;
        db.apply_vector_migration(embedding_dim)?;
        db.apply_graph_migration()?;
        db.apply_spec_migration()?;
        db.apply_fts_migration()?;
        db.apply_token_count_migration()?;
        db.apply_graph_rank_migration()?;
        db.apply_summary_migration()?;
        db.apply_usage_migration()?;
        db.apply_snapshot_migration()?;
        db.apply_snapshot_vector_migration(embedding_dim)?;
        db.apply_compound_graph_idx_migration()?;
        db.apply_conventions_migration()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/001_initial.sql"))
            .context("running base migrations")?;
        Ok(())
    }

    /// Create the sqlite-vec virtual table at `embedding_dim` dimensions.
    /// Idempotent (`IF NOT EXISTS`); the dimension is fixed at first creation.
    pub fn apply_vector_migration(&self, embedding_dim: usize) -> Result<()> {
        self.conn
            .execute_batch(&format!(
                "CREATE VIRTUAL TABLE IF NOT EXISTS embeddings USING vec0(\n    \
                 chunk_id INTEGER PRIMARY KEY,\n    embedding FLOAT[{embedding_dim}]\n);"
            ))
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
    pub fn apply_snapshot_vector_migration(&self, embedding_dim: usize) -> Result<()> {
        self.conn
            .execute_batch(&format!(
                "CREATE VIRTUAL TABLE IF NOT EXISTS snapshot_embeddings USING vec0(\n    \
                 chunk_id  INTEGER PRIMARY KEY,\n    embedding FLOAT[{embedding_dim}]\n);"
            ))
            .context("running snapshot vector migration")?;
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
    /// `blob` must be raw little-endian F32 bytes (see `embeddings::vec_to_blob`).
    pub fn insert_embedding(&self, chunk_id: i64, blob: &[u8]) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO embeddings (chunk_id, embedding) VALUES (?1, ?2)",
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
