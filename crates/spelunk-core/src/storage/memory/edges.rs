use anyhow::Result;

use super::{MemoryEdge, MemoryStore};

pub(super) fn row_to_edge(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEdge> {
    Ok(MemoryEdge {
        from_id: row.get(0)?,
        to_id: row.get(1)?,
        kind: row.get(2)?,
        created_at: row.get(3)?,
    })
}

impl MemoryStore {
    /// Insert a note in a transaction that also sets `invalid_at` on the superseded entry.
    /// Returns the new note's id.
    #[allow(clippy::too_many_arguments)]
    pub fn add_note_superseding(
        &self,
        kind: &str,
        title: &str,
        body: &str,
        tags: &[&str],
        linked_files: &[&str],
        valid_at: Option<i64>,
        supersedes_id: i64,
    ) -> Result<i64> {
        self.conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<i64> {
            self.conn.execute(
                "INSERT INTO notes (kind, title, body, tags, linked_files, valid_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    kind,
                    title,
                    body,
                    tags.join(","),
                    linked_files.join(","),
                    valid_at,
                ],
            )?;
            let new_id = self.conn.last_insert_rowid();
            let changed = self.conn.execute(
                "UPDATE notes
                 SET    status = 'archived',
                        superseded_by = ?2,
                        invalid_at = CASE WHEN invalid_at IS NULL THEN unixepoch() ELSE invalid_at END
                 WHERE  id = ?1 AND status = 'active'",
                rusqlite::params![supersedes_id, new_id],
            )?;
            if changed == 0 {
                // OLD is absent or already archived (e.g. a prior --supersedes
                // call already claimed it). Mirrors supersede()'s existing
                // reject-on-stale-OLD contract (ADR-068 E4): bail so the outer
                // match rolls the whole transaction back — the just-inserted
                // new note is never committed and no carrier write happens.
                anyhow::bail!("No active memory entry with id {supersedes_id} (old).");
            }
            self.conn.execute(
                "INSERT OR IGNORE INTO memory_edges (from_id, to_id, kind) VALUES (?1, ?2, 'supersedes')",
                rusqlite::params![new_id, supersedes_id],
            )?;
            Ok(new_id)
        })();
        match result {
            Ok(id) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(id)
            }
            Err(e) => {
                self.conn.execute_batch("ROLLBACK").ok();
                Err(e)
            }
        }
    }

    /// Archive `old_id` and link it to `new_id` as its replacement.
    /// Sets `invalid_at` to now if not already set.
    pub fn supersede(&self, old_id: i64, new_id: i64) -> Result<bool> {
        self.conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<bool> {
            let changed = self.conn.execute(
                "UPDATE notes
                 SET    status = 'archived',
                        superseded_by = ?2,
                        invalid_at = CASE WHEN invalid_at IS NULL THEN unixepoch() ELSE invalid_at END
                 WHERE  id = ?1 AND status = 'active'",
                rusqlite::params![old_id, new_id],
            )?;
            if changed > 0 {
                self.conn.execute(
                    "INSERT OR IGNORE INTO memory_edges (from_id, to_id, kind) VALUES (?1, ?2, 'supersedes')",
                    rusqlite::params![new_id, old_id],
                )?;
            }
            Ok(changed > 0)
        })();
        match result {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Delete every `memory_edges` row touching `note_id` (either endpoint).
    ///
    /// `memory dedupe` is the first place in this codebase that deletes an
    /// existing note row, so it cleans up incident edges explicitly here
    /// rather than relying on `memory_edges`' `ON DELETE CASCADE`.
    pub fn delete_edges_for_note(&self, note_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM memory_edges WHERE from_id = ?1 OR to_id = ?1",
            rusqlite::params![note_id],
        )?;
        Ok(())
    }

    /// Insert a directed edge between two notes.
    /// `kind` must be one of: supersedes, relates_to, contradicts.
    pub fn add_edge(&self, from_id: i64, to_id: i64, kind: &str) -> Result<()> {
        const VALID_KINDS: &[&str] = &["supersedes", "relates_to", "contradicts"];
        if !VALID_KINDS.contains(&kind) {
            anyhow::bail!(
                "invalid edge kind '{kind}'; must be one of: supersedes, relates_to, contradicts"
            );
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO memory_edges (from_id, to_id, kind) VALUES (?1, ?2, ?3)",
            rusqlite::params![from_id, to_id, kind],
        )?;
        Ok(())
    }

    /// Return all outgoing and incoming edges for a note.
    /// Returns `(outgoing, incoming)`.
    pub fn get_edges(&self, id: i64) -> Result<(Vec<MemoryEdge>, Vec<MemoryEdge>)> {
        let mut stmt = self.conn.prepare(
            "SELECT from_id, to_id, kind, created_at FROM memory_edges WHERE from_id = ?1 ORDER BY created_at",
        )?;
        let outgoing = stmt
            .query_map(rusqlite::params![id], row_to_edge)?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt2 = self.conn.prepare(
            "SELECT from_id, to_id, kind, created_at FROM memory_edges WHERE to_id = ?1 ORDER BY created_at",
        )?;
        let incoming = stmt2
            .query_map(rusqlite::params![id], row_to_edge)?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok((outgoing, incoming))
    }
}
