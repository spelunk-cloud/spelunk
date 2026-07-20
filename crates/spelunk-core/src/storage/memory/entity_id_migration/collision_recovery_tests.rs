//! ADR-068 fourth and fifth amendment coverage: `add_note`/
//! `add_note_superseding` insert-then-recover behavior once
//! `idx_notes_entity_id` is promoted to UNIQUE, and Step A's own
//! per-row collision skip.

use super::test_support::*;

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
// idempotent-ish outcome, not an unhandled crash. None of this story's
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

// ── Test-engineer adversarial round: self-supersede collision ───────────
//
// `add_note_superseding`'s collision-recovery path (criterion 3) was only
// ever exercised with the colliding "existing" row being a THIRD row,
// distinct from both the freshly-superseded OLD row and the new content.
// Nothing stops a caller from superseding `old_id` with content that is
// byte-identical to `old_id`'s OWN `{kind,title,body}` — e.g. `spelunk
// memory add --supersedes <id>` invoked with the same title/body as the
// entry it names. Post-promotion, the INSERT then collides with `old_id`
// itself: `recover_from_entity_id_collision` looks up the existing row by
// `entity_id` and finds `old_id`, so `existing_id == supersedes_id`. The
// archive-OLD UPDATE then runs `SET status='archived', superseded_by=?2
// WHERE id=?1` with `?1 = ?2 = old_id`: a row would archive itself and
// set its own `superseded_by` to its own `id`, a self-loop of exactly the
// shape `dedupe.rs`'s own self-edge guard exists to prevent, but on the
// supersede path rather than the collapse path.
#[test]
fn add_note_superseding_self_collision_does_not_create_self_referential_archived_row() {
    let store = open_store();
    let (old_id, _) = store
        .add_note(
            "decision",
            "same content",
            "same body",
            &[],
            &[],
            None,
            None,
        )
        .unwrap();
    store.promote_entity_id_unique_index().unwrap();
    assert!(index_is_unique(&store), "precondition: index promoted");

    // Supersede `old_id` with content identical to `old_id`'s own
    // kind/title/body: the INSERT collides with `old_id` itself.
    let result = store.add_note_superseding(
        "decision",
        "same content",
        "same body",
        &[],
        &[],
        None,
        old_id,
    );
    assert!(
        result.is_ok(),
        "a self-collision must not error: {:?}",
        result.err()
    );
    let (returned_id, created) = result.unwrap();
    assert_eq!(
        returned_id, old_id,
        "the collision resolves to old_id itself — there is only one row \
         with this content"
    );
    assert!(!created, "a collision is never a fresh insert");

    let row = store.get(old_id).unwrap().expect("row still exists");
    assert_ne!(
        row.superseded_by,
        Some(old_id),
        "BUG: a self-supersede collision must not leave a row pointing \
         superseded_by at its own id — this is a self-loop of the exact \
         shape dedupe.rs's own self-edge guard exists to prevent, just \
         reached via the supersede path instead of the collapse path"
    );
    let total_rows: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        total_rows, 1,
        "no phantom second row must be created by the self-collision"
    );
}

// ── Step A: any error other than a UNIQUE violation from the per-row
// backfill UPDATE must propagate unchanged. Exercised via a synthetic
// trigger on UPDATE, distinct from the UNIQUE-collision path already
// covered above.
#[test]
fn backfill_other_error_from_update_propagates_unchanged() {
    let store = open_store();
    store
        .conn
        .execute(
            "INSERT INTO notes (kind, title, body, entity_id) VALUES ('note', 'stray', 'body', NULL)",
            [],
        )
        .unwrap();
    let stray_id = store.conn.last_insert_rowid();
    store
        .conn
        .execute_batch(&format!(
            "CREATE TRIGGER reject_specific_update
             BEFORE UPDATE ON notes
             WHEN NEW.id = {stray_id}
             BEGIN SELECT RAISE(ABORT, 'synthetic step a failure'); END;"
        ))
        .unwrap();

    let result = store.backfill_entity_ids();
    assert!(
        result.is_err(),
        "criterion 9: a non-UNIQUE error from the backfill UPDATE must propagate"
    );
    let err = result.unwrap_err();
    // `.to_string()` on an anyhow::Error only shows the outermost
    // `.with_context(...)` layer ("backfilling entity_id for note #1");
    // the synthetic trigger message is a wrapped source, so it must be
    // checked via the full chain, not the top-level Display.
    let full_chain: String = err
        .chain()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        full_chain.contains("synthetic step a failure"),
        "expected the synthetic trigger error to propagate somewhere in \
         the error chain, got: {full_chain}"
    );
}
