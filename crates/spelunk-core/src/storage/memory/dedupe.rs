//! Collapse duplicate-`entity_id` groups already resident in `memory.db`.
//!
//! Backs the `spelunk memory dedupe` command. See ADR-068's third amendment
//! for the merge rule this implements: survivor = earliest `created_at`;
//! `tags`/`linked_files` union add-wins; archived sticks; `superseded_by`
//! adoption with an earliest-wins tie-break on conflict (any candidate value
//! that refers to a member of the same duplicate group — the survivor itself
//! or a fellow loser — is treated as absent rather than adopted, since it
//! would otherwise create a self-loop or a reference to a row this same run
//! deletes); every row elsewhere pointing at a loser is rewritten to the
//! survivor (self-edges dropped to NULL instead); losers and their
//! `note_embeddings` row are then deleted. The whole run is one transaction:
//! any error rolls back, leaving `memory.db` exactly as it was.
//!
//! This is deliberately never called from `Database`/`MemoryStore::open` or
//! any other automatic path (`init`, `add`, …): collapsing is destructive,
//! so it only happens when the user explicitly asks for it.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

use super::{MemoryStore, Note};
use crate::storage::entity_id::note_entity_id;

/// Summary of one `dedupe_entity_ids` run (or dry-run estimate).
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct DedupeSummary {
    pub total_notes: usize,
    pub duplicate_groups: usize,
    /// Losers collapsed (rows removed).
    pub rows_collapsed: usize,
    pub tags_merged: usize,
    pub linked_files_merged: usize,
    pub supersede_edges_repointed: usize,
    pub supersede_self_edges_dropped: usize,
}

#[cfg(test)]
thread_local! {
    /// Test-only fault injection: when set to `Some(n)`, the run fails right
    /// after the (0-indexed) n-th group has been fully applied, before COMMIT.
    /// Used to prove the whole-run rollback guarantee under a real multi-group
    /// transaction rather than only the empty/no-op case.
    static FAULT_AFTER_GROUP: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn inject_fault_after_group(n: usize) {
    FAULT_AFTER_GROUP.with(|f| f.set(Some(n)));
}

#[cfg(test)]
fn clear_fault() {
    FAULT_AFTER_GROUP.with(|f| f.set(None));
}

#[cfg(test)]
fn fault_due(i: usize) -> bool {
    FAULT_AFTER_GROUP.with(|f| f.get() == Some(i))
}

#[cfg(not(test))]
fn fault_due(_i: usize) -> bool {
    false
}

#[cfg(test)]
thread_local! {
    /// Test-only fault injection at a finer grain than `FAULT_AFTER_GROUP`:
    /// fires after the (0-indexed) n-th loser *within the current group* has
    /// been fully deleted (embedding + edges + note row), but before any
    /// later loser in the same group is touched. Used to prove the whole-run
    /// rollback guarantee holds mid-group, not only at a group boundary.
    static FAULT_AFTER_LOSER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn inject_fault_after_loser(n: usize) {
    FAULT_AFTER_LOSER.with(|f| f.set(Some(n)));
}

#[cfg(test)]
fn clear_loser_fault() {
    FAULT_AFTER_LOSER.with(|f| f.set(None));
}

#[cfg(test)]
fn loser_fault_due(i: usize) -> bool {
    FAULT_AFTER_LOSER.with(|f| f.get() == Some(i))
}

#[cfg(not(test))]
fn loser_fault_due(_i: usize) -> bool {
    false
}

impl MemoryStore {
    /// Collapse every duplicate `entity_id` group in one all-or-nothing
    /// transaction. `dry_run` computes the same summary via read-only queries
    /// and writes nothing.
    pub fn dedupe_entity_ids(&self, dry_run: bool) -> Result<DedupeSummary> {
        let all = self
            .all_notes_for_dedup()
            .context("reading notes for dedupe")?;
        let total_notes = all.len();

        // Group by entity_id. `all_notes_for_dedup` orders by created_at ASC,
        // so each group's first element is the earliest-created row: the
        // survivor, with no separate sort needed.
        let mut groups: Vec<Vec<Note>> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for n in all {
            let eid = note_entity_id(&n);
            match index.get(&eid) {
                Some(&i) => groups[i].push(n),
                None => {
                    index.insert(eid, groups.len());
                    groups.push(vec![n]);
                }
            }
        }
        let duplicate_groups: Vec<Vec<Note>> = groups.into_iter().filter(|g| g.len() > 1).collect();

        let mut summary = DedupeSummary {
            total_notes,
            duplicate_groups: duplicate_groups.len(),
            ..Default::default()
        };

        if duplicate_groups.is_empty() {
            return Ok(summary);
        }

        if dry_run {
            for group in &duplicate_groups {
                self.collapse_group(group, &mut summary, false)?;
            }
            return Ok(summary);
        }

        self.execute_batch("BEGIN IMMEDIATE")
            .context("beginning dedupe transaction")?;
        let result: Result<()> = (|| {
            for (i, group) in duplicate_groups.iter().enumerate() {
                self.collapse_group(group, &mut summary, true)?;
                if fault_due(i) {
                    anyhow::bail!("injected test fault after group {i}");
                }
            }
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.execute_batch("COMMIT")
                    .context("committing dedupe transaction")?;
                Ok(summary)
            }
            Err(e) => {
                let _ = self.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Plan (and, when `apply`, execute) the collapse of one duplicate group.
    /// `group` is ordered `created_at` ASC; `group[0]` is the survivor.
    ///
    /// Counting and mutation share this one path so a dry-run summary and a
    /// real run always agree: every count here is derived the same way in
    /// both modes, only the trailing writes are skipped when `!apply`.
    fn collapse_group(
        &self,
        group: &[Note],
        summary: &mut DedupeSummary,
        apply: bool,
    ) -> Result<()> {
        let survivor = &group[0];
        let losers = &group[1..];

        // ── tags / linked_files: union, add-wins ────────────────────────────
        let mut new_tags: Vec<String> = Vec::new();
        let mut new_files: Vec<String> = Vec::new();
        for loser in losers {
            for t in &loser.tags {
                if !survivor.tags.contains(t) && !new_tags.contains(t) {
                    new_tags.push(t.clone());
                }
            }
            for f in &loser.linked_files {
                if !survivor.linked_files.contains(f) && !new_files.contains(f) {
                    new_files.push(f.clone());
                }
            }
        }
        summary.tags_merged += new_tags.len();
        summary.linked_files_merged += new_files.len();

        // ── status: archived sticks ──────────────────────────────────────────
        let any_archived = group.iter().any(|n| n.status == "archived");

        // ── superseded_by adoption, earliest-wins on conflict ───────────────
        // A candidate value that refers to a member of *this same* duplicate
        // group (the survivor itself, or a fellow loser) must never be
        // adopted as-is: adopting the survivor's own id is a self-loop, and
        // adopting a fellow loser's id creates a live reference to a row this
        // very transaction is about to delete, which this SQLite build
        // rejects outright with a FOREIGN KEY constraint error since
        // `notes.superseded_by` has no `ON DELETE` clause. This mirrors the
        // self-edge guard the rewrite loop below already applies to
        // *external* dependents: any candidate resolving to a group member is
        // treated as absent (dropped, not chased transitively — the ADR's
        // merge rule for adoption is a flat "first non-null value", and
        // chasing a chain through another group's own in-flight resolution
        // would add order-dependent complexity the spec doesn't ask for), and
        // the search continues to the next (later-created) candidate. Group
        // is created_at ASC, so the first surviving candidate is, by
        // construction, the earliest-created row's genuinely-external value.
        let group_ids: HashSet<i64> = group.iter().map(|n| n.id).collect();
        let external_values: Vec<i64> = group
            .iter()
            .filter_map(|n| n.superseded_by)
            .filter(|v| !group_ids.contains(v))
            .collect();
        let first_non_null = external_values.first().copied();
        if let Some(val) = first_non_null {
            let conflicting = external_values.iter().any(|v| *v != val);
            if conflicting {
                tracing::warn!(
                    "memory dedupe: duplicate-entity_id group for survivor #{} carries \
                     conflicting superseded_by values; the earliest-created row's value \
                     ({val}) wins",
                    survivor.id
                );
            }
        }

        // ── edges elsewhere pointing at a loser: rewrite before deletion ────
        // Read-only lookups, safe to run in both dry-run and real mode.
        let mut rewrites: Vec<(i64, Option<i64>)> = Vec::new();
        for loser in losers {
            for ref_id in self.notes_pointing_at(loser.id)? {
                if ref_id == survivor.id {
                    rewrites.push((ref_id, None));
                } else {
                    rewrites.push((ref_id, Some(survivor.id)));
                }
            }
        }
        for (_, target) in &rewrites {
            match target {
                Some(_) => summary.supersede_edges_repointed += 1,
                None => summary.supersede_self_edges_dropped += 1,
            }
        }

        summary.rows_collapsed += losers.len();

        if !apply {
            return Ok(());
        }

        // ── apply phase (real run only) ─────────────────────────────────────
        if !new_tags.is_empty() || !new_files.is_empty() {
            self.union_tags_and_files(survivor.id, &new_tags, &new_files)?;
        }
        if any_archived {
            self.archive(survivor.id)?;
        }
        if let Some(val) = first_non_null
            && survivor.superseded_by != Some(val)
        {
            self.set_superseded_by(survivor.id, val)?;
        }
        for (ref_id, target) in &rewrites {
            match target {
                Some(new_target) => self.set_superseded_by(*ref_id, *new_target)?,
                None => self.clear_superseded_by(*ref_id)?,
            }
        }
        for (li, loser) in losers.iter().enumerate() {
            self.delete_note_embedding(loser.id)?;
            self.delete_edges_for_note(loser.id)?;
            self.delete_note(loser.id)?;
            if loser_fault_due(li) {
                anyhow::bail!("injected test fault after deleting loser index {li} within group");
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    /// (non-unique) `idx_notes_entity_id` but skips the automatic Step A/B
    /// pipeline a real `MemoryStore::open` runs. `dedupe_entity_ids` itself
    /// doesn't care whether Step A/B ran: it groups by recomputing
    /// `note_entity_id` in Rust regardless of the stored column or index
    /// state, but these tests need to seed duplicate-content rows directly,
    /// which a real `open()` on a fresh (zero-row, zero-duplicate) store
    /// would already have promoted to a UNIQUE index, rejecting the seed.
    fn open_store() -> MemoryStore {
        register_sqlite_vec();
        let conn = rusqlite::Connection::open(std::path::Path::new(":memory:"))
            .expect("open in-memory sqlite");
        let store = MemoryStore { conn };
        store.migrate().expect("schema migration");
        store
    }

    fn note_count(store: &MemoryStore) -> i64 {
        store
            .conn
            .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
            .unwrap()
    }

    fn has_embedding(store: &MemoryStore, note_id: i64) -> bool {
        store.get_embedding(note_id).unwrap().is_some()
    }

    /// Snapshot every column of every row in `table`, ordered by `order_by`,
    /// as generic SQLite `Value`s. Used to assert a rolled-back or dry-run
    /// call left the database byte-for-byte unchanged: unlike a row-count or
    /// single-column check, this catches a regression in *any* column
    /// (tags, superseded_by, status, entity_id, uuid, remote_id, ...) without
    /// having to hand-maintain a column list.
    fn full_table_snapshot(
        store: &MemoryStore,
        table: &str,
        order_by: &str,
    ) -> Vec<Vec<rusqlite::types::Value>> {
        let sql = format!("SELECT * FROM {table} ORDER BY {order_by}");
        let mut stmt = store.conn.prepare(&sql).unwrap();
        let n = stmt.column_count();
        stmt.query_map([], |row| {
            (0..n)
                .map(|i| row.get::<_, rusqlite::types::Value>(i))
                .collect()
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    }

    type TableSnapshot = Vec<Vec<rusqlite::types::Value>>;

    /// Snapshot of `notes` + `memory_edges` + `note_embeddings`, the three
    /// tables `dedupe_entity_ids` can touch.
    fn full_db_snapshot(store: &MemoryStore) -> (TableSnapshot, TableSnapshot, TableSnapshot) {
        (
            full_table_snapshot(store, "notes", "id"),
            full_table_snapshot(store, "memory_edges", "from_id, to_id, kind"),
            full_table_snapshot(store, "note_embeddings", "note_id"),
        )
    }

    // ── AC22 (zero groups): all-zero counts, no writes, dry-run or not ──────
    #[test]
    fn zero_duplicates_reports_all_zero_and_writes_nothing() {
        let store = open_store();
        store
            .add_note("note", "solo", "body", &[], &[], None, None)
            .unwrap();

        for dry in [true, false] {
            let summary = store.dedupe_entity_ids(dry).unwrap();
            assert_eq!(summary.total_notes, 1);
            assert_eq!(summary.duplicate_groups, 0);
            assert_eq!(summary.rows_collapsed, 0);
            assert_eq!(summary.tags_merged, 0);
            assert_eq!(summary.linked_files_merged, 0);
            assert_eq!(summary.supersede_edges_repointed, 0);
            assert_eq!(summary.supersede_self_edges_dropped, 0);
        }
        assert_eq!(note_count(&store), 1, "no row must be touched");
    }

    // ── AC9: --dry-run reports counts, makes no writes ──────────────────────
    #[test]
    fn dry_run_reports_counts_and_writes_nothing() {
        let store = open_store();
        let survivor = store
            .add_note_with_created_at(
                "decision",
                "dup",
                "body",
                &["a"],
                &["f.rs"],
                None,
                "active",
                100,
            )
            .unwrap();
        let _loser = store
            .add_note_with_created_at("decision", "dup", "body", &["b"], &[], None, "active", 200)
            .unwrap();

        let before_count = note_count(&store);
        let (before_tags, _): (String, String) = store
            .conn
            .query_row(
                "SELECT tags, linked_files FROM notes WHERE id = ?1",
                rusqlite::params![survivor],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();

        let summary = store.dedupe_entity_ids(true).unwrap();
        assert_eq!(summary.total_notes, 2);
        assert_eq!(summary.duplicate_groups, 1);
        assert_eq!(summary.rows_collapsed, 1, "one loser in the group");
        assert_eq!(summary.tags_merged, 1, "loser's 'b' tag would be merged");

        assert_eq!(note_count(&store), before_count, "row count unchanged");
        let (after_tags, _): (String, String) = store
            .conn
            .query_row(
                "SELECT tags, linked_files FROM notes WHERE id = ?1",
                rusqlite::params![survivor],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(before_tags, after_tags, "tags unchanged under dry-run");
    }

    // ── AC10 + AC11: real run collapses to one row, survivor = earliest ────
    #[test]
    fn real_run_collapses_group_survivor_is_earliest_created() {
        let store = open_store();
        let survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();

        let summary = store.dedupe_entity_ids(false).unwrap();
        assert_eq!(summary.rows_collapsed, 1);
        assert_eq!(
            note_count(&store),
            1,
            "row count must drop by rows_collapsed"
        );
        assert!(store.get(survivor).unwrap().is_some(), "survivor remains");
        assert!(store.get(loser).unwrap().is_none(), "loser is gone");
    }

    // ── AC12 + AC13: tags/linked_files union add-wins, no survivor value dropped ─
    #[test]
    fn tags_and_linked_files_union_add_wins() {
        let store = open_store();
        let survivor = store
            .add_note_with_created_at(
                "decision",
                "dup",
                "body",
                &["keep"],
                &["keep.rs"],
                None,
                "active",
                100,
            )
            .unwrap();
        store
            .add_note_with_created_at(
                "decision",
                "dup",
                "body",
                &["keep", "new"],
                &["new.rs"],
                None,
                "active",
                200,
            )
            .unwrap();

        let summary = store.dedupe_entity_ids(false).unwrap();
        assert_eq!(summary.tags_merged, 1, "only the genuinely-new tag counts");
        assert_eq!(summary.linked_files_merged, 1);

        let note = store.get(survivor).unwrap().unwrap();
        assert!(note.tags.contains(&"keep".to_string()));
        assert!(note.tags.contains(&"new".to_string()));
        assert!(note.linked_files.contains(&"keep.rs".to_string()));
        assert!(note.linked_files.contains(&"new.rs".to_string()));
    }

    // ── AC14: any row archived -> survivor becomes archived ─────────────────
    #[test]
    fn any_archived_row_makes_survivor_archived() {
        let store = open_store();
        let survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "archived", 200)
            .unwrap();

        store.dedupe_entity_ids(false).unwrap();
        let note = store.get(survivor).unwrap().unwrap();
        assert_eq!(note.status, "archived");
    }

    // ── AC15: no row archived -> survivor status unchanged ──────────────────
    #[test]
    fn no_archived_row_leaves_survivor_status_unchanged() {
        let store = open_store();
        let survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();

        store.dedupe_entity_ids(false).unwrap();
        let note = store.get(survivor).unwrap().unwrap();
        assert_eq!(note.status, "active");
    }

    // ── AC16: survivor lacks superseded_by, another row has one -> adopted ──
    #[test]
    fn survivor_adopts_lone_superseded_by_from_group() {
        let store = open_store();
        let elsewhere = store
            .add_note("note", "elsewhere target", "b", &[], &[], None, None)
            .unwrap();
        let survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();
        store.set_superseded_by(loser, elsewhere).unwrap();

        store.dedupe_entity_ids(false).unwrap();
        let note = store.get(survivor).unwrap().unwrap();
        assert_eq!(note.superseded_by, Some(elsewhere));
    }

    // ── AC17: conflicting superseded_by values -> earliest wins, warn, no error ─
    #[test]
    fn conflicting_superseded_by_earliest_wins_no_error() {
        let store = open_store();
        let target_a = store
            .add_note("note", "a", "b", &[], &[], None, None)
            .unwrap();
        let target_b = store
            .add_note("note", "b", "b", &[], &[], None, None)
            .unwrap();

        let survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();
        store.set_superseded_by(survivor, target_a).unwrap();
        store.set_superseded_by(loser, target_b).unwrap();

        let result = store.dedupe_entity_ids(false);
        assert!(result.is_ok(), "a conflicting value must never error");
        let note = store.get(survivor).unwrap().unwrap();
        assert_eq!(
            note.superseded_by,
            Some(target_a),
            "the earliest-created row's (survivor's own) value must win"
        );
    }

    // ── AC18: a row elsewhere pointing at a loser is rewritten to the survivor ─
    #[test]
    fn edge_elsewhere_pointing_at_loser_is_repointed_to_survivor() {
        let store = open_store();
        let survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();
        let dependent = store
            .add_note("note", "points at loser", "b", &[], &[], None, None)
            .unwrap();
        store.set_superseded_by(dependent, loser).unwrap();

        let summary = store.dedupe_entity_ids(false).unwrap();
        assert_eq!(summary.supersede_edges_repointed, 1);
        let note = store.get(dependent).unwrap().unwrap();
        assert_eq!(note.superseded_by, Some(survivor));
    }

    // ── AC19: a rewrite that would self-point is dropped to NULL instead ────
    #[test]
    fn rewrite_that_would_self_point_is_dropped_to_null() {
        let store = open_store();
        let survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();
        // The survivor itself already points at its own duplicate-group loser.
        store.set_superseded_by(survivor, loser).unwrap();

        let summary = store.dedupe_entity_ids(false).unwrap();
        assert_eq!(summary.supersede_self_edges_dropped, 1);
        let note = store.get(survivor).unwrap().unwrap();
        assert_eq!(
            note.superseded_by, None,
            "a self-pointing rewrite must drop to NULL rather than self-loop"
        );
    }

    // ── AC20: a loser's note_embeddings row is deleted; survivor's is untouched ─
    #[test]
    fn loser_embedding_deleted_survivor_embedding_untouched() {
        let store = open_store();
        let survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();
        // note_embeddings is FLOAT[896]: 896 * 4-byte floats per row.
        let vec_a = vec![0u8; 896 * 4];
        let mut vec_b = vec![0u8; 896 * 4];
        vec_b[0] = 1;
        store.insert_embedding(survivor, &vec_a).unwrap();
        store.insert_embedding(loser, &vec_b).unwrap();

        store.dedupe_entity_ids(false).unwrap();
        assert!(has_embedding(&store, survivor), "survivor embedding kept");
        assert_eq!(
            store.get_embedding(survivor).unwrap().as_deref(),
            Some(vec_a.as_slice()),
            "survivor's embedding bytes must be provably untouched, not merely present"
        );
        assert!(
            store.get_embedding(loser).unwrap().is_none(),
            "loser's embedding row must be gone (loser row itself is deleted too)"
        );
    }

    // ── AC21: a failure injected partway through rolls back the whole run ───
    #[test]
    fn injected_fault_partway_rolls_back_whole_run() {
        let store = open_store();
        // Two independent duplicate groups so a fault after group 0 leaves
        // group 1 (and group 0's own writes) to prove the ROLLBACK, not just
        // an early-return before any write happened.
        store
            .add_note_with_created_at("decision", "dup one", "body", &[], &[], None, "active", 100)
            .unwrap();
        store
            .add_note_with_created_at("decision", "dup one", "body", &[], &[], None, "active", 200)
            .unwrap();
        store
            .add_note_with_created_at("note", "dup two", "body", &[], &[], None, "active", 300)
            .unwrap();
        store
            .add_note_with_created_at("note", "dup two", "body", &[], &[], None, "active", 400)
            .unwrap();

        let before = note_count(&store);

        inject_fault_after_group(0);
        let result = store.dedupe_entity_ids(false);
        clear_fault();

        assert!(
            result.is_err(),
            "the injected fault must surface as an error"
        );
        assert_eq!(
            note_count(&store),
            before,
            "memory.db must be unchanged after a rolled-back run (no partial collapse)"
        );
    }

    // ── Adversarial: fault mid-group (after a loser is fully deleted, before
    // the next loser in the *same* group is touched), with a byte-for-byte
    // full-table comparison rather than just a row count. AC21's own test
    // only proves rollback at a *group* boundary; this proves it holds at a
    // finer grain too, and that every column of every table dedupe can touch
    // (notes, memory_edges, note_embeddings) is provably restored, not just
    // the row count. ─────────────────────────────────────────────────────
    #[test]
    fn injected_fault_mid_group_after_partial_loser_deletion_rolls_back_byte_for_byte() {
        let store = open_store();
        let survivor = store
            .add_note_with_created_at(
                "decision",
                "dup",
                "body",
                &["orig"],
                &[],
                None,
                "active",
                100,
            )
            .unwrap();
        let loser_a = store
            .add_note_with_created_at(
                "decision",
                "dup",
                "body",
                &["from-a"],
                &[],
                None,
                "archived",
                200,
            )
            .unwrap();
        let loser_b = store
            .add_note_with_created_at(
                "decision",
                "dup",
                "body",
                &["from-b"],
                &[],
                None,
                "active",
                300,
            )
            .unwrap();
        let external = store
            .add_note("note", "external dependent", "b", &[], &[], None, None)
            .unwrap();
        // external's supersede edge points at loser_a; a fully-correct
        // rollback must restore both the notes.superseded_by column AND this
        // memory_edges row exactly as they were.
        store.supersede(external, loser_a).unwrap();
        store.insert_embedding(loser_a, &[7u8; 896 * 4]).unwrap();

        let before = full_db_snapshot(&store);
        assert_eq!(before.0.len(), 4, "precondition: 4 notes seeded");

        // Fault fires right after loser_a (index 0) is fully deleted
        // (embedding + edges + note row gone) but before loser_b is touched
        // at all: a genuinely different point than the group-boundary fault
        // AC21's own test injects.
        inject_fault_after_loser(0);
        let result = store.dedupe_entity_ids(false);
        clear_loser_fault();

        assert!(
            result.is_err(),
            "the mid-group injected fault must surface as an error"
        );
        let after = full_db_snapshot(&store);
        assert_eq!(
            after, before,
            "notes + memory_edges + note_embeddings must be byte-for-byte \
             unchanged after a run that fails mid-group (partial loser \
             deletion must roll back too, not just whole-group commits)"
        );
        // Sanity: the row that would have been deleted is genuinely still
        // present with its original content (not just "some 4 rows exist").
        assert!(
            store.get(loser_a).unwrap().is_some(),
            "loser_a survives rollback"
        );
        let restored_survivor = store.get(survivor).unwrap().unwrap();
        assert_eq!(
            restored_survivor.tags,
            vec!["orig".to_string()],
            "survivor's own tags must not have been touched by the aborted run"
        );
        let _ = loser_b; // seeded only to make this a real multi-loser group
    }

    // ── Adversarial: --dry-run must leave every column of every touched
    // table untouched, not just row count / one column. ────────────────────
    #[test]
    fn dry_run_leaves_full_db_state_byte_for_byte_unchanged() {
        let store = open_store();
        let survivor = store
            .add_note_with_created_at(
                "decision",
                "dup",
                "body",
                &["a"],
                &["f.rs"],
                None,
                "active",
                100,
            )
            .unwrap();
        let loser = store
            .add_note_with_created_at(
                "decision",
                "dup",
                "body",
                &["b"],
                &[],
                None,
                "archived",
                200,
            )
            .unwrap();
        let external = store
            .add_note("note", "external", "b", &[], &[], None, None)
            .unwrap();
        store.supersede(external, loser).unwrap();
        store.insert_embedding(survivor, &[1u8; 896 * 4]).unwrap();
        store.insert_embedding(loser, &[2u8; 896 * 4]).unwrap();

        let before = full_db_snapshot(&store);
        let summary = store.dedupe_entity_ids(true).unwrap();
        assert_eq!(
            summary.duplicate_groups, 1,
            "precondition: a group exists to report on"
        );

        let after = full_db_snapshot(&store);
        assert_eq!(
            after, before,
            "dry-run must leave notes + memory_edges + note_embeddings \
             completely unchanged, in every column, not just row count"
        );
    }

    // ── Adversarial: multiple external rows point at *different* losers
    // within the same duplicate group. Each must be independently repointed
    // to the survivor. ───────────────────────────────────────────────────────
    #[test]
    fn multiple_external_rows_pointing_at_different_losers_all_repoint_to_survivor() {
        let store = open_store();
        let survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser_a = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();
        let loser_b = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
            .unwrap();
        let dep_a = store
            .add_note("note", "points at loser_a", "b", &[], &[], None, None)
            .unwrap();
        let dep_b = store
            .add_note("note", "points at loser_b", "b", &[], &[], None, None)
            .unwrap();
        store.set_superseded_by(dep_a, loser_a).unwrap();
        store.set_superseded_by(dep_b, loser_b).unwrap();

        let summary = store.dedupe_entity_ids(false).unwrap();
        assert_eq!(summary.supersede_edges_repointed, 2);
        assert_eq!(
            store.get(dep_a).unwrap().unwrap().superseded_by,
            Some(survivor),
            "dep_a's edge to loser_a must repoint to the survivor"
        );
        assert_eq!(
            store.get(dep_b).unwrap().unwrap().superseded_by,
            Some(survivor),
            "dep_b's edge to loser_b must repoint to the survivor, independently of dep_a"
        );
    }

    // ── Adversarial: delete_edges_for_note must remove edges in *both*
    // directions for a loser (edges the loser points from, and edges other
    // notes point at the loser), leaving no orphan. ─────────────────────────
    #[test]
    fn loser_deletion_removes_memory_edges_in_both_directions_no_orphan() {
        let store = open_store();
        let survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();
        let other_a = store
            .add_note("note", "a", "b", &[], &[], None, None)
            .unwrap();
        let other_b = store
            .add_note("note", "b", "b", &[], &[], None, None)
            .unwrap();
        // loser -> other_a (loser is from_id) and other_b -> loser (loser is to_id).
        store.add_edge(loser, other_a, "relates_to").unwrap();
        store.add_edge(other_b, loser, "relates_to").unwrap();

        store.dedupe_entity_ids(false).unwrap();

        let orphans: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM memory_edges WHERE from_id = ?1 OR to_id = ?1",
                rusqlite::params![loser],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            orphans, 0,
            "no memory_edges row may reference the deleted loser id"
        );
        let _ = survivor;
    }

    // ── Adversarial: relates_to/contradicts edges to a loser are dropped
    // outright by `delete_edges_for_note`, not repointed to the survivor.
    // This differs from tags/linked_files/superseded_by, which are carefully merged.
    // This documents current (lossy) behavior so a silent further regression
    // is still caught; see the board comment for why this is flagged as a
    // follow-up rather than treated as a spec violation (ADR-068's third
    // amendment only specifies a merge rule for `superseded_by`, not for the
    // `memory_edges` relationship graph). ───────────────────────────────────
    #[test]
    fn relates_to_edge_to_external_note_is_dropped_not_repointed_known_gap() {
        let store = open_store();
        let survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();
        let external = store
            .add_note("note", "external relation", "b", &[], &[], None, None)
            .unwrap();
        store.add_edge(loser, external, "relates_to").unwrap();

        store.dedupe_entity_ids(false).unwrap();

        let (survivor_out, _) = store.get_edges(survivor).unwrap();
        assert!(
            !survivor_out.iter().any(|e| e.to_id == external),
            "documents current behavior: the loser's relates_to edge to an \
             external note is NOT repointed onto the survivor (it is simply \
             deleted with the loser). If this assertion ever fails, dedupe's \
             edge handling changed; update this test alongside the ADR."
        );
    }

    // ── Adversarial: the survivor's adoption of a group member's
    // `superseded_by` value does not validate that the value isn't itself a
    // member of the same duplicate group. When a loser's own `superseded_by`
    // points directly at the survivor, adoption creates SURVIVOR -> SURVIVOR
    // (a self-loop), unlike the *rewrite* path (AC19), which explicitly
    // guards against exactly this and drops to NULL instead. This is a
    // genuine bug, not a documented scope gap: a self-referencing
    // `superseded_by` is nonsensical regardless of the ADR's text. ─────────
    #[test]
    fn adoption_must_not_selfloop_when_a_loser_points_at_the_survivor() {
        let store = open_store();
        let survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();
        // loser was (per this row) "superseded by" the survivor itself.
        store.set_superseded_by(loser, survivor).unwrap();

        store.dedupe_entity_ids(false).unwrap();

        let note = store.get(survivor).unwrap().unwrap();
        assert_ne!(
            note.superseded_by,
            Some(survivor),
            "BUG: the survivor's superseded_by must never be adopted as its \
             own id, that is a self-loop. The adoption step at dedupe.rs's \
             `first_non_null` handling has no self-edge guard, unlike the \
             rewrite loop just below it (AC19)."
        );
    }

    // ── Adversarial: a chained loser->loser `superseded_by` pointer (one
    // loser's `superseded_by` points at *another* loser in the same group,
    // not at the survivor) gets blindly adopted onto the survivor by value,
    // even though that target row is about to be deleted in this very
    // transaction. The rewrite loop (which repoints external dependents)
    // never sees this adoption write, because `rewrites` is computed from a
    // read taken *before* the adoption write happens.
    //
    // The practical symptom is even sharper than "a dangling pointer": this
    // SQLite build enforces `foreign_keys` ON BY DEFAULT (verified directly;
    // see the board comment, this contradicts `edges.rs`'s own doc comment
    // and the Engineer's stated rationale for `delete_edges_for_note`, both
    // of which assume FK enforcement is off on this connection). `notes`
    // (`superseded_by INTEGER REFERENCES notes(id)`) has no `ON DELETE`
    // clause, so once the survivor's adoption write leaves it pointing at
    // loser_b, the later `DELETE FROM notes WHERE id = loser_b` in the same
    // transaction is rejected outright with a FOREIGN KEY constraint error:
    // the whole dedupe run fails (and correctly rolls back) for a duplicate
    // shape it should handle cleanly. ───────────────────────────────────────
    #[test]
    fn adoption_must_not_dangle_when_a_loser_points_at_a_fellow_loser() {
        let store = open_store();
        let survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser_a = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();
        let loser_b = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
            .unwrap();
        // loser_a claims to be "superseded by" loser_b, a fellow member of
        // the very same duplicate group, not an external note.
        store.set_superseded_by(loser_a, loser_b).unwrap();

        let result = store.dedupe_entity_ids(false);

        assert!(
            result.is_ok(),
            "BUG: a loser's superseded_by pointing at a fellow loser in the \
             same group (a chained in-group reference) makes the whole \
             dedupe run fail with a FOREIGN KEY constraint error instead of \
             collapsing cleanly: {:?}. The adoption step blindly copies that \
             in-group value onto the survivor before the deletion loop runs, \
             creating a live FK reference to a row this very transaction \
             then tries to delete.",
            result.as_ref().err()
        );
        // If a future fix makes this succeed, it must not merely trade the
        // hard error for a silent dangling pointer.
        if result.is_ok() {
            let note = store.get(survivor).unwrap().unwrap();
            if let Some(target) = note.superseded_by {
                assert!(
                    store.get(target).unwrap().is_some(),
                    "survivor.superseded_by ({target}) must not point at a \
                     row that no longer exists"
                );
            }
        }
    }

    // ── AC24 (structural): dedupe is never reachable except via this method ─
    // Covered by construction: `MemoryStore::open` only ever calls
    // `backfill_entity_ids`/`promote_entity_id_unique_index` (see
    // `entity_id_migration.rs`), never `dedupe_entity_ids`. See also
    // `entity_id_migration::tests::promote_with_duplicates_leaves_index_non_unique_and_does_not_error`,
    // which proves opening a store with duplicates present does not collapse
    // them.
}
