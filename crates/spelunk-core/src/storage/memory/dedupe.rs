//! Collapse duplicate-`entity_id` groups already resident in `memory.db`.
//!
//! Backs the `spelunk memory dedupe` command. See ADR-068's third amendment
//! for the merge rule this implements: survivor = earliest `created_at`;
//! `tags`/`linked_files` union add-wins; archived sticks.
//!
//! `superseded_by` handling treats any reference to a row this *run* is
//! about to delete or collapse away — whether the referring row and the
//! referenced row belong to the *same* duplicate group or to two *different*
//! groups both being collapsed in this same run — as a single, first-class,
//! whole-run fact, established once up front, before any group is
//! processed and before any transaction begins. Such a value is never worth
//! keeping as written: its target is either some group's survivor (the
//! referring row's resolved value is that survivor, not the doomed id) or,
//! when that target is the referring row's *own* group's survivor, the
//! referring row itself — a self-loop, dropped.
//!
//! Five rounds of adversarial testing each found a wider-scope symptom of
//! handling this reactively, or at too narrow a scope, instead: a self-loop
//! from blind adoption; adoption racing the external-rewrite loop over the
//! same field; the loser-deletion loop hitting a live foreign-key reference
//! from a not-yet-deleted fellow loser (both a directional and a two-cycle
//! shape); and finally a reference that crosses into a *different*
//! duplicate group being collapsed in the same run — invisible to every
//! prior fix, because each was scoped to "within one group" rather than to
//! the actual invariant, which is whole-run: by the time any loser is
//! deleted, no live row anywhere in `notes` may still reference it,
//! regardless of which group it, or the referencing row, belongs to.
//!
//! The whole-run fix, computed once before any group is processed:
//!   - `loser_to_survivor` maps every id that will be deleted this run,
//!     across *every* duplicate group, to the survivor its own group
//!     collapses into. This is purely structural — derived only from group
//!     membership (earliest `created_at` per `entity_id` group) — so it is
//!     identical regardless of processing order and never depends on a live
//!     read, closing the cross-group snapshot staleness the fifth round
//!     found;
//!   - each group's own survivor resolves its final value by scanning every
//!     group member's original value (survivor's own included, in
//!     `created_at` order) and redirecting any doomed id through
//!     `loser_to_survivor`; a value that redirects to *this* group's own
//!     survivor is self-referential and dropped, the same rule rounds 1-2
//!     established for the single-group case, now applied after redirect
//!     rather than only to a raw, un-redirected id;
//!   - every *other* row in the table — an ordinary note, or a loser
//!     belonging to *any* group — whose own field still points at a doomed
//!     id is rewritten the same way, once, globally, before any loser is
//!     deleted. This is what makes the deletion loop safe in any order,
//!     intra-group (round 3) or cross-group (round 4), without a live query:
//!     every write target comes from `loser_to_survivor`, so it can only
//!     ever be a surviving row's id, never another doomed one.
//!
//! Losers and their `note_embeddings` row are then deleted. The whole run is
//! one transaction: any error rolls back, leaving `memory.db` exactly as it
//! was.
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

        // Group by entity_id, keeping each note's *index* into `all` rather
        // than moving the `Note` itself: `all`, in full — every singleton,
        // non-duplicate note included — is needed again below, for the
        // whole-run reference-rewrite pass. `all_notes_for_dedup` orders by
        // created_at ASC, so each group's first element is the
        // earliest-created row: the survivor, with no separate sort needed.
        let mut group_indices: Vec<Vec<usize>> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for (i, n) in all.iter().enumerate() {
            let eid = note_entity_id(n);
            match index.get(&eid) {
                Some(&gi) => group_indices[gi].push(i),
                None => {
                    index.insert(eid, group_indices.len());
                    group_indices.push(vec![i]);
                }
            }
        }
        let duplicate_group_indices: Vec<Vec<usize>> =
            group_indices.into_iter().filter(|g| g.len() > 1).collect();

        let mut summary = DedupeSummary {
            total_notes,
            duplicate_groups: duplicate_group_indices.len(),
            ..Default::default()
        };

        if duplicate_group_indices.is_empty() {
            return Ok(summary);
        }

        let duplicate_groups: Vec<Vec<&Note>> = duplicate_group_indices
            .iter()
            .map(|idxs| idxs.iter().map(|&i| &all[i]).collect())
            .collect();

        // ── whole-run facts, computed once, up front, before any group is
        // processed or any transaction begins (see the module doc) ────────
        //
        // `loser_to_survivor` names every id that will be deleted this run,
        // across *every* duplicate group, mapped to the survivor its own
        // group collapses into. Purely structural — derived only from group
        // membership — so it is identical regardless of processing order and
        // never depends on a live read. `note_group_of` names, for every id
        // that is a member of *any* duplicate group (survivor or loser),
        // which group; used only to decide whether a rewrite counts as
        // "genuinely external" for reporting, never for correctness.
        let mut loser_to_survivor: HashMap<i64, i64> = HashMap::new();
        let mut note_group_of: HashMap<i64, usize> = HashMap::new();
        let mut survivor_ids: HashSet<i64> = HashSet::new();
        for (gi, group) in duplicate_groups.iter().enumerate() {
            let survivor_id = group[0].id;
            survivor_ids.insert(survivor_id);
            for n in group {
                note_group_of.insert(n.id, gi);
            }
            for loser in &group[1..] {
                loser_to_survivor.insert(loser.id, survivor_id);
            }
        }

        if dry_run {
            for group in &duplicate_groups {
                self.collapse_group_survivor(group, &loser_to_survivor, &mut summary, false)?;
            }
            self.rewrite_cross_references(
                &all,
                &survivor_ids,
                &loser_to_survivor,
                &note_group_of,
                &mut summary,
                false,
            )?;
            return Ok(summary);
        }

        self.execute_batch("BEGIN IMMEDIATE")
            .context("beginning dedupe transaction")?;
        let result: Result<()> = (|| {
            // Phase 1: per group, merge tags/linked_files/status and resolve
            // the survivor's own final `superseded_by`. Every candidate
            // value is drawn from the immutable pre-transaction snapshot and
            // resolved through the whole-run map above, never a live read,
            // so groups may be processed in any order with an identical
            // result.
            for (i, group) in duplicate_groups.iter().enumerate() {
                self.collapse_group_survivor(group, &loser_to_survivor, &mut summary, true)?;
                if fault_due(i) {
                    anyhow::bail!("injected test fault after group {i}");
                }
            }
            // Phase 2: rewrite every *other* row in the whole table — an
            // ordinary note, or a loser belonging to any group in this run —
            // whose own field still refers to a doomed id, before any loser
            // is deleted. This is what makes the deletion loop safe
            // regardless of order, both within a group (round 3) and across
            // groups (round 4): by the time phase 3 runs, no live row
            // anywhere still references an id phase 3 is about to delete.
            self.rewrite_cross_references(
                &all,
                &survivor_ids,
                &loser_to_survivor,
                &note_group_of,
                &mut summary,
                true,
            )?;
            // Phase 3: delete every loser, from every group. Safe in any
            // order — intra-group or cross-group — because phase 2 already
            // cleared every live reference to every id being deleted here.
            for group in &duplicate_groups {
                for (li, loser) in group[1..].iter().enumerate() {
                    self.delete_note_embedding(loser.id)?;
                    self.delete_edges_for_note(loser.id)?;
                    self.delete_note(loser.id)?;
                    if loser_fault_due(li) {
                        anyhow::bail!(
                            "injected test fault after deleting loser index {li} within group"
                        );
                    }
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

    /// Plan (and, when `apply`, execute) one duplicate group's
    /// tags/linked_files/status merge and its survivor's own final
    /// `superseded_by`. `group` is ordered `created_at` ASC; `group[0]` is
    /// the survivor.
    ///
    /// Counting and mutation share this one path so a dry-run summary and a
    /// real run always agree: every count here is derived the same way in
    /// both modes, only the trailing writes are skipped when `!apply`.
    ///
    /// Does *not* touch any other row's field (see `rewrite_cross_references`
    /// for that) and does *not* delete anything: both now happen once,
    /// globally, after every group's own survivor has been resolved, so that
    /// no group's own processing can race or interact with another group's —
    /// see the module doc for why that used to be exactly the gap.
    fn collapse_group_survivor(
        &self,
        group: &[&Note],
        loser_to_survivor: &HashMap<i64, i64>,
        summary: &mut DedupeSummary,
        apply: bool,
    ) -> Result<()> {
        let survivor = group[0];
        let losers = &group[1..];
        let survivor_id = survivor.id;

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

        // ── superseded_by: resolve the survivor's final value against every
        // id doomed anywhere in this run, not just this group ───────────────
        //
        // A candidate value redirects through `loser_to_survivor` to the
        // survivor its own group collapses into (a no-op redirect when the
        // value isn't doomed at all this run). If that resolved target is
        // *this* group's own survivor, the candidate is self-referential —
        // whether the raw value pointed at this survivor directly, at a
        // fellow loser of this same group, or at a loser of some other group
        // (never actually possible to redirect back to *this* survivor,
        // since groups partition disjointly, but the check does not need to
        // assume that) — and must be dropped, exactly as rounds 1-2
        // established for the single-group case. Otherwise it's a genuinely
        // resolved external candidate, whether that candidate lives entirely
        // outside every duplicate group or is itself the survivor of a
        // *different* group being collapsed in this same run (round 4).
        let resolve = |v: i64| -> Option<i64> {
            let target = loser_to_survivor.get(&v).copied().unwrap_or(v);
            if target == survivor_id {
                None
            } else {
                Some(target)
            }
        };

        let external_values: Vec<i64> = group
            .iter()
            .filter_map(|n| n.superseded_by.and_then(resolve))
            .collect();
        let resolved_survivor_target = external_values.first().copied();
        if let Some(val) = resolved_survivor_target {
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
        // Did the survivor's *own* pre-existing value resolve to nothing —
        // i.e. was it self-referential, per the check above? That's what
        // `supersede_self_edges_dropped` reports, regardless of whether a
        // fall-through external candidate replaces it or it lands `None`.
        // (Losers' own doomed references are handled by
        // `rewrite_cross_references` and aren't counted here: those rows are
        // about to be deleted, so the field's value is not user-visible
        // state.)
        let survivor_self_edge_dropped = matches!(survivor.superseded_by.map(resolve), Some(None));
        if survivor_self_edge_dropped {
            summary.supersede_self_edges_dropped += 1;
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
        match resolved_survivor_target {
            Some(val) if survivor.superseded_by != Some(val) => {
                self.set_superseded_by(survivor.id, val)?;
            }
            None if survivor.superseded_by.is_some() => {
                // The survivor's own original value must have resolved to
                // nothing (self-referential, per the check above) with no
                // external fallback anywhere in the group: clear it
                // explicitly rather than leaving the stale original value
                // in place.
                self.clear_superseded_by(survivor.id)?;
            }
            _ => {}
        }

        Ok(())
    }

    /// Rewrite every note that is *not* itself a duplicate-group survivor —
    /// an ordinary note untouched by dedup, or a loser belonging to *any*
    /// group in this run — whose own `superseded_by` still refers to an id
    /// that will be deleted this run. Every write target is looked up
    /// through `loser_to_survivor`, so it can only ever resolve to a
    /// surviving row's id, never to another doomed id.
    ///
    /// Runs once, globally, over the full pre-transaction snapshot
    /// (`all_notes`), after every group's own survivor has been resolved
    /// (`collapse_group_survivor`) and before any loser is deleted: this is
    /// what guarantees no live row anywhere still references a
    /// soon-to-be-deleted id by the time deletion happens, regardless of
    /// group processing order or whether the reference crosses group
    /// boundaries — the actual invariant five rounds of adversarial testing
    /// converged on (see the module doc).
    fn rewrite_cross_references(
        &self,
        all_notes: &[Note],
        survivor_ids: &HashSet<i64>,
        loser_to_survivor: &HashMap<i64, i64>,
        note_group_of: &HashMap<i64, usize>,
        summary: &mut DedupeSummary,
        apply: bool,
    ) -> Result<()> {
        for note in all_notes {
            if survivor_ids.contains(&note.id) {
                continue; // the survivor's own field is resolved separately
            }
            let Some(v) = note.superseded_by else {
                continue;
            };
            let Some(&target) = loser_to_survivor.get(&v) else {
                continue; // not a doomed id: nothing to do
            };
            // Only count as a "repoint" when the referencing row isn't
            // itself a fellow member of the very group the doomed id
            // belongs to: that in-group case is inert clean-up (its target
            // is being deleted regardless of this write), the same
            // distinction `supersede_self_edges_dropped` draws for the
            // survivor's own field.
            let same_group = matches!(
                (note_group_of.get(&note.id), note_group_of.get(&v)),
                (Some(a), Some(b)) if a == b
            );
            if !same_group {
                summary.supersede_edges_repointed += 1;
            }
            if apply {
                self.set_superseded_by(note.id, target)?;
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

    // ── Adversarial (re-verification pass): the survivor's OWN pre-existing
    // `superseded_by` pointed at a fellow in-group loser (not itself, and not
    // the "loser points at survivor" shape already covered above — this is
    // the third permutation: *survivor* points at a *loser*). The adoption
    // fix correctly filters this in-group value out of `external_values` and
    // falls through to a genuinely-external candidate elsewhere in the group
    // (mirroring the "3+ losers, first candidate intra-group-dangling, later
    // candidate valid" check). But the *rewrite* loop below adoption computes
    // `notes_pointing_at(loser_x.id)` against the ORIGINAL pre-transaction
    // state, which still shows survivor -> loser_x, so it independently
    // re-discovers this same edge, treats it as a self-edge case (`ref_id ==
    // survivor.id`), and clears it back to NULL *after* the adoption write
    // already ran — clobbering the correctly-adopted external value.
    #[test]
    fn adoption_survivor_own_in_group_pointer_does_not_clobber_fallthrough_adoption() {
        let store = open_store();
        let external = store
            .add_note("note", "external target", "b", &[], &[], None, None)
            .unwrap();
        let survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser_x = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();
        let loser_y = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
            .unwrap();
        // Survivor's own pre-existing value points at a fellow duplicate
        // (loser_x), an in-group reference that must not be adopted verbatim.
        store.set_superseded_by(survivor, loser_x).unwrap();
        // loser_y carries a genuinely-external candidate: once the in-group
        // value is filtered out, this should be what the survivor ends up
        // pointing at (fall-through adoption), not NULL.
        store.set_superseded_by(loser_y, external).unwrap();

        store.dedupe_entity_ids(false).unwrap();

        let note = store.get(survivor).unwrap().unwrap();
        assert_eq!(
            note.superseded_by,
            Some(external),
            "BUG: the survivor's own pre-existing in-group pointer (at loser_x) \
             should be filtered out of adoption and fall through to loser_y's \
             genuinely-external value, same as any other in-group candidate. \
             Instead the rewrite loop (computed from notes_pointing_at(loser_x), \
             which still shows the ORIGINAL survivor->loser_x edge) independently \
             rediscovers survivor as a 'self-edge' dependent of loser_x and clears \
             it to NULL *after* the adoption write already set the correct \
             external value, silently discarding a valid adopted candidate."
        );
    }

    // ── Adversarial (re-verification pass): 3+ losers, first candidate in
    // iteration order is intra-group-dangling (points at a fellow loser), a
    // later candidate is genuinely external. Confirms fall-through works when
    // the in-group pointer is on a *loser*, not the survivor. ───────────────
    #[test]
    fn fallthrough_adoption_skips_intragroup_dangling_candidate_and_adopts_later_external_one() {
        let store = open_store();
        let external = store
            .add_note("note", "external target", "b", &[], &[], None, None)
            .unwrap();
        let survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser_a = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();
        let loser_b = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
            .unwrap();
        let loser_c = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 400)
            .unwrap();
        // loser_a (earliest loser, first candidate in iteration order) points
        // at a fellow loser: intra-group-dangling, must be skipped.
        store.set_superseded_by(loser_a, loser_b).unwrap();
        // loser_c (later in iteration order) carries the only valid external
        // candidate.
        store.set_superseded_by(loser_c, external).unwrap();

        store.dedupe_entity_ids(false).unwrap();

        let note = store.get(survivor).unwrap().unwrap();
        assert_eq!(
            note.superseded_by,
            Some(external),
            "the intra-group-dangling candidate from loser_a must be skipped \
             and the later, genuinely-external candidate from loser_c adopted"
        );
    }

    // ── Adversarial (re-verification pass): only intra-group candidates
    // exist anywhere in the group (no genuinely external value at all).
    // Confirms adoption correctly resolves to None rather than erroring or
    // keeping a bad in-group value. ─────────────────────────────────────────
    #[test]
    fn adoption_resolves_to_none_when_every_candidate_is_intragroup() {
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
        // loser_a -> loser_b and loser_b -> survivor: every candidate value
        // in the group resolves to a fellow group member, none external.
        store.set_superseded_by(loser_a, loser_b).unwrap();
        store.set_superseded_by(loser_b, survivor).unwrap();

        let result = store.dedupe_entity_ids(false);
        assert!(
            result.is_ok(),
            "an all-intra-group candidate set must not error: {:?}",
            result.as_ref().err()
        );
        let note = store.get(survivor).unwrap().unwrap();
        assert_eq!(
            note.superseded_by, None,
            "with zero valid external candidates in the group, the survivor \
             must adopt None, not error and not retain a dangling/self value"
        );
    }

    // ── Adversarial (round 3): a LATER-created loser's own `superseded_by`
    // points at an EARLIER-created fellow loser (not the survivor). Neither
    // adoption nor the rewrite loop ever touches this value — it's correctly
    // excluded from both, per the "a fellow loser's own field doesn't matter
    // since that row is being deleted" comment above the rewrite loop — so it
    // sits untouched on loser_late's own row until deletion time. But the
    // deletion loop deletes losers in `created_at` ASC order (loser_early,
    // then loser_late), i.e. it tries to delete loser_early — the row
    // loser_late still references — *before* loser_late (the referencing
    // row) is gone. Under live FK enforcement (empirically confirmed active
    // on this connection: `notes.superseded_by` has no `ON DELETE` clause)
    // this must fail with a FOREIGN KEY constraint error on the delete of
    // loser_early, exactly the same user-visible symptom as the two
    // previously-fixed bugs, but via the deletion loop's ordering rather than
    // the adoption/rewrite write paths. ──────────────────────────────────────
    #[test]
    fn later_loser_pointing_at_earlier_fellow_loser_must_not_break_deletion_order() {
        let store = open_store();
        let _survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser_early = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();
        let loser_late = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
            .unwrap();
        // loser_late (deleted SECOND, per created_at ASC order) points at
        // loser_early (deleted FIRST) — the referencing row outlives, in
        // deletion order, the row it references.
        store.set_superseded_by(loser_late, loser_early).unwrap();

        let result = store.dedupe_entity_ids(false);

        assert!(
            result.is_ok(),
            "BUG: a later-created loser pointing at an earlier-created \
             fellow loser breaks the deletion loop's naive created_at-ASC \
             order — deleting loser_early while loser_late (not yet \
             deleted) still references it via superseded_by triggers a \
             live FOREIGN KEY constraint error and the whole run fails \
             instead of collapsing cleanly: {:?}",
            result.as_ref().err()
        );
    }

    // ── Adversarial (round 3): the same deletion-order hazard as above, but
    // with the roles swapped per the story's own round-2-repro-swap
    // instruction: two losers point AT EACH OTHER (a 2-cycle among fellow
    // losers), survivor points at nothing. Neither value is external, so
    // nothing is adopted onto the survivor and nothing is queued in
    // `rewrites` — same as the one-directional case above — but now
    // *whichever* loser the deletion loop tries to delete first is still
    // referenced by the other (not-yet-deleted) loser, so this must fail
    // regardless of `created_at` order, not just in the "later points at
    // earlier" direction. ────────────────────────────────────────────────────
    #[test]
    fn mutually_referencing_fellow_losers_must_not_break_deletion_order() {
        let store = open_store();
        let _survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser_a = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();
        let loser_b = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
            .unwrap();
        store.set_superseded_by(loser_a, loser_b).unwrap();
        store.set_superseded_by(loser_b, loser_a).unwrap();

        let result = store.dedupe_entity_ids(false);

        assert!(
            result.is_ok(),
            "BUG: two fellow losers pointing at each other (a 2-cycle) \
             breaks the deletion loop regardless of created_at order — \
             whichever is deleted first is still referenced by the other, \
             not-yet-deleted loser, triggering a FOREIGN KEY constraint \
             error: {:?}",
            result.as_ref().err()
        );
    }

    // ── Adversarial (round 3): a 4-note group where multiple members carry
    // non-null superseded_by pointing at a mix of intra-group and external
    // targets, including a chain (loser -> loser -> external). Confirms the
    // unified resolution still picks a sensible, deterministic external
    // value, the counts stay accurate, AND — this is the actual failure mode
    // this round found — the deletion loop must not choke on the in-group
    // chain reference along the way. ─────────────────────────────────────────
    #[test]
    fn four_note_group_with_mixed_intragroup_and_external_pointers_resolves_deterministically() {
        let store = open_store();
        let external_a = store
            .add_note("note", "external a", "b", &[], &[], None, None)
            .unwrap();
        let external_b = store
            .add_note("note", "external b", "b", &[], &[], None, None)
            .unwrap();
        let _survivor = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser1 = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();
        let loser2 = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
            .unwrap();
        let loser3 = store
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 400)
            .unwrap();
        // loser1 (earliest loser, first candidate in iteration order) chains
        // to a fellow loser: intra-group, must be skipped for adoption AND
        // must not break deletion order.
        store.set_superseded_by(loser1, loser2).unwrap();
        // loser2 itself carries a genuinely-external value.
        store.set_superseded_by(loser2, external_a).unwrap();
        // loser3 carries a different external value (conflict case).
        store.set_superseded_by(loser3, external_b).unwrap();

        let summary = store.dedupe_entity_ids(false).unwrap();
        // Resolution order is group order (created_at ASC): survivor(None),
        // loser1(->loser2, in-group, dropped), loser2(->external_a, first
        // surviving external candidate), loser3(->external_b, conflicting).
        // external_a must win.
        let note = store.get(_survivor).unwrap().unwrap();
        assert_eq!(
            note.superseded_by,
            Some(external_a),
            "the earliest-created external candidate (from loser2) must win, \
             with loser1's in-group chain to loser2 correctly excluded"
        );
        assert_eq!(summary.rows_collapsed, 3);

        // Re-run determinism: rebuild an identical store and confirm the same
        // resolution and counts on a second, independent run (not just
        // repeatable within one process — a genuinely fresh grouping pass).
        let store2 = open_store();
        let external_a2 = store2
            .add_note("note", "external a", "b", &[], &[], None, None)
            .unwrap();
        let external_b2 = store2
            .add_note("note", "external b", "b", &[], &[], None, None)
            .unwrap();
        let survivor2 = store2
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser1b = store2
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
            .unwrap();
        let loser2b = store2
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
            .unwrap();
        let loser3b = store2
            .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 400)
            .unwrap();
        store2.set_superseded_by(loser1b, loser2b).unwrap();
        store2.set_superseded_by(loser2b, external_a2).unwrap();
        store2.set_superseded_by(loser3b, external_b2).unwrap();
        store2.dedupe_entity_ids(false).unwrap();
        let note2 = store2.get(survivor2).unwrap().unwrap();
        assert_eq!(
            note2.superseded_by,
            Some(external_a2),
            "the resolution must be deterministic across independent runs \
             on an equivalent input shape, not HashMap-iteration-order- \
             dependent"
        );
    }

    // ── Adversarial (round-4 re-verification): TWO duplicate groups in the
    // SAME `dedupe_entity_ids` call, where a member of the *second* group
    // (processed later) has its own `superseded_by` pointing at a member of
    // the *first* group (processed earlier). This probes whether "the
    // current group" is scoped correctly when the run collapses more than
    // one group.
    //
    // `group: &[Note]` passed into `collapse_group` is a fixed snapshot taken
    // by `all_notes_for_dedup()` once, before the transaction begins and
    // before any group is processed. The external-rewrite loop reads live via
    // `notes_pointing_at`, so it sees every prior group's writes. But the
    // in-group/adoption resolution reads a group member's `superseded_by`
    // straight off that same stale, pre-transaction `Note` snapshot — it has
    // no way to know a *different* group, processed earlier in this same
    // run, already rewrote that value or deleted its target.
    //
    // Group A (survivor_a, loser_a) is processed first (created_at 100/200).
    // Group B (survivor_b, external_x) is processed second (created_at
    // 150/250). `external_x` is itself group B's loser, but its own
    // `superseded_by` points at `loser_a` — a member of group A, unrelated to
    // group B's identity.
    //
    // Processing group A: the external-rewrite loop's live query correctly
    // finds external_x pointing at loser_a and repoints it to survivor_a
    // (external_x is not a member of group A, so this is legitimate), then
    // deletes loser_a.
    //
    // Processing group B: survivor_b's adoption resolution reads external_x's
    // *stale* snapshot value (still `loser_a`, not survivor_a) and treats it
    // as a genuinely-external candidate (loser_a isn't a member of group B's
    // `group_ids`), so it tries to write `survivor_b.superseded_by =
    // loser_a.id` — a row deleted by group A moments earlier in the very
    // same transaction. Under live FK enforcement this fails outright instead
    // of collapsing cleanly, the same user-visible symptom as rounds 1-3, but
    // via cross-group snapshot staleness rather than a single group's own
    // internal bookkeeping.
    #[test]
    fn external_row_that_is_itself_a_duplicate_in_a_different_group_is_resolved_correctly_across_groups()
     {
        let store = open_store();
        let survivor_a = store
            .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 100)
            .unwrap();
        let loser_a = store
            .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 200)
            .unwrap();
        let survivor_b = store
            .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 150)
            .unwrap();
        let external_x = store
            .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 250)
            .unwrap();
        // external_x is a loser in group B, but its own pre-existing
        // superseded_by points at loser_a, a member of the unrelated group A.
        store.set_superseded_by(external_x, loser_a).unwrap();

        let result = store.dedupe_entity_ids(false);

        assert!(
            result.is_ok(),
            "BUG: group B's adoption resolution read external_x's \
             superseded_by from a stale pre-transaction snapshot (still \
             pointing at loser_a) rather than the live DB (where group A's \
             earlier processing already repointed it to survivor_a and \
             deleted loser_a), so it tried to write survivor_b.superseded_by \
             = loser_a onto a row already deleted in this same transaction: \
             {:?}",
            result.as_ref().err()
        );
        if let Ok(summary) = &result {
            assert_eq!(summary.rows_collapsed, 2, "both groups' losers collapsed");
            let sb = store.get(survivor_b).unwrap().unwrap();
            assert_eq!(
                sb.superseded_by,
                Some(survivor_a),
                "survivor_b must end up pointing at survivor_a (the row \
                 loser_a was merged into), not the deleted loser_a, and not \
                 be left dangling"
            );
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
