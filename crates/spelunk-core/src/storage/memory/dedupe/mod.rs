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
mod superseded_by_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
