// Collapse duplicate-`entity_id` groups already resident in `memory.db`.
// Backs `spelunk memory dedupe`. See ADR-068's third amendment for the merge
// rule: survivor = earliest `created_at`; `tags`/`linked_files` union
// add-wins; archived sticks.
//
// Invariant: no live row may reference a loser once it's deleted, whether
// the reference is in-group or cross-group. `loser_to_survivor` (every id
// being deleted this run, mapped to its group's survivor) is computed once
// up front from the pre-transaction snapshot, so it stays valid regardless
// of processing order. All `superseded_by` rewrites happen before any
// delete: each group's own survivor resolves first, then every other row,
// then losers are deleted in any order.
//
// One transaction for the whole run: any error rolls back, `memory.db`
// stays unchanged. Never called automatically (`open`, `init`, `add`, ...):
// collapsing is destructive, so it only runs via explicit `memory dedupe`.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

use super::{MemoryStore, Note};
use crate::storage::backend::numeric_note_id;

// Dedupe is a local-SQLite-only maintenance pass, so every `Note` it sees was
// read straight out of `memory.db` and its id is always a rowid. The narrowing
// is still fallible rather than an unwrap so a future caller handing it
// remote-minted notes fails with the shared message instead of panicking.
fn rowid(n: &Note) -> anyhow::Result<i64> {
    numeric_note_id(&n.id)
}

fn superseded_rowid(n: &Note) -> Option<i64> {
    n.superseded_by.as_ref().and_then(|id| id.as_i64())
}
use crate::storage::entity_id::note_entity_id;

// Summary of one `dedupe_entity_ids` run (or dry-run estimate).
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct DedupeSummary {
    pub total_notes: usize,
    pub duplicate_groups: usize,
    // Losers collapsed (rows removed).
    pub rows_collapsed: usize,
    pub tags_merged: usize,
    pub linked_files_merged: usize,
    pub supersede_edges_repointed: usize,
    pub supersede_self_edges_dropped: usize,
}

#[cfg(test)]
thread_local! {
    // Fires after the (0-indexed) n-th group has been fully applied, before
    // COMMIT. Proves the whole-run rollback guarantee under a real
    // multi-group transaction, not just the empty/no-op case.
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
    // Finer-grained than FAULT_AFTER_GROUP: fires after the (0-indexed) n-th
    // loser within the current group is fully deleted, before the next
    // loser in the same group is touched. Proves rollback holds mid-group.
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
    // Collapse every duplicate `entity_id` group in one all-or-nothing
    // transaction. `dry_run` computes the same summary via read-only queries
    // and writes nothing.
    pub fn dedupe_entity_ids(&self, dry_run: bool) -> Result<DedupeSummary> {
        let all = self
            .all_notes_for_dedup()
            .context("reading notes for dedupe")?;
        let total_notes = all.len();

        // Index into `all` rather than moving the `Note`: `all` (including
        // non-duplicate notes) is needed again below for the cross-reference
        // rewrite pass. `all_notes_for_dedup` orders by created_at ASC, so
        // each group's first element is already the survivor.
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

        // Whole-run facts, computed once up front (see module doc).
        // loser_to_survivor: every id deleted this run -> its group's
        // survivor. Purely structural (derived from group membership), so
        // it's identical regardless of processing order.
        // note_group_of: id -> group index, used only to classify a rewrite
        // as "external" for reporting, not for correctness.
        let mut loser_to_survivor: HashMap<i64, i64> = HashMap::new();
        let mut note_group_of: HashMap<i64, usize> = HashMap::new();
        let mut survivor_ids: HashSet<i64> = HashSet::new();
        for (gi, group) in duplicate_groups.iter().enumerate() {
            let survivor_id = rowid(group[0])?;
            survivor_ids.insert(survivor_id);
            for n in group {
                note_group_of.insert(rowid(n)?, gi);
            }
            for loser in &group[1..] {
                loser_to_survivor.insert(rowid(loser)?, survivor_id);
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
            // the survivor's final superseded_by, from the pre-transaction
            // snapshot only (never a live read) so group order doesn't matter.
            for (i, group) in duplicate_groups.iter().enumerate() {
                self.collapse_group_survivor(group, &loser_to_survivor, &mut summary, true)?;
                if fault_due(i) {
                    anyhow::bail!("injected test fault after group {i}");
                }
            }
            // Phase 2: rewrite every other row (ordinary note or loser of any
            // group) whose field still points at a doomed id, before any
            // loser is deleted, so phase 3 can delete in any order safely.
            self.rewrite_cross_references(
                &all,
                &survivor_ids,
                &loser_to_survivor,
                &note_group_of,
                &mut summary,
                true,
            )?;
            // Phase 3: delete every loser, any order - phase 2 already
            // cleared every live reference to these ids.
            for group in &duplicate_groups {
                for (li, loser) in group[1..].iter().enumerate() {
                    let loser_id = rowid(loser)?;
                    self.delete_note_embedding(loser_id)?;
                    self.delete_edges_for_note(loser_id)?;
                    self.delete_note(loser_id)?;
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

    // Plan (and, when `apply`, execute) one duplicate group's
    // tags/linked_files/status merge and its survivor's final
    // `superseded_by`. `group` is created_at-ASC ordered; `group[0]` is the
    // survivor.
    //
    // Dry-run and real-run share this path so their counts always agree;
    // only the trailing writes are skipped when `!apply`.
    //
    // Does not touch any other row or delete anything (see
    // `rewrite_cross_references` and phase 3 in `dedupe_entity_ids`) - kept
    // separate so no group's processing can interact with another's.
    fn collapse_group_survivor(
        &self,
        group: &[&Note],
        loser_to_survivor: &HashMap<i64, i64>,
        summary: &mut DedupeSummary,
        apply: bool,
    ) -> Result<()> {
        let survivor = group[0];
        let losers = &group[1..];
        let survivor_id = rowid(survivor)?;

        // tags / linked_files: union, add-wins
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

        // status: archived sticks
        let any_archived = group.iter().any(|n| n.status == "archived");

        // superseded_by: resolve against every id doomed this run, not just
        // this group. A candidate redirects through loser_to_survivor to its
        // group's survivor (no-op if not doomed). If that target is *this*
        // group's own survivor, it's self-referential and dropped; otherwise
        // it's a genuine external value (possibly another group's survivor).
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
            .filter_map(|n| superseded_rowid(n).and_then(resolve))
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
        // supersede_self_edges_dropped counts only the survivor's own value
        // resolving to nothing; losers' references are handled (and not
        // counted) by rewrite_cross_references, since those rows are deleted.
        let survivor_self_edge_dropped =
            matches!(superseded_rowid(survivor).map(resolve), Some(None));
        if survivor_self_edge_dropped {
            summary.supersede_self_edges_dropped += 1;
        }

        summary.rows_collapsed += losers.len();

        if !apply {
            return Ok(());
        }

        // apply phase (real run only)
        if !new_tags.is_empty() || !new_files.is_empty() {
            self.union_tags_and_files(survivor_id, &new_tags, &new_files)?;
        }
        if any_archived {
            self.archive(survivor_id)?;
        }
        match resolved_survivor_target {
            Some(val) if superseded_rowid(survivor) != Some(val) => {
                self.set_superseded_by(survivor_id, val)?;
            }
            None if survivor.superseded_by.is_some() => {
                // Resolved to nothing (self-referential) with no external
                // fallback in the group: clear rather than leave stale.
                self.clear_superseded_by(survivor_id)?;
            }
            _ => {}
        }

        Ok(())
    }

    // Rewrite every row that is not itself a survivor (an ordinary note or
    // a loser of any group) whose `superseded_by` still points at an id
    // being deleted this run. Targets resolve through `loser_to_survivor`,
    // so a rewrite can only land on a surviving id, never another doomed one.
    //
    // Runs once, globally, before any loser is deleted (see module doc for
    // why this ordering is the actual invariant).
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
            let note_id = rowid(note)?;
            if survivor_ids.contains(&note_id) {
                continue; // the survivor's own field is resolved separately
            }
            let Some(v) = superseded_rowid(note) else {
                continue;
            };
            let Some(&target) = loser_to_survivor.get(&v) else {
                continue; // not a doomed id: nothing to do
            };
            // In-group rewrites are inert clean-up (target is deleted
            // regardless), so only cross-group rewrites count as a "repoint".
            let same_group = matches!(
                (note_group_of.get(&note_id), note_group_of.get(&v)),
                (Some(a), Some(b)) if a == b
            );
            if !same_group {
                summary.supersede_edges_repointed += 1;
            }
            if apply {
                self.set_superseded_by(note_id, target)?;
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
