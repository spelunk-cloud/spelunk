use anyhow::Result;
use rusqlite::OptionalExtension;

use super::{MemoryStore, Note};

// ── row mappers ──────────────────────────────────────────────────────────────

pub(super) fn row_to_note(row: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    Ok(Note {
        id: row.get(0)?,
        kind: row.get(1)?,
        title: row.get(2)?,
        body: row.get(3)?,
        tags: split_csv(row.get::<_, Option<String>>(4)?.as_deref()),
        linked_files: split_csv(row.get::<_, Option<String>>(5)?.as_deref()),
        created_at: row.get(6)?,
        status: row.get(7)?,
        superseded_by: row.get(8)?,
        source_ref: row.get(9)?,
        valid_at: row.get(10)?,
        invalid_at: row.get(11)?,
        distance: None,
        score: None,
        source_project: None,
        source_project_path: None,
    })
}

pub(super) fn row_to_note_with_distance(row: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    Ok(Note {
        id: row.get(0)?,
        kind: row.get(1)?,
        title: row.get(2)?,
        body: row.get(3)?,
        tags: split_csv(row.get::<_, Option<String>>(4)?.as_deref()),
        linked_files: split_csv(row.get::<_, Option<String>>(5)?.as_deref()),
        created_at: row.get(6)?,
        status: row.get(7)?,
        superseded_by: row.get(8)?,
        source_ref: row.get(9)?,
        valid_at: row.get(10)?,
        invalid_at: row.get(11)?,
        distance: Some(row.get(12)?),
        score: None,
        source_project: None,
        source_project_path: None,
    })
}

pub(super) fn split_csv(s: Option<&str>) -> Vec<String> {
    match s {
        None | Some("") => vec![],
        Some(s) => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    }
}

// ── MemoryStore note methods ─────────────────────────────────────────────────

impl MemoryStore {
    /// Insert a note and return its id. Does not store an embedding —
    /// call `insert_embedding` afterwards if the embedder is available.
    #[allow(clippy::too_many_arguments)]
    pub fn add_note(
        &self,
        kind: &str,
        title: &str,
        body: &str,
        tags: &[&str],
        linked_files: &[&str],
        source_ref: Option<&str>,
        valid_at: Option<i64>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO notes (kind, title, body, tags, linked_files, source_ref, valid_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                kind,
                title,
                body,
                tags.join(","),
                linked_files.join(","),
                source_ref,
                valid_at,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Insert a note with an explicit `created_at` timestamp (unix epoch seconds).
    ///
    /// Used by `memory reconcile` to preserve the original creation timestamp
    /// from the source store. All other callers should use `add_note`, which
    /// defers to the SQLite `DEFAULT (unixepoch())`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_note_with_created_at(
        &self,
        kind: &str,
        title: &str,
        body: &str,
        tags: &[&str],
        linked_files: &[&str],
        source_ref: Option<&str>,
        status: &str,
        created_at: i64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO notes \
             (kind, title, body, tags, linked_files, source_ref, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                kind,
                title,
                body,
                tags.join(","),
                linked_files.join(","),
                source_ref,
                status,
                created_at,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Return all notes for a project ordered by created_at ASC (used by reconcile
    /// to compute the memory.db content-hash set).
    ///
    /// Returns all notes regardless of status so that archived entries also
    /// participate in dedup (we must not re-import a note that was already
    /// imported and then archived in memory.db).
    pub fn all_notes_for_dedup(&self) -> Result<Vec<Note>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, title, body, tags, linked_files, created_at, status, \
             superseded_by, source_ref, valid_at, invalid_at \
             FROM notes ORDER BY created_at ASC",
        )?;
        let notes = stmt
            .query_map([], super::notes::row_to_note)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(notes)
    }

    /// Update an existing note's `superseded_by` link (used by reconcile to
    /// resolve supersede chains after batch import).
    pub fn set_superseded_by(&self, note_id: i64, successor_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE notes SET superseded_by = ?1 WHERE id = ?2",
            rusqlite::params![successor_id, note_id],
        )?;
        Ok(())
    }

    pub fn insert_embedding(&self, note_id: i64, blob: &[u8]) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO note_embeddings (note_id, embedding) VALUES (?1, ?2)",
            rusqlite::params![note_id, blob],
        )?;
        Ok(())
    }

    /// List notes, optionally filtered by kind, newest first.
    /// When `include_archived` is false only active entries are returned.
    pub fn list(
        &self,
        kind_filter: Option<&str>,
        limit: usize,
        include_archived: bool,
    ) -> Result<Vec<Note>> {
        self.list_filtered(kind_filter, None, limit, include_archived, None)
    }

    /// List notes with optional kind, source_ref (prefix), and as_of filters.
    /// When `as_of` is `Some(ts)`, only entries valid at that Unix timestamp are returned.
    pub fn list_filtered(
        &self,
        kind_filter: Option<&str>,
        source_ref_prefix: Option<&str>,
        limit: usize,
        include_archived: bool,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        let limit = limit.min(500);
        let status_clause = if include_archived {
            ""
        } else {
            "AND status = 'active'"
        };

        // Safety: only string literals and bind-param placeholders are appended to
        // `conditions`; all user-supplied values (kind, source_ref, as_of) are bound
        // via rusqlite params![...], never interpolated into the query string.
        let mut conditions = format!("WHERE 1=1 {status_clause}");
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![];

        if let Some(kind) = kind_filter {
            conditions.push_str(&format!(" AND kind = ?{}", params.len() + 1));
            params.push(Box::new(kind.to_string()));
        }
        if let Some(prefix) = source_ref_prefix {
            conditions.push_str(&format!(" AND source_ref LIKE ?{}", params.len() + 1));
            params.push(Box::new(format!("{prefix}%")));
        }
        if let Some(ts) = as_of {
            conditions.push_str(&format!(
                " AND (valid_at IS NULL OR valid_at <= ?{p}) AND (invalid_at IS NULL OR invalid_at > ?{p})",
                p = params.len() + 1
            ));
            params.push(Box::new(ts));
        }

        let sql = format!(
            "SELECT id, kind, title, body, tags, linked_files, created_at, status, superseded_by, source_ref, valid_at, invalid_at
             FROM notes {conditions} ORDER BY created_at DESC LIMIT {limit}"
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let notes = stmt
            .query_map(params_refs.as_slice(), row_to_note)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(notes)
    }

    /// Mark an entry as archived (hidden from search and ask context).
    pub fn archive(&self, id: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE notes SET status = 'archived' WHERE id = ?1 AND status = 'active'",
            rusqlite::params![id],
        )?;
        Ok(changed > 0)
    }

    /// Retrieve the raw embedding blob for a note (for use by `memory push`).
    pub fn get_embedding(&self, note_id: i64) -> Result<Option<Vec<u8>>> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT embedding FROM note_embeddings WHERE note_id = ?1",
                rusqlite::params![note_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(blob)
    }

    pub fn count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM notes WHERE status = 'active'",
            [],
            |r| r.get(0),
        )?)
    }

    /// Return all SHAs stored in source_ref (used by harvest to avoid duplicates).
    /// Also includes SHAs stored as "git:<sha>" tags for backwards compatibility.
    pub fn harvested_shas(&self) -> Result<std::collections::HashSet<String>> {
        let mut shas = std::collections::HashSet::new();

        // Primary: source_ref column (new provenance field).
        let mut stmt = self
            .conn
            .prepare_cached("SELECT source_ref FROM notes WHERE source_ref IS NOT NULL")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            shas.insert(row?);
        }

        // Backwards compat: legacy "git:<sha>" tags written by older versions.
        let mut stmt2 = self
            .conn
            .prepare_cached("SELECT tags FROM notes WHERE tags LIKE '%git:%'")?;
        let rows2 = stmt2.query_map([], |r| r.get::<_, Option<String>>(0))?;
        for row in rows2 {
            if let Some(tags) = row? {
                for tag in tags.split(',').map(str::trim) {
                    if let Some(sha) = tag.strip_prefix("git:") {
                        shas.insert(sha.to_string());
                    }
                }
            }
        }

        Ok(shas)
    }

    /// Check whether any memory entry already has the given source_ref (exact match).
    /// Used by harvest for idempotency before inserting.
    pub fn has_source_ref(&self, sha: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM notes WHERE source_ref = ?1 LIMIT 1",
            rusqlite::params![sha],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn get(&self, id: i64) -> Result<Option<Note>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, title, body, tags, linked_files, created_at, status, superseded_by, source_ref, valid_at, invalid_at
             FROM notes WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![id], row_to_note)?;
        Ok(rows.next().transpose()?)
    }
}
