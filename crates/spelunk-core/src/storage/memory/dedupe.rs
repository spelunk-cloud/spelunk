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
        for loser in losers {
            self.delete_note_embedding(loser.id)?;
            self.delete_edges_for_note(loser.id)?;
            self.delete_note(loser.id)?;
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

    // ── AC24 (structural): dedupe is never reachable except via this method ─
    // Covered by construction: `MemoryStore::open` only ever calls
    // `backfill_entity_ids`/`promote_entity_id_unique_index` (see
    // `entity_id_migration.rs`), never `dedupe_entity_ids`. See also
    // `entity_id_migration::tests::promote_with_duplicates_leaves_index_non_unique_and_does_not_error`,
    // which proves opening a store with duplicates present does not collapse
    // them.
}
