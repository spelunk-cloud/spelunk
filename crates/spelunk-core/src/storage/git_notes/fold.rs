//! Collapse the copies of one entity that `refs/notes/spelunk` accumulates.
//!
//! The ref is an append-only, entity-keyed event log: two machines recording
//! the same decision write lines with the same `entity_id` but their own `id`
//! and `created_at`, and `cat_sort_uniq` keeps both. Readers fold those copies
//! into one entry; writers never mutate in place.
//!
//! Every rule here is commutative, associative and idempotent, so any merge
//! order across any number of machines converges (ADR-068 A6) — with one
//! narrower exception: `superseded_by_entity_id` (ADR-068 E5) resolves via a
//! whole-group scan over every copy's `created_at`, rather than the pairwise
//! fold every other field uses, so it is commutative within one
//! `fold_records` call (any order of the same input converges) but not
//! associative across a partial pre-fold. That is safe because `fold_records`
//! is only ever invoked once, over the complete set of records read off the
//! ref (`GitNotesBackend::collect`), never incrementally.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use super::super::note_record::NoteRecord;

/// Collapse each `entity_id` group in `records` into exactly one record.
///
/// Groups are keyed on `resolve_entity_id()`, never the raw field: a legacy
/// line predating `entity_id` recomputes it from `{kind, title, body}` and so
/// folds together with a fresh line for the same entry.
///
/// Returns one record per entity in first-encounter order, which a later stable
/// sort turns into "`created_at` ties keep blob order" (ADR-069 D2).
pub(super) fn fold_records(records: Vec<NoteRecord>) -> Vec<NoteRecord> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<NoteRecord>> = HashMap::new();

    for record in records {
        let key = record.resolve_entity_id();
        match groups.entry(key) {
            Entry::Vacant(slot) => {
                order.push(slot.key().clone());
                slot.insert(vec![record]);
            }
            Entry::Occupied(mut slot) => slot.get_mut().push(record),
        }
    }

    order
        .into_iter()
        .filter_map(|key| groups.remove(&key).map(fold_group))
        .collect()
}

/// Fold one entity's copies onto its base copy.
fn fold_group(mut group: Vec<NoteRecord>) -> NoteRecord {
    // Resolved over the whole group before `base` is picked/consumed below
    // (ADR-068 E5): `base` is the *earliest*-created copy, not the most
    // recent, so `superseded_by_entity_id` cannot be folded by the same
    // pairwise base-vs-other rule the other fields use.
    let superseded_by_entity_id = resolve_superseded_by_entity_id(&group);

    let base_idx = base_index(&group);
    let mut base = group.swap_remove(base_idx);
    for other in group {
        merge_into(&mut base, other);
    }
    base.superseded_by_entity_id = superseded_by_entity_id;
    base
}

/// Resolve a fold group's `superseded_by_entity_id`: the value carried by
/// whichever record has the greatest `created_at` among those where the
/// field is non-`None` — "most recent write wins", not the lexicographically
/// smallest value `min_some` would pick (ADR-068 E5). Ties on `created_at`
/// defer to `base_key`'s own ascending tie-break (id, then the remaining
/// fields), the same convention `base_index` uses, so the pick stays
/// deterministic even when two independent machines' rows collide on `id`
/// (e.g. each starting its own SQLite sequence from 1). A final fallback on
/// the value itself keeps this a total order even in the degenerate case of
/// two records tying on every `base_key` field too (never happens with real
/// distinct writes, but keeps the fold provably order-independent).
fn resolve_superseded_by_entity_id(group: &[NoteRecord]) -> Option<String> {
    group
        .iter()
        .filter(|r| r.superseded_by_entity_id.is_some())
        .min_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| base_key(a).cmp(&base_key(b)))
                .then_with(|| a.superseded_by_entity_id.cmp(&b.superseded_by_entity_id))
        })
        .and_then(|r| r.superseded_by_entity_id.clone())
}

/// Index of the copy every base-sourced field is taken from.
///
/// Total by construction: two copies this cannot separate agree on every field
/// it sources, so which one wins cannot change the fold's output.
fn base_index(group: &[NoteRecord]) -> usize {
    group
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| base_key(a).cmp(&base_key(b)))
        .map(|(i, _)| i)
        .expect("a group always holds the record that created it")
}

/// Earliest recording wins; the trailing fields only break a tie.
fn base_key(r: &NoteRecord) -> (i64, i64, Option<&String>, Option<&String>, Option<i64>) {
    (
        r.created_at,
        r.id,
        r.source_ref.as_ref(),
        r.remote_id.as_ref(),
        r.superseded_by,
    )
}

/// `kind`/`title`/`body` are equal by construction (they are the identity) and
/// `created_at` is already the minimum, since `base_key` orders on it first.
///
/// `superseded_by_entity_id` is deliberately not folded here — `fold_group`
/// resolves it separately, over the whole group, via
/// `resolve_superseded_by_entity_id` (ADR-068 E5).
fn merge_into(base: &mut NoteRecord, other: NoteRecord) {
    // Archival is monotonic: never un-archive (`apply_remote_note`).
    if other.status == "archived" {
        base.status = "archived".to_string();
    }
    base.schema_version = base.schema_version.max(other.schema_version);
    base.valid_at = min_some(base.valid_at, other.valid_at);
    base.invalid_at = min_some(base.invalid_at, other.invalid_at);
    union_into(&mut base.tags, other.tags);
    union_into(&mut base.linked_files, other.linked_files);
}

/// Keep the smallest present value. `None` is the identity.
fn min_some<T: Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Add-wins OR-Set union: an incoming member is added, none is ever dropped.
fn union_into(base: &mut Vec<String>, other: Vec<String>) {
    base.extend(other);
    base.sort();
    base.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::entity_id::entity_id;

    /// Two copies of one decision, as two machines would write them.
    pub(super) fn copy(id: i64, created_at: i64, tags: &[&str]) -> NoteRecord {
        NoteRecord {
            schema_version: 1,
            id,
            kind: "decision".to_string(),
            title: "HTTP layer".to_string(),
            body: "use axum".to_string(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            linked_files: vec![],
            created_at,
            status: "active".to_string(),
            source_ref: None,
            valid_at: None,
            invalid_at: None,
            superseded_by: None,
            remote_id: None,
            entity_id: Some(entity_id("decision", "HTTP layer", "use axum")),
            superseded_by_entity_id: None,
        }
    }

    /// One copy per machine, each differing in every foldable field.
    fn copies() -> Vec<NoteRecord> {
        let mut archived = copy(3, 300, &["c"]);
        archived.status = "archived".to_string();
        archived.superseded_by_entity_id = Some("ffff".to_string());

        let mut middle = copy(2, 200, &["b"]);
        middle.valid_at = Some(50);
        middle.superseded_by_entity_id = Some("aaaa".to_string());

        let mut earliest = copy(1, 100, &["a"]);
        earliest.invalid_at = Some(900);

        vec![earliest, middle, archived]
    }

    /// A4/A6's stated standard: every rule is order-insensitive, so any merge
    /// order across any number of machines reaches the same entry.
    #[test]
    fn folding_converges_regardless_of_order() {
        let fingerprint = |group: Vec<NoteRecord>| {
            let folded = fold_records(group);
            assert_eq!(folded.len(), 1, "one entity, one entry");
            serde_json::to_string(&folded[0]).expect("serialize")
        };

        // Every permutation of three copies, plus a duplicated input to pin
        // idempotence (folding a copy twice changes nothing).
        let expected = fingerprint(copies());
        for (i, j, k) in [
            (0, 1, 2),
            (0, 2, 1),
            (1, 0, 2),
            (1, 2, 0),
            (2, 0, 1),
            (2, 1, 0),
        ] {
            let c = copies();
            let permuted = vec![copies_at(&c, i), copies_at(&c, j), copies_at(&c, k)];
            assert_eq!(
                fingerprint(permuted),
                expected,
                "permutation ({i},{j},{k}) must converge"
            );
        }

        let mut twice = copies();
        twice.extend(copies());
        assert_eq!(fingerprint(twice), expected, "folding is idempotent");
    }

    /// `NoteRecord` is not `Clone`; rebuild the i-th copy instead.
    fn copies_at(src: &[NoteRecord], i: usize) -> NoteRecord {
        let want = &src[i];
        let mut r = copy(want.id, want.created_at, &[]);
        r.tags = want.tags.clone();
        r.status = want.status.clone();
        r.valid_at = want.valid_at;
        r.invalid_at = want.invalid_at;
        r.superseded_by_entity_id = want.superseded_by_entity_id.clone();
        r
    }

    /// The fold collapses copies, never distinct entries.
    #[test]
    fn distinct_entities_are_not_collapsed() {
        let mut other = copy(1, 100, &[]);
        other.title = "storage layer".to_string();
        other.entity_id = Some(entity_id("decision", "storage layer", "use axum"));

        assert_eq!(fold_records(vec![copy(1, 100, &[]), other]).len(), 2);
    }

    /// The one entry a group of copies folds to, serialized for comparison.
    fn fingerprint(records: Vec<NoteRecord>) -> String {
        let folded = fold_records(records);
        assert_eq!(folded.len(), 1, "the fixture must be one entity's copies");
        serde_json::to_string(&folded[0]).expect("serialize")
    }

    /// Three copies tying on `(created_at, id)` and differing only in the
    /// remaining fields `base` supplies.
    fn tied_variants() -> Vec<NoteRecord> {
        let mut high = copy(1, 100, &[]);
        high.source_ref = Some("bbb".to_string());
        high.remote_id = Some("zzz".to_string());
        high.superseded_by = Some(9);

        let mut low = copy(1, 100, &[]);
        low.source_ref = Some("aaa".to_string());
        low.remote_id = Some("yyy".to_string());
        low.superseded_by = Some(2);

        // Separated from `low` only by the last component of the key.
        let mut mid = copy(1, 100, &[]);
        mid.source_ref = Some("aaa".to_string());
        mid.remote_id = Some("yyy".to_string());
        mid.superseded_by = Some(7);

        vec![high, low, mid]
    }

    /// `NoteRecord` is not `Clone`; rebuild the group in the given order.
    fn tied_in_order(idx: [usize; 3]) -> Vec<NoteRecord> {
        let mut src: Vec<Option<NoteRecord>> = tied_variants().into_iter().map(Some).collect();
        idx.iter()
            .map(|&i| src[i].take().expect("each index taken once"))
            .collect()
    }

    /// `(created_at, id)` alone is not total: two copies can tie there and still
    /// differ in the other fields `base` supplies, which would leave the fold
    /// order-dependent. Ordering those too is what keeps it convergent.
    #[test]
    fn base_is_deterministic_when_created_at_and_id_tie() {
        let expected = fingerprint(tied_in_order([0, 1, 2]));

        for idx in [[0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            assert_eq!(
                fingerprint(tied_in_order(idx)),
                expected,
                "copies tying on (created_at, id) must still fold identically in order {idx:?}"
            );
        }

        let folded = fold_records(tied_in_order([2, 0, 1]));
        assert_eq!(
            (
                folded[0].source_ref.as_deref(),
                folded[0].remote_id.as_deref(),
                folded[0].superseded_by
            ),
            (Some("aaa"), Some("yyy"), Some(2)),
            "the smallest copy on the full key supplies every base field"
        );
    }

    // ── ADR-068 amendment E5: superseded_by_entity_id resolves by recency ────

    /// A conflicting `superseded_by_entity_id` (e.g. from a lost cross-machine
    /// race double-superseding the same OLD by two different successors) must
    /// resolve to the value on the record with the greatest `created_at`, not
    /// the lexicographically smallest value `min_some` used to pick.
    #[test]
    fn superseded_by_entity_id_resolves_to_latest_created_at_not_lexicographic_min() {
        let mut earlier = copy(1, 100, &[]);
        earlier.status = "archived".to_string();
        earlier.superseded_by_entity_id = Some("aaaa".to_string()); // lex-smallest, but older

        let mut later = copy(2, 200, &[]);
        later.status = "archived".to_string();
        later.superseded_by_entity_id = Some("zzzz".to_string()); // lex-largest, but newer

        let folded = fold_records(vec![earlier, later]);
        assert_eq!(folded.len(), 1, "one entity, one entry");
        assert_eq!(
            folded[0].superseded_by_entity_id.as_deref(),
            Some("zzzz"),
            "must resolve to the later-created_at record's successor, even though its \
             entity_id string sorts lexicographically larger — the old `min_some` rule \
             would wrongly pick \"aaaa\" here"
        );
    }

    /// Two conflicting records tying on `created_at` resolve by `id` ascending —
    /// the same tie-break order `base_key` already uses. Checked in both input
    /// orders: the result must not depend on which one arrives first.
    #[test]
    fn superseded_by_entity_id_tie_on_created_at_breaks_by_id_ascending() {
        // Deliberately the *opposite* of lexicographic order: the smaller id
        // (which must win) carries the lexicographically larger value, so a
        // leftover `min_some` comparison would pick the wrong (higher-id)
        // record's value instead.
        fn tied_pair() -> (NoteRecord, NoteRecord) {
            let mut lower_id = copy(1, 100, &[]);
            lower_id.status = "archived".to_string();
            lower_id.superseded_by_entity_id = Some("zzzz".to_string());

            let mut higher_id = copy(2, 100, &[]);
            higher_id.status = "archived".to_string();
            higher_id.superseded_by_entity_id = Some("aaaa".to_string());

            (lower_id, higher_id)
        }

        let (lower_id, higher_id) = tied_pair();
        let folded = fold_records(vec![higher_id, lower_id]);
        assert_eq!(folded.len(), 1);
        assert_eq!(
            folded[0].superseded_by_entity_id.as_deref(),
            Some("zzzz"),
            "created_at ties must resolve to the smaller id's record, matching \
             base_key's tie-break — not the lexicographically smaller value"
        );

        let (lower_id, higher_id) = tied_pair();
        let folded = fold_records(vec![lower_id, higher_id]);
        assert_eq!(folded.len(), 1);
        assert_eq!(
            folded[0].superseded_by_entity_id.as_deref(),
            Some("zzzz"),
            "must resolve the same way regardless of input order"
        );
    }

    /// Three machines, not just two, independently double(triple)-superseding
    /// the same OLD with three different successors: the fold must scan the
    /// *whole* group and pick the true maximum, not just win a pairwise
    /// comparison. Deliberately unordered input (the true max is in the
    /// middle) so an accidental "last one wins" or "first beats second, stop"
    /// implementation would fail this.
    #[test]
    fn superseded_by_entity_id_three_way_conflict_resolves_to_latest_created_at() {
        let mut low = copy(1, 100, &[]);
        low.status = "archived".to_string();
        low.superseded_by_entity_id = Some("aaaa".to_string());

        let mut highest = copy(2, 300, &[]);
        highest.status = "archived".to_string();
        highest.superseded_by_entity_id = Some("mmmm".to_string());

        let mut mid = copy(3, 200, &[]);
        mid.status = "archived".to_string();
        mid.superseded_by_entity_id = Some("zzzz".to_string());

        // `highest` (created_at 300) is neither first nor last in the input,
        // and its value ("mmmm") is neither the lexicographic min nor max —
        // only a genuine whole-group scan by `created_at` picks it.
        let folded = fold_records(vec![low, highest, mid]);
        assert_eq!(folded.len(), 1, "one entity, one entry");
        assert_eq!(
            folded[0].superseded_by_entity_id.as_deref(),
            Some("mmmm"),
            "must resolve to the record with the greatest created_at among all \
             three conflicting copies, regardless of input order or lexicographic \
             value"
        );
    }

    /// Three conflicting records all tying on `created_at` — not a corner
    /// case: `NoteRecord::created_at` is second-granularity
    /// (`now_secs`), so three near-simultaneous writes/races commonly land on
    /// the same second in practice. All three must resolve by `id` ascending,
    /// regardless of input order.
    #[test]
    fn superseded_by_entity_id_three_way_tie_on_created_at_breaks_by_id_ascending() {
        fn tied_triple() -> Vec<NoteRecord> {
            let mut id3 = copy(3, 100, &[]);
            id3.status = "archived".to_string();
            id3.superseded_by_entity_id = Some("aaaa".to_string()); // lex-smallest, highest id

            let mut id1 = copy(1, 100, &[]);
            id1.status = "archived".to_string();
            id1.superseded_by_entity_id = Some("zzzz".to_string()); // lex-largest, lowest id — must win

            let mut id2 = copy(2, 100, &[]);
            id2.status = "archived".to_string();
            id2.superseded_by_entity_id = Some("mmmm".to_string());

            vec![id3, id1, id2]
        }

        for perm in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let triple = tied_triple();
            let mut src: Vec<Option<NoteRecord>> = triple.into_iter().map(Some).collect();
            let ordered: Vec<NoteRecord> = perm
                .iter()
                .map(|&i| src[i].take().expect("each index taken once"))
                .collect();
            let folded = fold_records(ordered);
            assert_eq!(folded.len(), 1);
            assert_eq!(
                folded[0].superseded_by_entity_id.as_deref(),
                Some("zzzz"),
                "all three tie on created_at, so the lowest id (1) must win \
                 regardless of input order {perm:?}, not the lexicographically \
                 smallest or largest value"
            );
        }
    }

    /// Regression guard: E5 is scoped to `superseded_by_entity_id` only —
    /// `valid_at`/`invalid_at` folding is untouched, still resolving via
    /// `min_some` (earliest wins), a different semantic (a temporal validity
    /// window, not a conflicting-successor pointer).
    #[test]
    fn valid_at_and_invalid_at_still_fold_via_min_some() {
        let mut a = copy(1, 100, &[]);
        a.valid_at = Some(50);
        a.invalid_at = Some(900);

        let mut b = copy(2, 200, &[]);
        b.valid_at = Some(10);
        b.invalid_at = Some(500);

        let folded = fold_records(vec![a, b]);
        assert_eq!(folded.len(), 1);
        assert_eq!(
            folded[0].valid_at,
            Some(10),
            "valid_at must still take the min"
        );
        assert_eq!(
            folded[0].invalid_at,
            Some(500),
            "invalid_at must still take the min"
        );
    }
}

/// Convergence, over generated copies rather than a fixed fixture.
///
/// A6's standard is that every rule is commutative, associative and idempotent.
/// The fixed-permutation test above pins one worked example; these pin the
/// property itself.
#[cfg(test)]
mod convergence {
    use super::tests::copy;
    use super::*;
    use proptest::prelude::*;

    /// The fields two machines' copies of one entity may legitimately differ in.
    #[derive(Debug, Clone)]
    struct Spec {
        id: i64,
        created_at: i64,
        archived: bool,
        tags: Vec<String>,
        linked_files: Vec<String>,
        valid_at: Option<i64>,
        invalid_at: Option<i64>,
        superseded_by_entity_id: Option<String>,
        source_ref: Option<String>,
        remote_id: Option<String>,
        superseded_by: Option<i64>,
    }

    fn build(s: &Spec) -> NoteRecord {
        let mut r = copy(s.id, s.created_at, &[]);
        r.tags = s.tags.clone();
        r.linked_files = s.linked_files.clone();
        r.status = if s.archived { "archived" } else { "active" }.to_string();
        r.valid_at = s.valid_at;
        r.invalid_at = s.invalid_at;
        r.superseded_by_entity_id = s.superseded_by_entity_id.clone();
        r.source_ref = s.source_ref.clone();
        r.remote_id = s.remote_id.clone();
        r.superseded_by = s.superseded_by;
        r
    }

    /// Ranges are tiny on purpose: ties are where an incomplete order would
    /// leak through, so the generator has to produce them often.
    fn arb_spec() -> impl Strategy<Value = Spec> {
        (
            (0i64..3, 0i64..3, any::<bool>()),
            (
                prop::collection::vec("[a-c]", 0..3),
                prop::collection::vec("[x-z]", 0..3),
            ),
            (prop::option::of(0i64..3), prop::option::of(0i64..3)),
            (
                prop::option::of("[a-c]"),
                prop::option::of("[a-c]"),
                prop::option::of("[a-c]"),
                prop::option::of(0i64..3),
            ),
        )
            .prop_map(
                |(
                    (id, created_at, archived),
                    (tags, linked_files),
                    (valid_at, invalid_at),
                    (superseded_by_entity_id, source_ref, remote_id, superseded_by),
                )| Spec {
                    id,
                    created_at,
                    archived,
                    tags,
                    linked_files,
                    valid_at,
                    invalid_at,
                    superseded_by_entity_id,
                    source_ref,
                    remote_id,
                    superseded_by,
                },
            )
    }

    fn every_order(specs: &[Spec]) -> Vec<Vec<Spec>> {
        if specs.len() <= 1 {
            return vec![specs.to_vec()];
        }
        let mut out = Vec::new();
        for i in 0..specs.len() {
            let mut rest = specs.to_vec();
            let head = rest.remove(i);
            for mut order in every_order(&rest) {
                order.insert(0, head.clone());
                out.push(order);
            }
        }
        out
    }

    fn fold_of(specs: &[Spec]) -> String {
        entry(fold_records(specs.iter().map(build).collect()))
    }

    fn entry(folded: Vec<NoteRecord>) -> String {
        assert_eq!(folded.len(), 1, "every spec is a copy of one entity");
        serde_json::to_string(&folded[0]).expect("serialize")
    }

    /// Same as `entry`, but blanks `superseded_by_entity_id` first — see the
    /// comment on `folding_is_idempotent_and_associative`'s second half for
    /// why that one field is compared separately.
    fn entry_ignoring_successor(mut folded: Vec<NoteRecord>) -> String {
        assert_eq!(folded.len(), 1, "every spec is a copy of one entity");
        folded[0].superseded_by_entity_id = None;
        serde_json::to_string(&folded[0]).expect("serialize")
    }

    proptest! {
        /// Commutative: two machines holding the same copies in any order reach
        /// the same entry.
        #[test]
        fn folding_converges_over_every_order(specs in prop::collection::vec(arb_spec(), 1..6)) {
            let expected = fold_of(&specs);
            for order in every_order(&specs) {
                prop_assert_eq!(fold_of(&order), expected.clone(), "order {:?} diverged", order);
            }
        }

        /// Idempotent: re-reading a folded entry changes nothing. Associative:
        /// folding one machine's copies first reaches the same entry as folding
        /// the union in one pass — with one deliberate exception, scoped out
        /// below: `superseded_by_entity_id` (ADR-068 E5) resolves via a
        /// whole-group scan over every copy's `created_at`, not the pairwise
        /// `base`-vs-`other` fold every other field uses, so a partial
        /// pre-fold can discard the `created_at` context a later merge would
        /// need to re-resolve it correctly. This is safe in practice:
        /// `fold_records` is only ever called once, over every commit's
        /// records in a single pass (`GitNotesBackend::collect`'s doc
        /// comment: "the only site that can fold an entity's copies
        /// together") — never incrementally on a partial pre-fold.
        #[test]
        fn folding_is_idempotent_and_associative(
            left in prop::collection::vec(arb_spec(), 1..4),
            right in prop::collection::vec(arb_spec(), 1..4),
        ) {
            let records = |specs: &[Spec]| -> Vec<NoteRecord> { specs.iter().map(build).collect() };

            prop_assert_eq!(
                entry(fold_records(fold_records(records(&left)))),
                fold_of(&left),
                "fold(fold(x)) must equal fold(x)"
            );

            let mut union = records(&left);
            union.extend(records(&right));
            let whole = fold_records(union);

            let mut partial = fold_records(records(&left));
            partial.extend(records(&right));
            let partial_folded = fold_records(partial);

            prop_assert_eq!(
                entry_ignoring_successor(partial_folded),
                entry_ignoring_successor(whole),
                "partial fold diverged on a field other than superseded_by_entity_id"
            );
        }
    }
}
