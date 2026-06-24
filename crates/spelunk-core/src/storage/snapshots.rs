#![allow(dead_code)]
use anyhow::Result;

use super::Database;

/// A recorded snapshot of the codebase at a specific commit.
#[derive(Debug, serde::Serialize)]
pub struct Snapshot {
    pub id: i64,
    pub commit_sha: String,
    pub created_at: i64,
    pub file_count: i64,
    pub chunk_count: i64,
}

fn row_to_snapshot(row: &rusqlite::Row<'_>) -> rusqlite::Result<Snapshot> {
    Ok(Snapshot {
        id: row.get(0)?,
        commit_sha: row.get(1)?,
        created_at: row.get(2)?,
        file_count: row.get(3)?,
        chunk_count: row.get(4)?,
    })
}

/// A single version of a symbol, from either the live index or a snapshot.
#[derive(Debug, serde::Serialize)]
pub struct SymbolVersion {
    pub chunk_id: i64,
    pub node_type: String,
    pub name: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
    pub file_path: String,
    /// None = live index; Some(sha) = snapshot
    pub commit_sha: Option<String>,
    /// Unix timestamp of the snapshot, or None for live index
    pub snapshot_created_at: Option<i64>,
}

impl Database {
    /// Insert a new snapshot record. Returns the snapshot id.
    pub fn create_snapshot(&self, commit_sha: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO snapshots (commit_sha) VALUES (?1)",
            rusqlite::params![commit_sha],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Lookup a snapshot by commit SHA (exact match). Returns None if not found.
    pub fn get_snapshot_by_sha(&self, commit_sha: &str) -> Result<Option<Snapshot>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, commit_sha, created_at, file_count, chunk_count
             FROM snapshots WHERE commit_sha = ?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![commit_sha], row_to_snapshot)?;
        Ok(rows.next().transpose()?)
    }

    /// Return all snapshots ordered by creation time (newest first).
    pub fn list_snapshots(&self) -> Result<Vec<Snapshot>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, commit_sha, created_at, file_count, chunk_count
             FROM snapshots ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_snapshot)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Delete a snapshot and all its files, chunks, and embeddings (via CASCADE).
    pub fn delete_snapshot(&self, commit_sha: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "DELETE FROM snapshots WHERE commit_sha = ?1",
            rusqlite::params![commit_sha],
        )?;
        Ok(changed > 0)
    }

    /// Update file_count and chunk_count after indexing is complete.
    pub fn update_snapshot_stats(
        &self,
        snapshot_id: i64,
        file_count: i64,
        chunk_count: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE snapshots SET file_count = ?2, chunk_count = ?3 WHERE id = ?1",
            rusqlite::params![snapshot_id, file_count, chunk_count],
        )?;
        Ok(())
    }

    /// Insert a file record for a snapshot. Returns the snapshot_file id.
    pub fn insert_snapshot_file(
        &self,
        snapshot_id: i64,
        path: &str,
        language: Option<&str>,
        hash: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO snapshot_files (snapshot_id, path, language, hash) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![snapshot_id, path, language, hash],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Insert a chunk record for a snapshot. Returns the snapshot_chunk id.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_snapshot_chunk(
        &self,
        snapshot_id: i64,
        file_id: i64,
        node_type: &str,
        name: Option<&str>,
        start_line: usize,
        end_line: usize,
        content: &str,
        metadata: Option<&str>,
        token_count: usize,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO snapshot_chunks
             (snapshot_id, file_id, node_type, name, start_line, end_line, content, metadata, token_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                snapshot_id,
                file_id,
                node_type,
                name,
                start_line as i64,
                end_line as i64,
                content,
                metadata,
                token_count as i64,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Insert an embedding for a snapshot chunk.
    ///
    /// Takes the raw float vector; int8-quantised here for the
    /// `snapshot_embeddings` `int8[896]` column (see `vec_to_int8_blob`).
    pub fn insert_snapshot_embedding(&self, chunk_id: i64, vector: &[f32]) -> Result<()> {
        let blob = crate::embeddings::vec_to_int8_blob(vector);
        // vec_int8() reinterprets the raw blob as int8 (sqlite-vec defaults to f32).
        self.conn.execute(
            "INSERT OR REPLACE INTO snapshot_embeddings (chunk_id, embedding) VALUES (?1, vec_int8(?2))",
            rusqlite::params![chunk_id, blob],
        )?;
        Ok(())
    }

    /// Delete snapshot embeddings for a snapshot (vec0 tables don't honour ON DELETE CASCADE).
    pub fn delete_snapshot_embeddings_for_snapshot(&self, snapshot_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM snapshot_embeddings
             WHERE chunk_id IN (SELECT id FROM snapshot_chunks WHERE snapshot_id = ?1)",
            rusqlite::params![snapshot_id],
        )?;
        Ok(())
    }

    /// KNN search within a specific snapshot. Returns results ordered by ascending distance.
    pub fn search_snapshot(
        &self,
        snapshot_id: i64,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<crate::search::SearchResult>> {
        let limit = limit.min(1_000);
        let query_blob = crate::embeddings::vec_to_int8_blob(query);
        let sql = format!(
            "WITH knn AS (
                 SELECT chunk_id, distance
                 FROM   snapshot_embeddings
                 WHERE  embedding MATCH vec_int8(?1)
                   AND  k = {limit}
             )
             SELECT  sc.id,
                     CAST(k.distance AS REAL),
                     sc.node_type,
                     sc.name,
                     CAST(sc.start_line AS INTEGER),
                     CAST(sc.end_line   AS INTEGER),
                     sc.content,
                     sf.path,
                     sf.language,
                     sc.token_count
             FROM knn k
             JOIN snapshot_chunks sc ON sc.id = k.chunk_id
             JOIN snapshot_files  sf ON sf.id = sc.file_id
             WHERE sc.snapshot_id = ?2
             ORDER BY k.distance"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![&query_blob, snapshot_id], |row| {
            Ok(crate::search::SearchResult {
                chunk_id: row.get(0)?,
                // int8 L2 distance is ~127× the f32 distance; rescale to match.
                distance: row.get::<_, f32>(1)? / crate::embeddings::INT8_SCALE,
                node_type: row.get(2)?,
                name: row.get(3)?,
                start_line: row.get::<_, i64>(4)? as usize,
                end_line: row.get::<_, i64>(5)? as usize,
                content: row.get(6)?,
                file_path: row.get(7)?,
                language: row.get(8)?,
                from_graph: false,
                governing_specs: vec![],
                token_count: row.get::<_, i64>(9)? as usize,
                project_name: None,
                project_path: None,
                summary: None,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Return all versions of a symbol across all snapshots plus the live index.
    /// `name_fragment` is matched as a suffix.
    pub fn symbol_history(&self, name_fragment: &str) -> Result<Vec<SymbolVersion>> {
        // Escape LIKE metacharacters in the user-supplied name fragment so that
        // '%' and '_' are treated as literals, not wildcards.
        let pattern = format!("%{}", super::escape_like(name_fragment));

        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.node_type, c.name, c.start_line, c.end_line, c.content, f.path, NULL, NULL
             FROM chunks c
             JOIN files f ON f.id = c.file_id
             WHERE c.name LIKE ?1 ESCAPE '\\'
             ORDER BY f.path, c.start_line",
        )?;
        let live: Vec<SymbolVersion> = stmt
            .query_map(rusqlite::params![pattern], |row| {
                Ok(SymbolVersion {
                    chunk_id: row.get(0)?,
                    node_type: row.get(1)?,
                    name: row.get(2)?,
                    start_line: row.get::<_, i64>(3)? as usize,
                    end_line: row.get::<_, i64>(4)? as usize,
                    content: row.get(5)?,
                    file_path: row.get(6)?,
                    commit_sha: row.get(7)?,
                    snapshot_created_at: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt2 = self.conn.prepare(
            "SELECT sc.id, sc.node_type, sc.name, sc.start_line, sc.end_line, sc.content,
                    sf.path, s.commit_sha, s.created_at
             FROM snapshot_chunks sc
             JOIN snapshot_files sf ON sf.id = sc.file_id
             JOIN snapshots s       ON s.id  = sc.snapshot_id
             WHERE sc.name LIKE ?1 ESCAPE '\\'
             ORDER BY s.created_at ASC, sf.path, sc.start_line",
        )?;
        let snaps: Vec<SymbolVersion> = stmt2
            .query_map(rusqlite::params![pattern], |row| {
                Ok(SymbolVersion {
                    chunk_id: row.get(0)?,
                    node_type: row.get(1)?,
                    name: row.get(2)?,
                    start_line: row.get::<_, i64>(3)? as usize,
                    end_line: row.get::<_, i64>(4)? as usize,
                    content: row.get(5)?,
                    file_path: row.get(6)?,
                    commit_sha: row.get(7)?,
                    snapshot_created_at: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Snapshots first (ASC by created_at), then live index.
        let mut all = snaps;
        all.extend(live);
        Ok(all)
    }

    /// Return the total number of snapshots stored.
    pub fn snapshot_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))?)
    }
}
