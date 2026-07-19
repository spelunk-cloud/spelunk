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
        // Not selected by the row-mapper queries; DB→Note callers don't need it.
        remote_id: None,
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
        remote_id: None,
    })
}

/// Append the members of `incoming` that `current` lacks, preserving `current`'s
/// order. Returns `None` only when the result is empty and `current` was NULL,
/// so a row with no tags is not rewritten to `""`.
fn union_csv(current: Option<&str>, incoming: &[String]) -> Option<String> {
    let mut merged = split_csv(current);
    for v in incoming {
        let v = v.trim();
        if !v.is_empty() && !merged.iter().any(|e| e == v) {
            merged.push(v.to_string());
        }
    }
    match (merged.is_empty(), current) {
        (true, None) => None,
        _ => Some(merged.join(",")),
    }
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
    /// Insert a note and return `(id, created)`. `created` is `true` when a
    /// genuinely new row was inserted, `false` when the insert collided with
    /// an existing row's `entity_id` (only possible once `idx_notes_entity_id`
    /// has been promoted to UNIQUE — see `entity_id_migration.rs`) and that
    /// existing row was reused instead. Does not store an embedding on a fresh
    /// insert — call `insert_embedding` afterwards if the embedder is available.
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
    ) -> Result<(i64, bool)> {
        let entity_id = crate::storage::entity_id::entity_id(kind, title, body);
        let result = self.conn.execute(
            "INSERT INTO notes \
             (kind, title, body, tags, linked_files, source_ref, valid_at, entity_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                kind,
                title,
                body,
                tags.join(","),
                linked_files.join(","),
                source_ref,
                valid_at,
                entity_id,
            ],
        );
        self.recover_from_entity_id_collision(result, &entity_id, tags, linked_files)
    }

    /// Insert a note with an explicit `created_at` timestamp (unix epoch seconds).
    ///
    /// Used by `memory reconcile` to preserve the original creation timestamp
    /// from the source store. All other callers should use `add_note`, which
    /// defers to the SQLite `DEFAULT (unixepoch())`.
    ///
    /// Returns `(id, created)` — see `add_note` for what `created` means.
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
    ) -> Result<(i64, bool)> {
        let entity_id = crate::storage::entity_id::entity_id(kind, title, body);
        let result = self.conn.execute(
            "INSERT INTO notes \
             (kind, title, body, tags, linked_files, source_ref, status, created_at, entity_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                kind,
                title,
                body,
                tags.join(","),
                linked_files.join(","),
                source_ref,
                status,
                created_at,
                entity_id,
            ],
        );
        self.recover_from_entity_id_collision(result, &entity_id, tags, linked_files)
    }

    /// Shared insert-then-recover tail for `add_note`/`add_note_with_created_at`.
    ///
    /// `insert_result` is the raw outcome of an `INSERT` that populated
    /// `entity_id`. On success, returns `(new_id, true)` unchanged.
    ///
    /// On a UNIQUE-constraint failure, this can only be `idx_notes_entity_id`:
    /// neither function's INSERT populates `uuid` or `remote_id` (both left
    /// `NULL`, and both columns' own UNIQUE indexes are partial `WHERE ... IS
    /// NOT NULL`, so a `NULL` never collides). So any UNIQUE violation from
    /// this specific INSERT is the promoted entity_id index rejecting
    /// byte-identical `kind`/`title`/`body` content (ADR-068's third
    /// amendment: "recording byte-identical kind/title/body twice now yields
    /// one entry"). Recover by looking up the existing row, merging `tags`
    /// and `linked_files` into it (add-wins, via the existing
    /// `union_tags_and_files` — reused, not reimplemented), and returning its
    /// id with `created = false`. Does **not** touch `status` or
    /// `superseded_by` on this path — mirrors `reconcile.rs`'s own handling
    /// of an existing-row collision, not `dedupe.rs`'s fuller merge (which
    /// collapses two rows that already diverged in the store, a different
    /// scenario from a single fresh insert colliding with one existing row).
    ///
    /// Before `idx_notes_entity_id` is promoted (the common case while
    /// duplicate groups still exist), no UNIQUE violation is possible on
    /// `entity_id` at all, so this path is never taken and behavior is
    /// unchanged from a bare INSERT — this is what lets duplicate-group test
    /// fixtures built against an unpromoted store keep inserting distinct
    /// rows for identical content.
    ///
    /// Any other error (a different constraint, I/O, etc.) propagates unchanged.
    pub(super) fn recover_from_entity_id_collision(
        &self,
        insert_result: rusqlite::Result<usize>,
        entity_id: &str,
        tags: &[&str],
        linked_files: &[&str],
    ) -> Result<(i64, bool)> {
        match insert_result {
            Ok(_) => Ok((self.conn.last_insert_rowid(), true)),
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation
                    && err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                let existing_id: i64 = self.conn.query_row(
                    "SELECT id FROM notes WHERE entity_id = ?1",
                    rusqlite::params![entity_id],
                    |r| r.get(0),
                )?;
                let owned_tags: Vec<String> = tags.iter().map(|s| s.to_string()).collect();
                let owned_files: Vec<String> = linked_files.iter().map(|s| s.to_string()).collect();
                self.union_tags_and_files(existing_id, &owned_tags, &owned_files)?;
                Ok((existing_id, false))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Return all notes for a project ordered by created_at ASC, id ASC (used
    /// by reconcile to compute the memory.db content-hash set, and by
    /// `dedupe_entity_ids` to pick each duplicate group's survivor).
    ///
    /// Returns all notes regardless of status so that archived entries also
    /// participate in dedup (we must not re-import a note that was already
    /// imported and then archived in memory.db).
    ///
    /// The `id ASC` secondary key is a deliberate, explicit tie-break: two
    /// rows can share the exact same `created_at` (e.g. a batch import, or
    /// two ordinary inserts landing in the same unixepoch() second), and
    /// `ORDER BY created_at ASC` alone leaves that tie's resolution order to
    /// SQLite's query plan rather than a documented invariant. `id` is
    /// monotonically increasing with insertion order, so this pins "earliest
    /// created, and among ties, first inserted" as the actual, stable
    /// definition of "earliest" every caller of this function relies on.
    pub fn all_notes_for_dedup(&self) -> Result<Vec<Note>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, title, body, tags, linked_files, created_at, status, \
             superseded_by, source_ref, valid_at, invalid_at \
             FROM notes ORDER BY created_at ASC, id ASC",
        )?;
        let notes = stmt
            .query_map([], super::notes::row_to_note)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(notes)
    }

    /// Merge `tags` and `linked_files` into an existing note, add-wins: values
    /// are only ever added, never removed or reordered.
    ///
    /// Two entries with identical text but different tags share one `entity_id`
    /// and so collapse on import; unioning is what keeps the losing copy's tags
    /// from being dropped. Mirrors the pull-side Add-Wins policy in `sync.rs`.
    ///
    /// Returns `true` when the row changed.
    pub fn union_tags_and_files(
        &self,
        note_id: i64,
        tags: &[String],
        linked_files: &[String],
    ) -> Result<bool> {
        let (cur_tags, cur_files): (Option<String>, Option<String>) = self.conn.query_row(
            "SELECT tags, linked_files FROM notes WHERE id = ?1",
            rusqlite::params![note_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

        let merged_tags = union_csv(cur_tags.as_deref(), tags);
        let merged_files = union_csv(cur_files.as_deref(), linked_files);

        if merged_tags.as_deref() == cur_tags.as_deref()
            && merged_files.as_deref() == cur_files.as_deref()
        {
            return Ok(false);
        }

        self.conn.execute(
            "UPDATE notes SET tags = ?1, linked_files = ?2 WHERE id = ?3",
            rusqlite::params![merged_tags, merged_files, note_id],
        )?;
        Ok(true)
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

    /// Clear an existing note's `superseded_by` link back to NULL. Used by
    /// `memory dedupe`'s self-edge guard: a rewrite that would otherwise point
    /// a row at itself drops the link instead.
    pub fn clear_superseded_by(&self, note_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE notes SET superseded_by = NULL WHERE id = ?1",
            rusqlite::params![note_id],
        )?;
        Ok(())
    }

    /// Ids of every row whose `superseded_by` points at `target_id`.
    pub fn notes_pointing_at(&self, target_id: i64) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM notes WHERE superseded_by = ?1")?;
        let ids = stmt
            .query_map(rusqlite::params![target_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    /// Delete a note row outright. Used only by `memory dedupe` to remove a
    /// duplicate-group loser after its tags/linked_files/superseded_by have
    /// been folded into the survivor and any edges pointing at it rewritten.
    pub fn delete_note(&self, note_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM notes WHERE id = ?1",
            rusqlite::params![note_id],
        )?;
        Ok(())
    }

    /// Delete a note's embedding row, if present. A no-op when absent. Used by
    /// `memory dedupe` when deleting a duplicate-group loser: two vectors have
    /// no meaningful union, so the loser's embedding is dropped rather than
    /// merged (the survivor's own embedding, if any, is untouched).
    pub fn delete_note_embedding(&self, note_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM note_embeddings WHERE note_id = ?1",
            rusqlite::params![note_id],
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
