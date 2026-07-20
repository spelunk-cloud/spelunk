//! ADR-068 third amendment: backfill `entity_id` onto existing rows and
//! promote `idx_notes_entity_id` to UNIQUE once it is safe to do so.
//!
//! Split into two independently-safe steps, run unconditionally at
//! `MemoryStore::open` (mirrors the `apply_dim_upgrade_migration` /
//! `schema_int8_embeddings` marker-table idiom already used for the
//! 768->896 embedding dim upgrade in `db.rs` and for
//! `schema_v896_note_embeddings` in this same store):
//!
//! - Step A (`backfill_entity_ids`) populates `entity_id` for any row where
//!   it is still `NULL`. Before the index is ever promoted this can never
//!   fail on a constraint (migration 023's index starts non-unique). But
//!   Step A and Step B both run on **every** `MemoryStore::open`, not just
//!   the first, so on a later open (index already promoted UNIQUE by a
//!   prior Step B) a row that reaches Step A still `entity_id IS NULL` — from
//!   any insert path that has its own identity gap, e.g. a pre-fix
//!   `add_note_superseding` row or the still-open `apply_remote_note` gap
//!   (ADR-068 fifth amendment E3) — can collide with an existing row's
//!   computed `entity_id`. ADR-068's fifth amendment (E2) hardens Step A's
//!   per-row `UPDATE` to catch that collision and skip the row (leaving it
//!   `NULL` for a future `dedupe`-then-retry) rather than hard-failing
//!   `open`.
//! - Step B (`promote_entity_id_unique_index`) checks for duplicate
//!   `entity_id` groups. Zero groups: promote the index to UNIQUE and record
//!   a marker so later opens skip the scan. One or more groups: leave the
//!   non-unique index in place and log an actionable message pointing at
//!   `spelunk memory dedupe`. Neither step ever hard-aborts `open`.

use anyhow::{Context, Result};
use rusqlite::OptionalExtension;

use super::MemoryStore;
use crate::storage::entity_id::entity_id;

/// Marker table recorded once the index has been promoted to UNIQUE, so
/// later opens skip the duplicate-group scan entirely.
const ENTITY_ID_UNIQUE_MARKER: &str = "schema_entity_id_unique";

impl MemoryStore {
    /// Step A: populate `entity_id` for every row where it is currently
    /// `NULL`, computed in Rust (sha256 is unavailable to raw SQL). Idempotent:
    /// an interrupted run just leaves the remaining `NULL` rows for the next
    /// open to pick up, and rows that already carry a value are never
    /// re-selected.
    ///
    /// ADR-068 fifth amendment (E2): once a prior open has promoted
    /// `idx_notes_entity_id` to UNIQUE, this per-row `UPDATE` can collide with
    /// an existing row's `entity_id` (see the module doc comment for how such
    /// a row can exist). On that specific collision, skip the row — leave it
    /// `NULL` and log one actionable warning naming it and pointing at
    /// `spelunk memory dedupe` — rather than propagating the error and
    /// hard-failing `MemoryStore::open`. Any other error from the `UPDATE`
    /// still propagates unchanged.
    pub(super) fn backfill_entity_ids(&self) -> Result<()> {
        let rows: Vec<(i64, String, String, String)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id, kind, title, body FROM notes WHERE entity_id IS NULL")
                .context("preparing entity_id backfill scan")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .context("scanning notes for entity_id backfill")?
                .collect::<rusqlite::Result<_>>()
                .context("reading entity_id backfill rows")?
        };

        for (id, kind, title, body) in rows {
            let eid = entity_id(&kind, &title, &body);
            let result = self.conn.execute(
                "UPDATE notes SET entity_id = ?1 WHERE id = ?2",
                rusqlite::params![eid, id],
            );
            match result {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(err, _))
                    if err.code == rusqlite::ErrorCode::ConstraintViolation
                        && err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
                {
                    tracing::warn!(
                        "note #{id} could not be backfilled with an entity_id: it collides \
                         with an existing row's entity_id under the already-promoted UNIQUE \
                         index; run `spelunk memory dedupe` to collapse them, then re-run \
                         spelunk"
                    );
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("backfilling entity_id for note #{id}"));
                }
            }
        }
        Ok(())
    }

    /// Step B: promote `idx_notes_entity_id` to UNIQUE once zero duplicate
    /// groups remain. Never hard-aborts: a store with duplicates stays fully
    /// functional under the non-unique index until the user runs `spelunk
    /// memory dedupe`.
    pub(super) fn promote_entity_id_unique_index(&self) -> Result<()> {
        let already: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                rusqlite::params![ENTITY_ID_UNIQUE_MARKER],
                |_| Ok(true),
            )
            .optional()
            .context("checking entity_id unique-index marker")?
            .is_some();
        if already {
            return Ok(());
        }

        let dup_groups = self.entity_id_duplicate_group_count()?;
        if dup_groups > 0 {
            tracing::warn!(
                "entity_id has {dup_groups} duplicate group(s); run \
                 `spelunk memory dedupe` to collapse them, then re-run spelunk \
                 to enforce uniqueness"
            );
            return Ok(());
        }

        self.conn
            .execute_batch(&format!(
                "DROP INDEX IF EXISTS idx_notes_entity_id; \
                 CREATE UNIQUE INDEX idx_notes_entity_id \
                     ON notes(entity_id) WHERE entity_id IS NOT NULL; \
                 CREATE TABLE IF NOT EXISTS {ENTITY_ID_UNIQUE_MARKER} \
                     (sentinel INTEGER PRIMARY KEY);"
            ))
            .context("promoting idx_notes_entity_id to UNIQUE")?;
        Ok(())
    }

    /// Count of distinct `entity_id` values shared by more than one row.
    /// Shared by Step B's gate and `spelunk memory dedupe`'s summary.
    pub fn entity_id_duplicate_group_count(&self) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM (
                     SELECT entity_id FROM notes
                     WHERE entity_id IS NOT NULL
                     GROUP BY entity_id HAVING COUNT(*) > 1
                 )",
                [],
                |r| r.get(0),
            )
            .context("counting duplicate entity_id groups")
    }
}

#[cfg(test)]
mod collision_recovery_tests;
#[cfg(test)]
mod migration_tests;
#[cfg(test)]
mod test_support;
