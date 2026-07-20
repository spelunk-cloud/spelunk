//! ADR-068 fourth amendment coverage: `add_note`'s insert-then-recover
//! behavior once `idx_notes_entity_id` is promoted to UNIQUE.

use super::test_support::*;

// Adversarial (QA final review): the public `add_note` API, not the raw
// SQL layer, hitting the promoted index with duplicate content.
//
// A raw duplicate INSERT should be rejected at the SQL level once the
// index is promoted (the previous test proves that; that's the index
// doing its job). But nothing in the codebase catches that rejection
// before it reaches a real caller: `MemoryStore::add_note` (used
// directly by `spelunk memory add`, the most common write path) performs
// a bare INSERT with no pre-check and no error handling for it.
//
// Once Step B promotes `idx_notes_entity_id` to UNIQUE (per AC5, on the
// very next open of any store with zero duplicate groups, the
// overwhelmingly common steady state), the next `spelunk memory add` (or
// a harvest/reconcile retry) submitting byte-identical kind/title/body
// content hard-fails with a raw SQLite error surfaced to the user
// ("Error: UNIQUE constraint failed: notes.entity_id"), reproduced
// directly against a real built binary.
//
// This contradicts the third amendment's own decision text: recording
// byte-identical kind/title/body twice "yields one entry" and "is
// accepted". None of the 24 acceptance criteria or five rounds of
// adversarial testing exercised this: every round was scoped to
// `dedupe.rs`'s own collapse (DELETE) path, never the ordinary INSERT
// colliding with the index `entity_id_migration.rs` newly promotes.
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
         very next open of any store with zero duplicate groups, the \
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
// still exist), identical content must keep inserting distinct rows:
// the very mechanism dedupe.rs's own fixtures rely on to build
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

// ── Criterion 28: any error other than the specific notes.entity_id
// UNIQUE violation must propagate unchanged, not be swallowed by the
// collision-recovery match arm. Exercised via a synthetic trigger that
// raises a distinct error for a specific title, so the failure is
// unambiguously NOT a UNIQUE-on-entity_id violation.
#[test]
fn add_note_other_error_propagates_unchanged_not_swallowed_as_collision() {
    let store = open_store();
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER reject_specific_title
             BEFORE INSERT ON notes
             WHEN NEW.title = 'trigger-reject'
             BEGIN SELECT RAISE(ABORT, 'synthetic non-unique failure'); END;",
        )
        .unwrap();

    let result = store.add_note("note", "trigger-reject", "body", &[], &[], None, None);
    assert!(
        result.is_err(),
        "criterion 28: a non-UNIQUE error must propagate, not be swallowed"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("synthetic non-unique failure"),
        "expected the synthetic trigger error to propagate verbatim, got: {msg}"
    );
    let total_rows: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total_rows, 0, "a failed insert must leave no row behind");
}
