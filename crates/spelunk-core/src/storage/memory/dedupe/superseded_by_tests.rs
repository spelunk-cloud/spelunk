//! `superseded_by` reference-resolution edge cases across duplicate-`entity_id`
//! groups: adoption self-loop/dangling guards, deletion-order safety, and
//! cross-group reference resolution (see the module doc on `dedupe/mod.rs`
//! for the five rounds of adversarial hardening this covers).

use super::test_support::{note_count, open_store};
use super::*;

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
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser, _) = store
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
// SQLite build enforces `foreign_keys` ON by default (verified directly),
// and `notes` (`superseded_by INTEGER REFERENCES notes(id)`) has no
// `ON DELETE` clause, so once the survivor's adoption write leaves it
// pointing at loser_b, the later `DELETE FROM notes WHERE id = loser_b` in
// the same transaction is rejected outright with a FOREIGN KEY constraint
// error: the whole dedupe run fails (and correctly rolls back) for a
// duplicate shape it should handle cleanly. ─────────────────────────────
#[test]
fn adoption_must_not_dangle_when_a_loser_points_at_a_fellow_loser() {
    let store = open_store();
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_a, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_b, _) = store
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
    let (external, _) = store
        .add_note("note", "external target", "b", &[], &[], None, None)
        .unwrap();
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_x, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_y, _) = store
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
    let (external, _) = store
        .add_note("note", "external target", "b", &[], &[], None, None)
        .unwrap();
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_a, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_b, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
        .unwrap();
    let (loser_c, _) = store
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
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_a, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_b, _) = store
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
    let (_survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_early, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_late, _) = store
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
    let (_survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_a, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_b, _) = store
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
    let (external_a, _) = store
        .add_note("note", "external a", "b", &[], &[], None, None)
        .unwrap();
    let (external_b, _) = store
        .add_note("note", "external b", "b", &[], &[], None, None)
        .unwrap();
    let (_survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser1, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser2, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
        .unwrap();
    let (loser3, _) = store
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
    let (external_a2, _) = store2
        .add_note("note", "external a", "b", &[], &[], None, None)
        .unwrap();
    let (external_b2, _) = store2
        .add_note("note", "external b", "b", &[], &[], None, None)
        .unwrap();
    let (survivor2, _) = store2
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser1b, _) = store2
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser2b, _) = store2
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
        .unwrap();
    let (loser3b, _) = store2
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
    let (survivor_a, _) = store
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_a, _) = store
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (survivor_b, _) = store
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 150)
        .unwrap();
    let (external_x, _) = store
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

// ── Adversarial (round-5 re-verification, whole-run restructuring):
// a CHAINED cross-group reference. Group A's survivor's own field points
// at a *loser* of group B; group B's survivor's own field independently
// points at a *loser* of group C. `loser_to_survivor` is a flat, one-hop
// map (every doomed id maps directly to its own group's survivor, never
// to another doomed id, since group membership partitions disjointly),
// so each edge should resolve in exactly one redirect to the concrete
// target group's survivor — never a raw loser id, and never chasing
// through what the *target* survivor's own field happens to point at
// (that would be a different, unrelated edge). This test proves both
// hops resolve independently and correctly in the same run. ────────────
#[test]
fn chained_cross_group_reference_resolves_one_hop_not_to_intermediate_loser() {
    let store = open_store();
    let (survivor_a, _) = store
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (survivor_b, _) = store
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 150)
        .unwrap();
    let (loser_b, _) = store
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 250)
        .unwrap();
    let (survivor_c, _) = store
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 175)
        .unwrap();
    let (loser_c, _) = store
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 275)
        .unwrap();
    // A's survivor points at B's loser; B's survivor independently points
    // at C's loser. Two separate edges, not a transitive chain.
    store.set_superseded_by(survivor_a, loser_b).unwrap();
    store.set_superseded_by(survivor_b, loser_c).unwrap();

    let summary = store.dedupe_entity_ids(false).unwrap();
    assert_eq!(
        summary.rows_collapsed, 2,
        "one loser each in groups B and C"
    );

    let a = store.get(survivor_a).unwrap().unwrap();
    assert_eq!(
        a.superseded_by,
        Some(survivor_b),
        "A's edge to B's loser must resolve to B's survivor id directly, \
         not the raw (now-deleted) loser_b id"
    );
    let b = store.get(survivor_b).unwrap().unwrap();
    assert_eq!(
        b.superseded_by,
        Some(survivor_c),
        "B's own edge to C's loser must resolve to C's survivor \
         independently of A's edge into B"
    );
    assert!(
        store.get(loser_b).unwrap().is_none() && store.get(loser_c).unwrap().is_none(),
        "both losers must actually be gone"
    );
}

// ── Adversarial (round-5 re-verification): an ordinary note that is a
// member of *no* duplicate group at all (not a survivor, not a loser of
// any group) whose `superseded_by` points at a loser belonging to some
// *other* group being collapsed in this same run. `rewrite_cross_references`'s
// "every non-survivor row" framing must genuinely include this row: its
// id is absent from `note_group_of` entirely (unlike a fellow loser's,
// which *is* present), so the `same_group` check must not accidentally
// treat "absent from the map" as "same group" (e.g. via a mismatched
// default or an `unwrap_or` that coerces both sides to a shared
// sentinel). Three unrelated duplicate groups are present simultaneously
// so there's a real chance for the ordinary note's id or the target's id
// to collide with map bookkeeping if the None-handling were wrong. ─────
#[test]
fn ordinary_note_outside_every_group_pointing_at_a_loser_is_rewritten_and_counted() {
    let store = open_store();
    // Three unrelated duplicate groups, present purely as bookkeeping
    // noise / potential id-collision surface for note_group_of.
    let (_survivor_a, _) = store
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (_loser_a, _) = store
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 110)
        .unwrap();
    let (survivor_b, _) = store
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 120)
        .unwrap();
    let (loser_b, _) = store
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 130)
        .unwrap();
    let (_survivor_c, _) = store
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 140)
        .unwrap();
    let (_loser_c, _) = store
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 150)
        .unwrap();
    // An ordinary, wholly unique note: not a member of any group above.
    let (ordinary, _) = store
        .add_note("note", "ordinary unique note", "body", &[], &[], None, None)
        .unwrap();
    store.set_superseded_by(ordinary, loser_b).unwrap();

    let summary = store.dedupe_entity_ids(false).unwrap();
    assert_eq!(summary.rows_collapsed, 3, "one loser in each of 3 groups");
    assert_eq!(
        summary.supersede_edges_repointed, 1,
        "the ordinary note's edge into group B's loser must count as a \
         genuine (non-same-group) repoint"
    );
    let note = store.get(ordinary).unwrap().unwrap();
    assert_eq!(
        note.superseded_by,
        Some(survivor_b),
        "BUG CHECK: an ordinary note with no group membership at all must \
         still be rewritten by rewrite_cross_references; note_group_of.get \
         returning None for this row must not be mistaken for group \
         membership"
    );
}

// ── Adversarial (round-5 re-verification): three duplicate groups whose
// survivors form a reference CYCLE through each other's losers (A -> B's
// loser, B -> C's loser, C -> A's loser). `loser_to_survivor` is computed
// once, structurally, before any group is processed, so no group's
// resolution can depend on another group having been processed first —
// this test constructs the identical relational shape under two
// different physical `created_at` orderings (which changes
// `duplicate_groups`' internal vector order, since that order follows
// each group's earliest-member `created_at`) and confirms the resulting
// graph is the same in both cases, proving the whole-run map's
// construction is genuinely order-independent rather than accidentally
// relying on processing group A before B before C. ──────────────────────
#[test]
fn three_group_reference_cycle_resolves_identically_under_two_processing_orders() {
    // Variant 1: groups created in A, B, C order (duplicate_groups will
    // be ordered A, B, C, since that's each group's earliest-created_at
    // order too).
    let store1 = open_store();
    let (survivor_a1, _) = store1
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_a1, _) = store1
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 110)
        .unwrap();
    let (survivor_b1, _) = store1
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_b1, _) = store1
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 210)
        .unwrap();
    let (survivor_c1, _) = store1
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 300)
        .unwrap();
    let (loser_c1, _) = store1
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 310)
        .unwrap();
    store1.set_superseded_by(survivor_a1, loser_b1).unwrap();
    store1.set_superseded_by(survivor_b1, loser_c1).unwrap();
    store1.set_superseded_by(survivor_c1, loser_a1).unwrap();

    let summary1 = store1.dedupe_entity_ids(false).unwrap();

    // Variant 2: same relational shape (A -> B's loser -> C's loser ->
    // A's loser), but groups are created in C, A, B physical order, so
    // `duplicate_groups`' earliest-created_at-derived vector order is
    // C, A, B instead of A, B, C.
    let store2 = open_store();
    let (survivor_c2, _) = store2
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_c2, _) = store2
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 110)
        .unwrap();
    let (survivor_a2, _) = store2
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_a2, _) = store2
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 210)
        .unwrap();
    let (survivor_b2, _) = store2
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 300)
        .unwrap();
    let (loser_b2, _) = store2
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 310)
        .unwrap();
    store2.set_superseded_by(survivor_a2, loser_b2).unwrap();
    store2.set_superseded_by(survivor_b2, loser_c2).unwrap();
    store2.set_superseded_by(survivor_c2, loser_a2).unwrap();

    let summary2 = store2.dedupe_entity_ids(false).unwrap();

    assert_eq!(
        summary1.rows_collapsed, summary2.rows_collapsed,
        "same relational shape must collapse the same number of rows \
         regardless of which group's earliest member happens to sort \
         first"
    );
    assert_eq!(summary1.rows_collapsed, 3);

    // Same relational outcome under both physical orderings: each
    // survivor ends up pointing at the *next* group's survivor around
    // the cycle, in both variants.
    let a1 = store1.get(survivor_a1).unwrap().unwrap();
    let b1 = store1.get(survivor_b1).unwrap().unwrap();
    let c1 = store1.get(survivor_c1).unwrap().unwrap();
    assert_eq!(a1.superseded_by, Some(survivor_b1));
    assert_eq!(b1.superseded_by, Some(survivor_c1));
    assert_eq!(c1.superseded_by, Some(survivor_a1));

    let a2 = store2.get(survivor_a2).unwrap().unwrap();
    let b2 = store2.get(survivor_b2).unwrap().unwrap();
    let c2 = store2.get(survivor_c2).unwrap().unwrap();
    assert_eq!(
        a2.superseded_by,
        Some(survivor_b2),
        "identical relational outcome under the C, A, B physical/vector order"
    );
    assert_eq!(b2.superseded_by, Some(survivor_c2));
    assert_eq!(c2.superseded_by, Some(survivor_a2));
}

// ── Adversarial (round-5 re-verification): hand-derive every summary
// count for a constructed multi-group scenario and assert the reported
// numbers match the hand count exactly, not just "no crash" / "no error".
// Scenario, worked by hand before running:
//   - group A: survivor_a, loser_a1, loser_a2. loser_a1 points at loser_a2
//     (a same-group loser->loser reference: inert clean-up, must be
//     cleared but NOT counted in supersede_edges_repointed). survivor_a's
//     own field points at loser_c1 (a DIFFERENT group's loser): resolves
//     to survivor_c, not a self-edge-drop (target != survivor_a), and not
//     counted in supersede_edges_repointed either (that counter only
//     covers rewrite_cross_references's *non-survivor* rows, per the
//     module's own doc — the survivor's own adoption is deliberately a
//     separate mechanism from the "elsewhere in the table" rewrite).
//   - group B: survivor_b, loser_b1. Nothing points at anything.
//   - group C: survivor_c, loser_c1. Nothing points at anything.
//   - ordinary (no group at all): points at loser_b1 -> resolves to
//     survivor_b, IS counted (genuinely external, non-same-group).
// Hand count: total_notes = 8, duplicate_groups = 3, rows_collapsed =
// 2 (group A) + 1 (group B) + 1 (group C) = 4, tags_merged = 0,
// linked_files_merged = 0, supersede_edges_repointed = 1 (only
// `ordinary`'s edge; loser_a1's same-group edge to loser_a2 is excluded),
// supersede_self_edges_dropped = 0 (survivor_a's own field resolves to a
// genuine external target, survivor_c, not to itself). ─────────────────
#[test]
fn hand_derived_summary_counts_match_multi_group_scenario_exactly() {
    let store = open_store();
    let (survivor_a, _) = store
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_a1, _) = store
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 110)
        .unwrap();
    let (loser_a2, _) = store
        .add_note_with_created_at("decision", "dup-a", "body", &[], &[], None, "active", 120)
        .unwrap();
    let (survivor_b, _) = store
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_b1, _) = store
        .add_note_with_created_at("decision", "dup-b", "body", &[], &[], None, "active", 210)
        .unwrap();
    let (survivor_c, _) = store
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 300)
        .unwrap();
    let (loser_c1, _) = store
        .add_note_with_created_at("decision", "dup-c", "body", &[], &[], None, "active", 310)
        .unwrap();
    let (ordinary, _) = store
        .add_note("note", "ordinary unique note", "body", &[], &[], None, None)
        .unwrap();

    store.set_superseded_by(loser_a1, loser_a2).unwrap();
    store.set_superseded_by(survivor_a, loser_c1).unwrap();
    store.set_superseded_by(ordinary, loser_b1).unwrap();

    let summary = store.dedupe_entity_ids(false).unwrap();

    assert_eq!(summary.total_notes, 8);
    assert_eq!(summary.duplicate_groups, 3);
    assert_eq!(summary.rows_collapsed, 4, "hand count: 2 + 1 + 1");
    assert_eq!(summary.tags_merged, 0);
    assert_eq!(summary.linked_files_merged, 0);
    assert_eq!(
        summary.supersede_edges_repointed, 1,
        "hand count: only `ordinary`'s edge; loser_a1's same-group edge \
         to loser_a2 must not be counted"
    );
    assert_eq!(
        summary.supersede_self_edges_dropped, 0,
        "hand count: survivor_a's own field resolves to a genuine \
         cross-group external target (survivor_c), not a self-edge"
    );

    // Cross-check the actual field values agree with the hand-derived
    // counts, not just the counts in isolation.
    let a = store.get(survivor_a).unwrap().unwrap();
    assert_eq!(a.superseded_by, Some(survivor_c));
    let b = store.get(survivor_b).unwrap().unwrap();
    assert_eq!(b.superseded_by, None);
    let c = store.get(survivor_c).unwrap().unwrap();
    assert_eq!(c.superseded_by, None);
    let o = store.get(ordinary).unwrap().unwrap();
    assert_eq!(o.superseded_by, Some(survivor_b));
    assert_eq!(note_count(&store), 4, "8 - 4 collapsed = 4 remaining rows");
}

// ── Test-engineer adversarial round: fold-group tie-breaking boundary ───
//
// The merge rule says "survivor = earliest created_at", but says nothing
// about what happens when two rows in a duplicate group share the exact
// same `created_at` (plausible for anything inserted in the same batch
// import or the same second via `add_note_with_created_at`, or even two
// ordinary `add_note` calls landing in the same unixepoch() second).
// `all_notes_for_dedup` orders `ORDER BY created_at ASC` with no
// secondary key, so a tie's resolution order is whatever SQLite's query
// plan happens to produce — not guaranteed stable by the SQL standard.
// Pin the id as an explicit secondary tie-break (lower id = inserted
// first = survivor) so the choice is a documented invariant instead of
// an accident of the query planner, and prove it holds across repeated
// runs on independently-built stores.
#[test]
fn tied_created_at_breaks_deterministically_on_lower_id() {
    for _ in 0..5 {
        let store = open_store();
        let (first, _) = store
            .add_note_with_created_at(
                "decision",
                "tied dup",
                "body",
                &[],
                &[],
                None,
                "active",
                500,
            )
            .unwrap();
        let (second, _) = store
            .add_note_with_created_at(
                "decision",
                "tied dup",
                "body",
                &[],
                &[],
                None,
                "active",
                500,
            )
            .unwrap();
        assert!(
            first < second,
            "precondition: insertion order gives first the lower id"
        );

        let summary = store.dedupe_entity_ids(false).unwrap();
        assert_eq!(summary.rows_collapsed, 1);
        assert!(
            store.get(first).unwrap().is_some(),
            "the lower-id (first-inserted) row must be the deterministic \
             survivor when created_at ties, on every run"
        );
        assert!(
            store.get(second).unwrap().is_none(),
            "the higher-id row must be the loser when created_at ties"
        );
    }
}

// ── Test-engineer adversarial round: dedupe interacting with rows built
// via `add_note_superseding` (ADR-068 fifth amendment E1), not just
// `add_note`/`add_note_with_created_at` as every prior dedupe.rs test
// does. Pre-promotion, two independent `add_note_superseding` calls for
// byte-identical content (each superseding a *different* OLD row) create
// two distinct rows sharing one entity_id — a duplicate group whose
// members both carry an *inbound* edge from an OLD row's own
// `superseded_by`, not an outbound one. Verifies `dedupe_entity_ids`
// repoints those inbound edges to the survivor exactly like any other
// external reference, proving the two features (E1's supersede path and
// the third amendment's dedupe) compose correctly rather than only ever
// having been tested in isolation.
#[test]
fn duplicate_group_built_via_add_note_superseding_repoints_old_rows_to_survivor() {
    let store = open_store();
    assert!(
        !index_is_unique_for_test(&store),
        "precondition: not yet promoted, so add_note_superseding can \
         still create two distinct rows for identical content"
    );

    let (old1, _) = store
        .add_note("decision", "old one", "old body one", &[], &[], None, None)
        .unwrap();
    let (old2, _) = store
        .add_note("decision", "old two", "old body two", &[], &[], None, None)
        .unwrap();

    let (new1, created1) = store
        .add_note_superseding(
            "decision",
            "dup replacement",
            "dup body",
            &[],
            &[],
            None,
            old1,
        )
        .unwrap();
    assert!(created1, "pre-promotion: fresh row");
    let (new2, created2) = store
        .add_note_superseding(
            "decision",
            "dup replacement",
            "dup body",
            &[],
            &[],
            None,
            old2,
        )
        .unwrap();
    assert!(
        created2,
        "pre-promotion: identical content still creates a second, \
         distinct row (the duplicate-group precondition for this test)"
    );
    assert_ne!(new1, new2);

    // Precondition: each OLD row now points at its own distinct
    // successor — new1 and new2, which are themselves a duplicate group.
    assert_eq!(store.get(old1).unwrap().unwrap().superseded_by, Some(new1));
    assert_eq!(store.get(old2).unwrap().unwrap().superseded_by, Some(new2));

    let summary = store.dedupe_entity_ids(false).unwrap();
    assert_eq!(summary.duplicate_groups, 1, "new1/new2 form one group");
    assert_eq!(summary.rows_collapsed, 1);

    // new1 (earlier created_at) survives; new2 is the loser.
    assert!(store.get(new1).unwrap().is_some());
    assert!(store.get(new2).unwrap().is_none());

    // old2's superseded_by, which pointed at the now-deleted new2, must
    // be repointed to the survivor new1 — this is the cross-reference
    // rewrite exercising an edge that originated from the supersede path
    // rather than a plain add_note/set_superseded_by call.
    assert_eq!(
        store.get(old2).unwrap().unwrap().superseded_by,
        Some(new1),
        "old2's supersede edge, created by add_note_superseding, must be \
         repointed off the deleted duplicate onto the survivor"
    );
    assert_eq!(
        store.get(old1).unwrap().unwrap().superseded_by,
        Some(new1),
        "old1's own edge already pointed at the survivor and must be \
         unaffected"
    );
}

// ── Test-engineer adversarial round: Step A's collision-skip
// (entity_id_migration.rs) leaves a colliding row's
// `entity_id` column NULL rather than erroring. `dedupe_entity_ids`
// itself never reads that stored column — `note_entity_id` recomputes
// fresh from `{kind,title,body}` on every call (see `entity_id.rs`) — so
// a row Step A was forced to skip is still discoverable and collapsible
// by `spelunk memory dedupe`, exactly the recovery path the fifth
// amendment's warning message ("run `spelunk memory dedupe` to collapse
// them") promises. This proves that promise end to end rather than
// trusting the two mechanisms compose from reading each in isolation:
// the skipped row is also, simultaneously, the target of an unrelated
// row's `superseded_by` — the exact "a row that's simultaneously a
// supersede target" scenario Step A's hardening was written to survive.
#[test]
fn step_a_skipped_row_that_is_a_supersede_target_is_still_collapsed_by_dedupe() {
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
    assert!(
        index_is_unique_for_test(&store),
        "precondition: index promoted"
    );

    // A stray NULL-entity_id row colliding with `existing_id` (simulating
    // a pre-E1 add_note_superseding row, or any other latent NULL-id
    // insert path) — bypasses add_note's own recovery entirely.
    store
        .conn
        .execute(
            "INSERT INTO notes (kind, title, body) VALUES ('decision', 'dup entry', 'same content')",
            [],
        )
        .unwrap();
    let stray_id = store.conn.last_insert_rowid();

    // A third, unrelated row whose superseded_by points AT the stray
    // row: the stray is simultaneously a supersede target.
    let (pointer_id, _) = store
        .add_note("note", "points at stray", "b", &[], &[], None, None)
        .unwrap();
    store.set_superseded_by(pointer_id, stray_id).unwrap();

    // Step A must skip the stray row without error.
    store
        .backfill_entity_ids()
        .expect("Step A must not hard-fail on the collision");
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
        "precondition: Step A left the colliding row's entity_id NULL"
    );

    // `spelunk memory dedupe` must still find and collapse it, even
    // though the stored `entity_id` column never got populated: it
    // recomputes entity_id from {kind,title,body}, not from the column.
    let summary = store.dedupe_entity_ids(false).unwrap();
    assert_eq!(
        summary.duplicate_groups, 1,
        "dedupe must discover the group despite the stray row's NULL \
         stored entity_id column"
    );
    assert_eq!(summary.rows_collapsed, 1);
    assert!(
        store.get(existing_id).unwrap().is_some(),
        "existing_id (earlier-created) survives"
    );
    assert!(
        store.get(stray_id).unwrap().is_none(),
        "the stray row is collapsed away"
    );
    assert_eq!(
        store.get(pointer_id).unwrap().unwrap().superseded_by,
        Some(existing_id),
        "pointer_id's edge to the now-deleted stray row must be \
         repointed to the survivor — proving the promised \
         Step-A-skip-then-dedupe recovery path actually closes the loop"
    );
}

/// Test-only helper mirroring `entity_id_migration.rs`'s private
/// `index_is_unique`, needed here since this module's tests also drive
/// `promote_entity_id_unique_index` directly.
fn index_is_unique_for_test(store: &MemoryStore) -> bool {
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
