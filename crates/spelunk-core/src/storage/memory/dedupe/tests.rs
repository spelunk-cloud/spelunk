//! Core `dedupe_entity_ids` behavior: happy path, dry-run, tags/linked_files
//! merge, archived/superseded_by merge, rollback, and basic edge handling.
//! See `superseded_by_tests` for the intra- and cross-group `superseded_by`
//! reference-resolution edge cases.

use super::test_support::*;
use super::*;

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
    let (survivor, _) = store
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
    let (_loser, _) = store
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
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser, _) = store
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
    let (survivor, _) = store
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
    let (survivor, _) = store
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
    let (survivor, _) = store
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
    let (elsewhere, _) = store
        .add_note("note", "elsewhere target", "b", &[], &[], None, None)
        .unwrap();
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser, _) = store
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
    let (target_a, _) = store
        .add_note("note", "a", "b", &[], &[], None, None)
        .unwrap();
    let (target_b, _) = store
        .add_note("note", "b", "b", &[], &[], None, None)
        .unwrap();

    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser, _) = store
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
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (dependent, _) = store
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
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser, _) = store
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
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser, _) = store
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

// Adversarial: fault mid-group (after a loser is deleted, before the
// next loser in the same group is touched), checked byte-for-byte across
// every table dedupe can touch, not just a row count. AC21's own test
// only proves rollback at a group boundary; this proves it holds mid-group.
#[test]
fn injected_fault_mid_group_after_partial_loser_deletion_rolls_back_byte_for_byte() {
    let store = open_store();
    let (survivor, _) = store
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
    let (loser_a, _) = store
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
    let (loser_b, _) = store
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
    let (external, _) = store
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
    let (survivor, _) = store
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
    let (loser, _) = store
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
    let (external, _) = store
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
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser_a, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (loser_b, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 300)
        .unwrap();
    let (dep_a, _) = store
        .add_note("note", "points at loser_a", "b", &[], &[], None, None)
        .unwrap();
    let (dep_b, _) = store
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
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (other_a, _) = store
        .add_note("note", "a", "b", &[], &[], None, None)
        .unwrap();
    let (other_b, _) = store
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

// Adversarial: relates_to/contradicts edges to a loser are dropped by
// delete_edges_for_note, not repointed, unlike tags/linked_files/
// superseded_by. ADR-068's third amendment only specifies a merge rule
// for superseded_by, not the memory_edges graph, so this pins the
// current (lossy) behavior as a known gap rather than a spec violation.
#[test]
fn relates_to_edge_to_external_note_is_dropped_not_repointed_known_gap() {
    let store = open_store();
    let (survivor, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 100)
        .unwrap();
    let (loser, _) = store
        .add_note_with_created_at("decision", "dup", "body", &[], &[], None, "active", 200)
        .unwrap();
    let (external, _) = store
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
