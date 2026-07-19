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
        let (id, _) = store
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
        let (survivor_id, _) = store
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
        let (loser_id, _) = store
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

    // ── Adversarial (QA final review): the public `add_note` API, not the
    // raw SQL layer, hitting the promoted index with duplicate content.
    //
    // The previous test intentionally documents that a raw duplicate INSERT
    // *should* be rejected at the SQL level once the index is promoted —
    // that's the index doing its job. This test is about a level up: does
    // anything in the codebase catch that rejection before it reaches a
    // real caller? Nothing does. `MemoryStore::add_note` (used directly by
    // `spelunk memory add`, the most common write path in the CLI, per
    // CLAUDE.md's own agent-workflow guidance to run it "as you make
    // decisions") performs a bare `INSERT` with no pre-check and no error
    // handling for a UNIQUE-constraint rejection.
    //
    // Once Step B has promoted `idx_notes_entity_id` to UNIQUE — which, per
    // AC5, happens on the very next `MemoryStore::open` of *any* store with
    // zero duplicate groups, the overwhelmingly common steady state for a
    // real project — the very next `spelunk memory add` (or a harvest/reconcile
    // retry) that happens to submit byte-identical `kind`/`title`/`body`
    // content to something already stored no longer succeeds. It hard-fails
    // with a raw, low-level SQLite error surfaced straight to the user
    // ("Error: UNIQUE constraint failed: notes.entity_id"), reproduced
    // directly against a real built binary during this review.
    //
    // This directly contradicts this story's own ADR-068 third-amendment
    // decision text: "Excluding `created_at` from identity is settled... The
    // consequence, recording byte-identical `kind`/`title`/`body` twice now
    // yields one entry, is accepted." "Yields one entry" describes a graceful,
    // idempotent-ish outcome — not an unhandled crash. None of this story's
    // 24 acceptance criteria or five rounds of adversarial testing exercised
    // this interaction: every round was scoped to `dedupe.rs`'s own collapse
    // logic (the DELETE path), never to the ordinary INSERT path colliding
    // with the index `entity_id_migration.rs` newly promotes.
    #[test]
    fn add_note_after_promotion_does_not_hard_crash_on_duplicate_content() {
        let store = open_store();
        let (first_id, first_created) = store
            .add_note(
                "decision",
                "dup entry",
                "same content",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        assert!(
            first_created,
            "criterion 25: a genuinely new row inserts fine"
        );

        // Zero duplicate groups at this point: promotes immediately, exactly
        // as a real `MemoryStore::open` would on this store's very next open.
        store.promote_entity_id_unique_index().unwrap();
        assert!(index_is_unique(&store), "precondition: index is now UNIQUE");

        // The exact scenario the ADR's third amendment says "yields one
        // entry": a second, ordinary `add_note` call for byte-identical
        // kind/title/body content.
        let result = store.add_note(
            "decision",
            "dup entry",
            "same content",
            &[],
            &[],
            None,
            None,
        );
        assert!(
            result.is_ok(),
            "BUG: after Step B promotes idx_notes_entity_id to UNIQUE (the \
             very next open of any store with zero duplicate groups — the \
             common case), a plain `memory add` of byte-identical \
             kind/title/body content hard-fails with a raw UNIQUE constraint \
             SQL error instead of the 'yields one entry' outcome ADR-068's \
             third amendment says is the accepted consequence of excluding \
             created_at from identity. Reproduced live against the built \
             CLI binary: `spelunk memory add` for a second time with \
             identical kind/title/body prints \
             'Error: UNIQUE constraint failed: notes.entity_id' and exits 1. \
             Underlying error: {:?}",
            result.as_ref().err()
        );

        // Criterion 26/30: the reused row's id is returned with created=false.
        let (second_id, second_created) = result.unwrap();
        assert_eq!(
            second_id, first_id,
            "criterion 26: a collision must return the EXISTING row's id"
        );
        assert!(
            !second_created,
            "criterion 30: the bool must be false for a reused row"
        );

        // Only one row exists — no phantom second insert survived underneath
        // the recovery path.
        assert_eq!(
            store.count().unwrap(),
            1,
            "the collision must not leave behind a second row"
        );
    }

    // Criterion 26: tags/linked_files on the call that collides must merge
    // (add-wins) into the existing row rather than being dropped.
    #[test]
    fn add_note_after_promotion_merges_tags_and_linked_files_into_existing_row() {
        let store = open_store();
        let (id, _) = store
            .add_note(
                "decision",
                "dup entry",
                "same content",
                &["alpha"],
                &["a.rs"],
                None,
                None,
            )
            .unwrap();
        store.promote_entity_id_unique_index().unwrap();

        let (reused_id, created) = store
            .add_note(
                "decision",
                "dup entry",
                "same content",
                &["beta"],
                &["b.rs"],
                None,
                None,
            )
            .unwrap();
        assert_eq!(reused_id, id);
        assert!(!created);

        let note = store.get(id).unwrap().expect("row still exists");
        assert_eq!(
            note.tags,
            vec!["alpha".to_string(), "beta".to_string()],
            "tags must union, add-wins, existing tag never dropped"
        );
        assert_eq!(
            note.linked_files,
            vec!["a.rs".to_string(), "b.rs".to_string()],
            "linked_files must union the same way"
        );
    }

    // Criterion 27: the collision path must not touch status or superseded_by
    // on the existing row — mirrors reconcile.rs's own existing-row handling,
    // not dedupe.rs's fuller merge (a different scenario: collapsing two rows
    // that already diverged, not a single fresh insert colliding with one).
    #[test]
    fn add_note_after_promotion_does_not_touch_status_or_superseded_by() {
        let store = open_store();
        let (other_id, _) = store
            .add_note("note", "unrelated", "b", &[], &[], None, None)
            .unwrap();
        let (id, _) = store
            .add_note_with_created_at(
                "decision",
                "dup entry",
                "same content",
                &[],
                &[],
                None,
                "archived",
                100,
            )
            .unwrap();
        store.set_superseded_by(id, other_id).unwrap();
        store.promote_entity_id_unique_index().unwrap();

        let (reused_id, created) = store
            .add_note(
                "decision",
                "dup entry",
                "same content",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        assert_eq!(reused_id, id);
        assert!(!created);

        let note = store.get(id).unwrap().expect("row still exists");
        assert_eq!(
            note.status, "archived",
            "criterion 27: status must be left untouched by the collision path"
        );
        assert_eq!(
            note.superseded_by,
            Some(other_id),
            "criterion 27: superseded_by must be left untouched by the collision path"
        );
    }

    // Criterion 29: before promotion (the common case while duplicate groups
    // still exist), identical content must keep inserting distinct rows —
    // this is the very mechanism dedupe.rs's own fixtures rely on to build
    // duplicate-group scenarios in the first place.
    #[test]
    fn add_note_before_promotion_still_inserts_distinct_rows_for_identical_content() {
        let store = open_store();
        assert!(!index_is_unique(&store), "precondition: not yet promoted");

        let (first_id, first_created) = store
            .add_note(
                "decision",
                "dup entry",
                "same content",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let (second_id, second_created) = store
            .add_note(
                "decision",
                "dup entry",
                "same content",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();

        assert!(first_created);
        assert!(
            second_created,
            "pre-promotion, a second identical insert must still be a fresh row"
        );
        assert_ne!(first_id, second_id);
        assert_eq!(store.count().unwrap(), 2);
    }

    // ── ADR-068 fifth amendment E1: add_note_superseding gains entity_id ────

    // Criterion 1/2: a supersede-created row gets a non-NULL entity_id equal
    // to entity_id(kind, title, body), the INSERT succeeds normally when
    // there's no collision, and the archive-OLD UPDATE runs against the
    // freshly-inserted row.
    #[test]
    fn add_note_superseding_sets_entity_id_and_archives_old_on_fresh_insert() {
        let store = open_store();
        let (old_id, _) = store
            .add_note("decision", "old", "old body", &[], &[], None, None)
            .unwrap();

        let (new_id, created) = store
            .add_note_superseding("decision", "new", "new body", &[], &[], None, old_id)
            .unwrap();
        assert!(created, "criterion 2: a fresh row inserts normally");

        let expected_eid = crate::storage::entity_id::entity_id("decision", "new", "new body");
        let stored_eid: Option<String> = store
            .conn
            .query_row(
                "SELECT entity_id FROM notes WHERE id = ?1",
                rusqlite::params![new_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored_eid,
            Some(expected_eid),
            "criterion 1: entity_id must be entity_id(kind, title, body)"
        );

        let old = store.get(old_id).unwrap().expect("old row still exists");
        assert_eq!(old.status, "archived");
        assert_eq!(
            old.superseded_by,
            Some(new_id),
            "archive-OLD must target the freshly-inserted row"
        );
    }

    // Criterion 3: a collision with an existing row's entity_id (post-
    // promotion) must not error; tags/linked_files merge into the existing
    // row via union_tags_and_files, and archive-OLD targets the EXISTING row.
    #[test]
    fn add_note_superseding_recovers_from_collision_and_archives_old_on_existing_row() {
        let store = open_store();
        let (existing_id, _) = store
            .add_note(
                "decision",
                "dup entry",
                "same content",
                &["alpha"],
                &[],
                None,
                None,
            )
            .unwrap();
        store.promote_entity_id_unique_index().unwrap();
        assert!(index_is_unique(&store), "precondition: index promoted");

        let (old_id, _) = store
            .add_note("decision", "to retire", "b", &[], &[], None, None)
            .unwrap();

        let (returned_id, created) = store
            .add_note_superseding(
                "decision",
                "dup entry",
                "same content",
                &["beta"],
                &[],
                None,
                old_id,
            )
            .unwrap();
        assert!(
            !created,
            "criterion 3: a colliding insert must report created=false"
        );
        assert_eq!(
            returned_id, existing_id,
            "criterion 3: the EXISTING row's id must be returned, not a new one"
        );
        let total_rows: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            total_rows, 2,
            "no new row must have been created by the collision — still just \
             `existing` and (now-archived) `old`"
        );

        let existing = store.get(existing_id).unwrap().unwrap();
        assert_eq!(
            existing.tags,
            vec!["alpha".to_string(), "beta".to_string()],
            "tags must union via union_tags_and_files, add-wins"
        );

        let old = store.get(old_id).unwrap().expect("old row still exists");
        assert_eq!(old.status, "archived");
        assert_eq!(
            old.superseded_by,
            Some(existing_id),
            "criterion 3: archive-OLD must target the EXISTING row, not a new one"
        );
    }

    // Criterion 4: pre-promotion, add_note_superseding must keep creating
    // distinct rows for identical content — mirrors add_note's own criterion
    // 29 for this function.
    #[test]
    fn add_note_superseding_before_promotion_still_inserts_distinct_rows() {
        let store = open_store();
        assert!(!index_is_unique(&store), "precondition: not yet promoted");

        let (old_id, _) = store
            .add_note("decision", "old", "old body", &[], &[], None, None)
            .unwrap();
        let (first_id, first_created) = store
            .add_note_superseding("decision", "dup", "body", &[], &[], None, old_id)
            .unwrap();

        let (old_id2, _) = store
            .add_note("decision", "old2", "old body2", &[], &[], None, None)
            .unwrap();
        let (second_id, second_created) = store
            .add_note_superseding("decision", "dup", "body", &[], &[], None, old_id2)
            .unwrap();

        assert!(first_created);
        assert!(
            second_created,
            "pre-promotion, identical content must still create distinct rows"
        );
        assert_ne!(first_id, second_id);
    }

    // Criterion 5: an error other than the specific notes.entity_id UNIQUE
    // violation must propagate unchanged — exercised here via a
    // `supersedes_id` that doesn't reference any row at all, which the
    // `memory_edges` foreign key rejects. This is a different SQLite error
    // entirely (FOREIGN KEY, not UNIQUE on notes.entity_id), so it must not
    // be swallowed by the collision-recovery path: it must propagate as an
    // error, and the whole transaction must roll back (no orphaned note left
    // behind).
    #[test]
    fn add_note_superseding_other_errors_propagate_and_roll_back() {
        let store = open_store();
        let result = store.add_note_superseding("decision", "new", "body", &[], &[], None, 999_999);
        assert!(
            result.is_err(),
            "criterion 5: a non-collision error must propagate, not be swallowed"
        );
        assert_eq!(
            store.count().unwrap(),
            0,
            "the failed transaction must roll back; no orphaned note left behind"
        );
    }

    // ── ADR-068 fifth amendment E2: Step A hardens against a collision ──────

    // Criterion 7: a NULL-entity_id row whose computed value collides with an
    // existing row's entity_id (only reachable once the index is already
    // UNIQUE from a prior open) is skipped (left NULL), and open succeeds.
    #[test]
    fn backfill_skips_a_row_whose_computed_entity_id_collides_and_open_succeeds() {
        let store = open_store();
        let (existing_id, _) = store
            .add_note(
                "decision",
                "dup entry",
                "same content",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        store.promote_entity_id_unique_index().unwrap();
        assert!(index_is_unique(&store), "precondition: index promoted");

        // Simulate a row left behind by some NULL-entity_id insert path
        // (e.g. a pre-E1 add_note_superseding row): insert directly with
        // entity_id NULL, bypassing add_note's own recovery entirely.
        store
            .conn
            .execute(
                "INSERT INTO notes (kind, title, body) VALUES ('decision', 'dup entry', 'same content')",
                [],
            )
            .unwrap();
        let stray_id = store.conn.last_insert_rowid();

        // Step A must not error even though this row's computed entity_id
        // collides with `existing_id`'s.
        store
            .backfill_entity_ids()
            .expect("criterion 7: Step A must not hard-fail on a collision");

        let stray_eid: Option<String> = store
            .conn
            .query_row(
                "SELECT entity_id FROM notes WHERE id = ?1",
                rusqlite::params![stray_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stray_eid, None,
            "criterion 7: the colliding row must be left NULL, not silently dropped"
        );

        let existing_eid: Option<String> = store
            .conn
            .query_row(
                "SELECT entity_id FROM notes WHERE id = ?1",
                rusqlite::params![existing_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            existing_eid.is_some(),
            "the pre-existing row's own entity_id must be untouched"
        );
    }

    // Criterion 8: a NULL-entity_id row with no collision backfills exactly
    // as today (already covered by `backfill_populates_null_entity_ids`
    // above; this test pins the *mixed* case: one colliding row alongside one
    // clean row in the same Step A pass).
    #[test]
    fn backfill_still_populates_non_colliding_rows_alongside_a_colliding_one() {
        let store = open_store();
        store
            .add_note(
                "decision",
                "dup entry",
                "same content",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        store.promote_entity_id_unique_index().unwrap();

        store
            .conn
            .execute(
                "INSERT INTO notes (kind, title, body) VALUES ('decision', 'dup entry', 'same content')",
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO notes (kind, title, body) VALUES ('note', 'clean row', 'clean body')",
                [],
            )
            .unwrap();
        let clean_id = store.conn.last_insert_rowid();

        store.backfill_entity_ids().unwrap();

        let clean_eid: Option<String> = store
            .conn
            .query_row(
                "SELECT entity_id FROM notes WHERE id = ?1",
                rusqlite::params![clean_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            clean_eid,
            Some(crate::storage::entity_id::entity_id(
                "note",
                "clean row",
                "clean body"
            )),
            "criterion 8: a non-colliding NULL row must still backfill normally"
        );
    }
}
