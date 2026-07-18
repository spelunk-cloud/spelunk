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
//!   it is still `NULL`. It can never fail on a constraint: migration 023's
//!   index stays non-unique until Step B promotes it.
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
            self.conn
                .execute(
                    "UPDATE notes SET entity_id = ?1 WHERE id = ?2",
                    rusqlite::params![eid, id],
                )
                .with_context(|| format!("backfilling entity_id for note #{id}"))?;
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
mod tests {
    use super::ENTITY_ID_UNIQUE_MARKER;
    use crate::storage::memory::MemoryStore;
    use std::sync::OnceLock;

    fn register_sqlite_vec() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )));
            }
        });
    }

    /// A store built via the schema-only path: `migrate()` creates the
    /// (non-unique) `idx_notes_entity_id` but skips the Step A/B pipeline a
    /// real `MemoryStore::open` runs. Each test then drives Step A/B itself,
    /// so a fresh empty store isn't auto-promoted to UNIQUE before the test
    /// gets a chance to seed the rows it actually wants to exercise.
    fn open_store() -> MemoryStore {
        register_sqlite_vec();
        let conn = rusqlite::Connection::open(std::path::Path::new(":memory:"))
            .expect("open in-memory sqlite");
        let store = MemoryStore { conn };
        store.migrate().expect("schema migration");
        store
    }

    fn marker_exists(store: &MemoryStore) -> bool {
        store
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                rusqlite::params![ENTITY_ID_UNIQUE_MARKER],
                |_| Ok(true),
            )
            .optional()
            .unwrap()
            .is_some()
    }

    fn index_is_unique(store: &MemoryStore) -> bool {
        let sql: String = store
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_notes_entity_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        sql.to_uppercase().contains("UNIQUE")
    }

    fn null_entity_id_count(store: &MemoryStore) -> i64 {
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE entity_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }

    use rusqlite::OptionalExtension;

    // ── AC1: a store with rows lacking entity_id gets them populated ────────
    #[test]
    fn backfill_populates_null_entity_ids() {
        let store = open_store();
        // Insert directly, bypassing add_note (which already stamps entity_id),
        // to simulate a pre-023 row that predates the column.
        store
            .conn
            .execute(
                "INSERT INTO notes (kind, title, body, entity_id) VALUES ('note', 't', 'b', NULL)",
                [],
            )
            .unwrap();
        assert_eq!(null_entity_id_count(&store), 1);

        store.backfill_entity_ids().unwrap();

        assert_eq!(null_entity_id_count(&store), 0);
        let stamped: String = store
            .conn
            .query_row("SELECT entity_id FROM notes WHERE title='t'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            stamped,
            crate::storage::entity_id::entity_id("note", "t", "b")
        );
    }

    // ── AC2: a fully-populated store is a no-op ─────────────────────────────
    #[test]
    fn backfill_on_fully_populated_store_is_noop() {
        let store = open_store();
        store
            .add_note("note", "t", "b", &[], &[], None, None)
            .unwrap();
        assert_eq!(null_entity_id_count(&store), 0);
        // Must not error and must not disturb the already-correct value.
        store.backfill_entity_ids().unwrap();
        assert_eq!(null_entity_id_count(&store), 0);
    }

    // ── AC3: rows inserted post-023 already carrying entity_id are untouched ─
    #[test]
    fn backfill_leaves_already_stamped_rows_untouched() {
        let store = open_store();
        let id = store
            .add_note("decision", "already stamped", "body", &[], &[], None, None)
            .unwrap();
        let before: String = store
            .conn
            .query_row(
                "SELECT entity_id FROM notes WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        store.backfill_entity_ids().unwrap();
        let after: String = store
            .conn
            .query_row(
                "SELECT entity_id FROM notes WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, after);
    }

    // ── AC4: an interrupted population resumes cleanly with no duplication ──
    #[test]
    fn backfill_resumes_after_partial_population() {
        let store = open_store();
        store
            .conn
            .execute_batch(
                "INSERT INTO notes (kind, title, body, entity_id) VALUES ('note', 'a', 'a-body', NULL);
                 INSERT INTO notes (kind, title, body, entity_id) VALUES ('note', 'b', 'b-body', NULL);",
            )
            .unwrap();
        assert_eq!(null_entity_id_count(&store), 2);

        // First pass populates both.
        store.backfill_entity_ids().unwrap();
        assert_eq!(null_entity_id_count(&store), 0);

        // Simulate a fresh row landing NULL again (as if a third row arrived
        // between an interrupted run and the next open) and rerun: only the
        // new row is touched, no duplication of existing rows.
        store
            .conn
            .execute(
                "INSERT INTO notes (kind, title, body, entity_id) VALUES ('note', 'c', 'c-body', NULL)",
                [],
            )
            .unwrap();
        store.backfill_entity_ids().unwrap();
        assert_eq!(null_entity_id_count(&store), 0);

        let total: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 3, "no row must be duplicated across the two passes");
    }

    // ── AC5: zero duplicate groups promotes the index and records the marker ─
    #[test]
    fn promote_with_zero_duplicates_creates_unique_index_and_marker() {
        let store = open_store();
        store
            .add_note("note", "unique one", "body", &[], &[], None, None)
            .unwrap();
        store
            .add_note("note", "unique two", "body two", &[], &[], None, None)
            .unwrap();

        assert!(!marker_exists(&store), "marker must not pre-exist");
        store.promote_entity_id_unique_index().unwrap();

        assert!(marker_exists(&store), "marker must be recorded");
        assert!(index_is_unique(&store), "index must be promoted to UNIQUE");
    }

    // ── AC6: one or more duplicate groups leaves the index non-unique, no error ─
    #[test]
    fn promote_with_duplicates_leaves_index_non_unique_and_does_not_error() {
        let store = open_store();
        // Two rows sharing kind/title/body (the entity_id key) but different
        // created_at, simulating the exact real-data shape from the task brief.
        store
            .add_note_with_created_at(
                "requirement",
                "Exit codes follow protocol",
                "body",
                &[],
                &[],
                None,
                "active",
                1_700_000_000,
            )
            .unwrap();
        store
            .add_note_with_created_at(
                "requirement",
                "Exit codes follow protocol",
                "body",
                &[],
                &[],
                None,
                "active",
                1_700_000_001,
            )
            .unwrap();

        let result = store.promote_entity_id_unique_index();
        assert!(result.is_ok(), "duplicates must never abort the open");
        assert!(
            !marker_exists(&store),
            "marker must not be recorded while duplicates remain"
        );
        assert!(
            !index_is_unique(&store),
            "index must stay non-unique while duplicates remain"
        );
    }

    // ── AC7: once promoted, later opens skip the duplicate scan ─────────────
    #[test]
    fn promote_skips_rescan_once_marker_present() {
        let store = open_store();
        store
            .add_note("note", "solo", "body", &[], &[], None, None)
            .unwrap();
        store.promote_entity_id_unique_index().unwrap();
        assert!(marker_exists(&store));

        // Drop the notes table entirely: if the marker check did not
        // short-circuit before the duplicate-group scan, the scan's query
        // against a now-missing table would surface as an error.
        store.conn.execute_batch("DROP TABLE notes;").unwrap();

        let result = store.promote_entity_id_unique_index();
        assert!(
            result.is_ok(),
            "marker must short-circuit before any duplicate scan runs: {:?}",
            result.err()
        );
    }

    // ── AC8: duplicates dedupe'd to zero, then reopened: promotes next open ─
    #[test]
    fn promote_after_manual_collapse_to_zero_duplicates_succeeds() {
        let store = open_store();
        let survivor_id = store
            .add_note_with_created_at(
                "note",
                "dup title",
                "dup body",
                &[],
                &[],
                None,
                "active",
                1_700_000_000,
            )
            .unwrap();
        let loser_id = store
            .add_note_with_created_at(
                "note",
                "dup title",
                "dup body",
                &[],
                &[],
                None,
                "active",
                1_700_000_050,
            )
            .unwrap();

        // First open-equivalent: duplicates present, index stays non-unique.
        store.promote_entity_id_unique_index().unwrap();
        assert!(!index_is_unique(&store));

        // Collapse manually (mirrors what `spelunk memory dedupe` would do).
        store
            .conn
            .execute(
                "DELETE FROM notes WHERE id = ?1",
                rusqlite::params![loser_id],
            )
            .unwrap();
        assert!(survivor_id > 0);

        // "Reopen": call promote again now duplicates are gone.
        store.promote_entity_id_unique_index().unwrap();
        assert!(
            index_is_unique(&store),
            "index must promote once duplicates are collapsed to zero"
        );
        assert!(marker_exists(&store));
    }

    // ── Adversarial (AC5/item 8): the promoted index must genuinely reject a
    // duplicate INSERT at the SQL level, not merely report itself as "UNIQUE"
    // in `sqlite_master`'s stored SQL text (index_is_unique() above proves
    // only the latter). A CREATE UNIQUE INDEX statement can be issued against
    // a table that already violates it in edge cases (e.g. NULLs, or a
    // logic bug in the WHERE-partial-index clause); the only real proof is a
    // rejected write. ────────────────────────────────────────────────────────
    #[test]
    fn promoted_unique_index_genuinely_rejects_a_duplicate_insert() {
        let store = open_store();
        store
            .add_note("note", "only one", "body", &[], &[], None, None)
            .unwrap();

        store.promote_entity_id_unique_index().unwrap();
        assert!(index_is_unique(&store), "precondition: index promoted");

        let dup_entity_id = crate::storage::entity_id::entity_id("note", "only one", "body");
        let insert_result = store.conn.execute(
            "INSERT INTO notes (kind, title, body, entity_id) VALUES ('note', 'only one', 'body', ?1)",
            rusqlite::params![dup_entity_id],
        );
        assert!(
            insert_result.is_err(),
            "a promoted UNIQUE index must reject a subsequent duplicate \
             entity_id insert, not merely exist with 'UNIQUE' in its SQL text"
        );
        let msg = insert_result.unwrap_err().to_string().to_lowercase();
        assert!(
            msg.contains("unique"),
            "expected a UNIQUE constraint violation, got: {msg}"
        );

        // A non-duplicate insert must still succeed: the index is selective
        // (WHERE entity_id IS NOT NULL), not a blanket rejection of writes.
        let other_entity_id =
            crate::storage::entity_id::entity_id("note", "a different one", "body");
        store
            .conn
            .execute(
                "INSERT INTO notes (kind, title, body, entity_id) VALUES ('note', 'a different one', 'body', ?1)",
                rusqlite::params![other_entity_id],
            )
            .expect("a genuinely distinct entity_id must still insert fine");
    }
}
