use anyhow::Result;

use super::Database;

/// A row from the `files` table, returned by [`Database::file_records_under`].
pub struct FileRecord {
    pub id: i64,
    pub path: String,
    pub language: Option<String>,
    pub hash: String,
    pub indexed_at: i64,
}

impl Database {
    /// Insert or update a file record. `mtime` is the file's filesystem
    /// modification time in unix seconds (0 when unavailable), persisted so the
    /// embed queue can order by file recency without re-stat()ing at
    /// queue-build time. On a hash-unchanged file the caller skips this call
    /// entirely, so a file's stored mtime is only refreshed when it is
    /// re-parsed.
    pub fn upsert_file(
        &self,
        path: &str,
        language: Option<&str>,
        hash: &str,
        mtime: i64,
    ) -> Result<i64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        self.conn.execute(
            "INSERT INTO files (path, language, hash, indexed_at, mtime)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
                language   = excluded.language,
                hash       = excluded.hash,
                indexed_at = excluded.indexed_at,
                mtime      = excluded.mtime",
            rusqlite::params![path, language, hash, now, mtime],
        )?;

        // ON CONFLICT UPDATE doesn't reset last_insert_rowid; fetch it explicitly.
        let id: i64 = self.conn.query_row(
            "SELECT id FROM files WHERE path = ?1",
            rusqlite::params![path],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Returns the stored hash for a file path, or None if not indexed.
    pub fn file_hash(&self, path: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT hash FROM files WHERE path = ?1")?;
        let mut rows = stmt.query(rusqlite::params![path])?;
        Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
    }

    /// Returns the stored filesystem mtime (unix secs) for a file path, or None
    /// if not indexed. The persisted counterpart of the recency key the embed
    /// queue orders on.
    pub fn file_mtime(&self, path: &str) -> Result<Option<i64>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT mtime FROM files WHERE path = ?1")?;
        let mut rows = stmt.query(rusqlite::params![path])?;
        Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
    }

    /// Whether a file has at least one stored chunk. A hash-current file with
    /// zero chunks means a prior parse committed `upsert_file`'s new hash but
    /// was interrupted before any chunk of that file landed (no transaction
    /// spans the two writes - see `process_text_file` in the CLI's parse
    /// phase); the hash-only skip check alone cannot see that half-indexed
    /// state.
    pub fn file_has_chunks(&self, path: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM chunks c JOIN files f ON f.id = c.file_id \
             WHERE f.path = ?1)",
        )?;
        stmt.query_row(rusqlite::params![path], |r| r.get::<_, bool>(0))
            .map_err(Into::into)
    }

    /// Look up the file id for a given path, or None if not indexed.
    pub fn file_id_for_path(&self, path: &str) -> Result<Option<i64>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id FROM files WHERE path = ?1")?;
        let mut rows = stmt.query(rusqlite::params![path])?;
        Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
    }

    /// Return all chunk IDs and their content for a given file id.
    pub fn chunks_content_for_file_id(&self, file_id: i64) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, content FROM chunks WHERE file_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map(rusqlite::params![file_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// List all indexed file paths under the given root prefix.
    pub fn file_paths_under(&self, root: &str) -> Result<Vec<(i64, String)>> {
        // Escape LIKE metacharacters in the user-supplied root so that '%' and '_'
        // in real directory names are treated as literals.
        let prefix = format!("{}%", super::escape_like(root));
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, path FROM files WHERE path LIKE ?1 ESCAPE '\\'")?;
        let rows = stmt.query_map(rusqlite::params![prefix], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// List all indexed files under the given root prefix, including hash and indexed_at.
    pub fn file_records_under(&self, root: &str) -> Result<Vec<FileRecord>> {
        // Escape LIKE metacharacters in the user-supplied root so that '%' and '_'
        // in real directory names are treated as literals.
        let prefix = format!("{}%", super::escape_like(root));
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, path, language, hash, indexed_at FROM files WHERE path LIKE ?1 ESCAPE '\\'",
        )?;
        let rows = stmt.query_map(rusqlite::params![prefix], |r| {
            Ok(FileRecord {
                id: r.get(0)?,
                path: r.get(1)?,
                language: r.get(2)?,
                hash: r.get(3)?,
                indexed_at: r.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Delete a file record and all its chunks, embeddings, and graph edges.
    pub fn delete_file(&self, file_id: i64, file_path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM embeddings WHERE chunk_id IN (SELECT id FROM chunks WHERE file_id = ?1)",
            rusqlite::params![file_id],
        )?;
        self.conn.execute(
            "DELETE FROM chunks WHERE file_id = ?1",
            rusqlite::params![file_id],
        )?;
        self.conn.execute(
            "DELETE FROM graph_edges WHERE source_file = ?1",
            rusqlite::params![file_path],
        )?;
        self.conn.execute(
            "DELETE FROM files WHERE id = ?1",
            rusqlite::params![file_id],
        )?;
        Ok(())
    }
}
