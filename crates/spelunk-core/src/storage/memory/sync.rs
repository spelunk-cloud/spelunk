//! Sync support for the local memory store.
//!
//! Identity model. Two stable identifiers bridge the local and cloud stores:
//!
//! * `uuid` — the entry's local identity (a fresh UUIDv7, Founder decision §3).
//!   Pushed to the cloud as `external_id`; the cloud-api batch endpoint dedupes
//!   on it, so re-pushing the same entry is idempotent (skipped server-side).
//! * `remote_id` — the cloud-minted entry id. cloud-api mints its own UUIDv7
//!   `id`, independent of our `external_id`. We record it on push (from the 207
//!   batch result) and on pull. **Pull dedupes on `remote_id`**, so an entry
//!   that originated locally is never re-inserted when it returns on the `since`
//!   feed.
//!
//! Every method here keys on these stable ids, never the machine-local
//! autoincrement `id`, which is what makes a re-run of `sync` a no-op.

use anyhow::Result;
use rusqlite::OptionalExtension;
use uuid::Uuid;

use super::MemoryStore;

/// A local note prepared for push to the cloud.
///
/// Carries the stable `uuid` (used as the cloud `external_id` / idempotency key)
/// and **text only** — no embedding vector. The server backfills the embedding
/// with its configured model (embedding-model conformance); shipping a local
/// vector would reintroduce the embedding-space mismatch that conformance removed.
#[derive(Debug, Clone)]
pub struct SyncRow {
    /// Local autoincrement id (for recording the cloud id after a push).
    pub local_id: i64,
    /// Stable local identity (UUIDv7) → cloud `external_id`.
    pub uuid: String,
    /// Cloud-minted id, once known (set after a prior push/pull).
    pub remote_id: Option<String>,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub source_ref: Option<String>,
    /// Whether this entry is archived/tombstoned locally (drives cloud DELETE).
    pub archived: bool,
}

/// A local `relates_to` edge ready to propagate to the cloud.
///
/// Both endpoints are already synced (each carries a cloud `remote_id`), so the
/// server knows them by their `external_id` (the entry's stable `uuid`), the
/// only id the batch edge route resolves. The local row ids are kept so the
/// caller can push only edges touching entries synced in the current round.
#[derive(Debug, Clone)]
pub struct SyncEdge {
    pub from_local_id: i64,
    pub to_local_id: i64,
    pub from_external_id: String,
    pub to_external_id: String,
}

impl MemoryStore {
    /// Assign a fresh UUIDv7 to `note_id` if it lacks one; return the entry's
    /// UUID. Idempotent. This is the Founder-decided backfill (§3) — a *fresh*
    /// UUIDv7 minted on first sync, not a content-derived UUIDv5 — so identity is
    /// uniform with cloud-api's UUIDv7 default.
    pub fn ensure_uuid(&self, note_id: i64) -> Result<String> {
        if let Some(existing) = self.uuid_for(note_id)? {
            return Ok(existing);
        }
        let uuid = Uuid::now_v7().to_string();
        self.conn.execute(
            "UPDATE notes SET uuid = ?1 WHERE id = ?2 AND uuid IS NULL",
            rusqlite::params![uuid, note_id],
        )?;
        Ok(self.uuid_for(note_id)?.unwrap_or(uuid))
    }

    /// Return the UUID for a local note id, if assigned.
    pub fn uuid_for(&self, note_id: i64) -> Result<Option<String>> {
        let uuid: Option<String> = self
            .conn
            .query_row(
                "SELECT uuid FROM notes WHERE id = ?1",
                rusqlite::params![note_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(uuid)
    }

    /// Local note id carrying the given stable `uuid` (the local identity
    /// pushed as the cloud `external_id`), if any. The forward counterpart to
    /// [`Self::uuid_for`]; used to apply a relayed push-ack (`external_id` ->
    /// `remote_id`) back onto the originating row.
    pub fn note_id_for_uuid(&self, uuid: &str) -> Result<Option<i64>> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM notes WHERE uuid = ?1",
                rusqlite::params![uuid],
                |r| r.get(0),
            )
            .optional()?;
        Ok(id)
    }

    /// Record the cloud-minted id for a local note (after a successful push or
    /// when first seen on pull). Idempotent.
    pub fn set_remote_id(&self, note_id: i64, remote_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE notes SET remote_id = ?1 WHERE id = ?2 AND remote_id IS NULL",
            rusqlite::params![remote_id, note_id],
        )?;
        Ok(())
    }

    /// Whether any local note already carries this cloud `remote_id`.
    pub fn has_remote_id(&self, remote_id: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM notes WHERE remote_id = ?1",
            rusqlite::params![remote_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Collect local notes that are candidates to push, backfilling a fresh
    /// UUIDv7 on any that lack one. Returns text-only rows (no vectors) ordered
    /// oldest-first.
    ///
    /// Per decision #183 the caller pushes only the rows `WHERE remote_id IS
    /// NULL` (live entries not yet on the cloud), and tombstones archived rows
    /// that *do* carry a `remote_id`. Both subsets are returned here; the caller
    /// (`push_local`) partitions on `remote_id`/`archived`.
    ///
    /// `include_archived` mirrors the caller's flag; archived rows are still
    /// returned (as tombstones) when requested so deletes propagate.
    pub fn rows_for_sync(&self, include_archived: bool) -> Result<Vec<SyncRow>> {
        let status_clause = if include_archived {
            ""
        } else {
            "WHERE status = 'active'"
        };
        // Read candidate ids first (immutable borrow), then mint UUIDs (mutating),
        // then build rows — avoids holding a statement across the UUID UPDATE.
        let ids: Vec<i64> = {
            let sql =
                format!("SELECT id FROM notes {status_clause} ORDER BY created_at ASC, id ASC");
            let mut stmt = self.conn.prepare(&sql)?;
            stmt.query_map([], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            self.ensure_uuid(id)?;
            if let Some(row) = self.sync_row(id)? {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// Build a [`SyncRow`] for a single note id (UUID must already be assigned).
    fn sync_row(&self, note_id: i64) -> Result<Option<SyncRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, uuid, remote_id, kind, title, body, source_ref, status \
                 FROM notes WHERE id = ?1",
                rusqlite::params![note_id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?;

        let Some((local_id, uuid, remote_id, kind, title, body, source_ref, status)) = row else {
            return Ok(None);
        };
        let Some(uuid) = uuid else { return Ok(None) };

        Ok(Some(SyncRow {
            local_id,
            uuid,
            remote_id,
            kind,
            title,
            body,
            source_ref,
            archived: status == "archived",
        }))
    }

    /// Idempotently apply a note pulled from the cloud, keyed by `remote_id`
    /// (the cloud-minted id).
    ///
    /// - If a local note already carries this `remote_id` (we pushed it, or we
    ///   pulled it before), reconcile lifecycle only: a cloud tombstone archives
    ///   the local copy. Content is append-only and never mutated.
    /// - Otherwise the entry is inserted with `entity_id` populated, via the
    ///   same insert-then-recover path `add_note`/`add_note_superseding` use
    ///   ([`Self::recover_from_entity_id_collision`]). A collision with an
    ///   existing row reuses that row instead of erroring: the pulled
    ///   `remote_id` is adopted onto it (`set_remote_id`'s own `WHERE
    ///   remote_id IS NULL` guard is a no-op when the row already carries a
    ///   different one), and an archived pull archives it, never un-archiving
    ///   an already-archived row.
    ///
    /// Returns `true` only when a genuinely new row was inserted; `false`
    /// covers both "already known via `remote_id`" and "reused via an
    /// `entity_id` collision". Re-running with the same input is a no-op:
    /// the source of `sync`'s idempotency.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_remote_note(
        &self,
        remote_id: &str,
        kind: &str,
        title: &str,
        body: &str,
        source_ref: Option<&str>,
        created_at: i64,
        archived: bool,
    ) -> Result<bool> {
        if let Some(existing_id) = self.note_id_for_remote_id(remote_id)? {
            if archived {
                // Never un-archive (Add-Wins keeps the archive).
                self.archive(existing_id)?;
            }
            return Ok(false);
        }

        self.conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<bool> {
            let status = if archived { "archived" } else { "active" };
            // A pulled entry gets a fresh local `uuid` too, so a later push of
            // this store still has a stable external_id for it.
            let uuid = Uuid::now_v7().to_string();
            let entity_id = crate::storage::entity_id::entity_id(kind, title, body);
            let insert_result = self.conn.execute(
                "INSERT INTO notes \
                 (uuid, remote_id, kind, title, body, source_ref, status, created_at, entity_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    uuid, remote_id, kind, title, body, source_ref, status, created_at, entity_id
                ],
            );
            let (row_id, created) =
                self.recover_from_entity_id_collision(insert_result, &entity_id, &[], &[])?;
            if !created {
                self.set_remote_id(row_id, remote_id)?;
                if archived {
                    self.archive(row_id)?;
                }
            }
            Ok(created)
        })();
        match result {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                self.conn.execute_batch("ROLLBACK").ok();
                Err(e)
            }
        }
    }

    /// Local note id carrying the given cloud `remote_id`, if any.
    pub fn note_id_for_remote_id(&self, remote_id: &str) -> Result<Option<i64>> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM notes WHERE remote_id = ?1",
                rusqlite::params![remote_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(id)
    }

    /// The pull cursor: the max cloud `remote_id` already synced locally
    /// (decision #183).
    ///
    /// `remote_id` holds the cloud-minted UUIDv7 `id`. Because UUIDv7 strings
    /// sort lexically the same as their byte/time order, `MAX(remote_id)` is the
    /// newest cloud id we have, and the cloud `/memory/since?since_id=<this>`
    /// returns everything strictly after it. This replaces the old timestamp
    /// watermark, which was frail under local↔remote clock drift — the cursor is
    /// now derived from synced rows, not wall-clock time, and needs no separate
    /// `sync_state` cache table. Returns `None` when nothing has been synced yet
    /// (a full catch-up).
    pub fn max_remote_id(&self) -> Result<Option<String>> {
        let cursor: Option<String> = self
            .conn
            .query_row("SELECT MAX(remote_id) FROM notes", [], |r| r.get(0))
            .optional()?
            .flatten();
        Ok(cursor)
    }

    /// Every `relates_to` edge whose BOTH endpoints are already synced to the
    /// cloud (each has a `remote_id`) and carry a stable `uuid`.
    ///
    /// Only `relates_to` is enumerated: a `supersedes` edge already travels
    /// with its entry's lifecycle on push, and `contradicts` is server-derived
    /// and never pushed up. An edge with an unsynced endpoint is omitted here,
    /// so it is skipped until a later sync lands that endpoint. The row ids are
    /// returned so the caller can narrow to edges touching entries synced in
    /// the current round.
    pub fn relates_to_edges_for_sync(&self) -> Result<Vec<SyncEdge>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.from_id, e.to_id, nf.uuid, nt.uuid \
             FROM memory_edges e \
             JOIN notes nf ON nf.id = e.from_id \
             JOIN notes nt ON nt.id = e.to_id \
             WHERE e.kind = 'relates_to' \
               AND nf.remote_id IS NOT NULL AND nt.remote_id IS NOT NULL \
               AND nf.uuid IS NOT NULL AND nt.uuid IS NOT NULL \
             ORDER BY e.created_at, e.from_id, e.to_id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SyncEdge {
                    from_local_id: r.get(0)?,
                    to_local_id: r.get(1)?,
                    from_external_id: r.get(2)?,
                    to_external_id: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Count of active (non-archived) rows still in the push outbox
    /// (`remote_id IS NULL`), for the quiet `spelunk status` "N pending" line.
    ///
    /// A read, not a mutation: unlike [`Self::rows_for_sync`] this never calls
    /// [`Self::ensure_uuid`] and never materializes full rows, so calling it
    /// repeatedly (e.g. every `status` invocation) cannot itself change what a
    /// concurrent `rows_for_sync` sees.
    pub fn pending_sync_count(&self) -> Result<i64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM notes WHERE status = 'active' AND remote_id IS NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(n)
    }
}
