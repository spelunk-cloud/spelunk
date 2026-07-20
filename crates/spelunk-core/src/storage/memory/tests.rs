use super::MemoryStore;
use std::sync::OnceLock;

/// Register the sqlite-vec extension exactly once per test process.
/// `MemoryStore::migrate()` creates a `vec0` virtual table, which
/// requires the extension to be loaded before any connection is opened.
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

fn open_store() -> MemoryStore {
    register_sqlite_vec();
    MemoryStore::open(std::path::Path::new(":memory:"))
        .expect("failed to open in-memory MemoryStore")
}

fn count_edges(store: &MemoryStore, from_id: i64, to_id: i64, kind: &str) -> i64 {
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM memory_edges WHERE from_id = ?1 AND to_id = ?2 AND kind = ?3",
            rusqlite::params![from_id, to_id, kind],
            |r| r.get(0),
        )
        .unwrap_or(0)
}

// ── supersede() ──────────────────────────────────────────────────────────────

#[test]
fn supersede_happy_path() {
    let store = open_store();

    let (old_id, _) = store
        .add_note("decision", "Old decision", "old body", &[], &[], None, None)
        .unwrap();
    let (new_id, _) = store
        .add_note("decision", "New decision", "new body", &[], &[], None, None)
        .unwrap();

    let changed = store.supersede(old_id, new_id).unwrap();
    assert!(changed, "supersede() should return true on first call");

    // (a) old note must be archived with superseded_by set
    let old_note = store.get(old_id).unwrap().expect("old note must exist");
    assert_eq!(old_note.status, "archived");
    assert_eq!(old_note.superseded_by, Some(new_id));

    // (b) a memory_edges row must exist linking new → old
    assert_eq!(
        count_edges(&store, new_id, old_id, "supersedes"),
        1,
        "expected exactly one supersedes edge"
    );
}

#[test]
fn supersede_idempotent() {
    let store = open_store();

    let (old_id, _) = store
        .add_note("note", "Alpha", "body", &[], &[], None, None)
        .unwrap();
    let (new_id, _) = store
        .add_note("note", "Beta", "body", &[], &[], None, None)
        .unwrap();

    let first = store.supersede(old_id, new_id).unwrap();
    assert!(first);

    // Second call on an already-archived note must return false
    let second = store.supersede(old_id, new_id).unwrap();
    assert!(
        !second,
        "supersede() should return false when note is already archived"
    );

    // Must not have inserted a duplicate edge
    assert_eq!(
        count_edges(&store, new_id, old_id, "supersedes"),
        1,
        "duplicate supersedes edge must not be inserted"
    );
}

// ── add_note_superseding() ──────────────────────────────────────────────────

#[test]
fn add_note_superseding_happy_path_archives_old_and_links_new() {
    let store = open_store();

    let (old_id, _) = store
        .add_note("decision", "Old decision", "old body", &[], &[], None, None)
        .unwrap();

    let (new_id, created) = store
        .add_note_superseding(
            "decision",
            "New decision",
            "new body",
            &[],
            &[],
            None,
            old_id,
        )
        .unwrap();
    assert!(
        created,
        "a fresh supersede insert must report created = true"
    );

    let old_note = store.get(old_id).unwrap().expect("old note must exist");
    assert_eq!(old_note.status, "archived");
    assert_eq!(old_note.superseded_by, Some(new_id));

    assert_eq!(
        count_edges(&store, new_id, old_id, "supersedes"),
        1,
        "expected exactly one supersedes edge"
    );
}

/// ADR-068 amendment E4: re-superseding an already-archived OLD (via a second
/// `add_note_superseding` call naming a different successor) must reject with
/// an error and roll back the whole transaction — no orphaned new note, no
/// second supersedes edge, OLD's existing successor link untouched.
#[test]
fn add_note_superseding_rejects_already_archived_old_and_writes_nothing() {
    let store = open_store();

    let (old_id, _) = store
        .add_note("decision", "Old decision", "old body", &[], &[], None, None)
        .unwrap();
    let (successor_a, _) = store
        .add_note_superseding("decision", "Successor A", "body a", &[], &[], None, old_id)
        .unwrap();

    let count_before = store.count().unwrap();

    let result =
        store.add_note_superseding("decision", "Successor B", "body b", &[], &[], None, old_id);
    assert!(
        result.is_err(),
        "re-superseding an already-archived OLD must error, not silently succeed"
    );

    assert_eq!(
        store.count().unwrap(),
        count_before,
        "a rejected supersede must not leave an orphaned new note row"
    );

    let old_note = store.get(old_id).unwrap().expect("old note must exist");
    assert_eq!(
        old_note.superseded_by,
        Some(successor_a),
        "OLD's successor link must still point at the first, not the rejected second, successor"
    );

    assert_eq!(
        count_edges(&store, successor_a, old_id, "supersedes"),
        1,
        "the original supersedes edge must be untouched"
    );
}

/// Superseding a nonexistent OLD id must also error, not silently create an
/// unlinked new note (the archive-`OLD` `UPDATE` matches zero rows either way).
#[test]
fn add_note_superseding_rejects_nonexistent_old() {
    let store = open_store();
    let count_before = store.count().unwrap();

    let result = store.add_note_superseding("decision", "New", "new body", &[], &[], None, 999_999);
    assert!(
        result.is_err(),
        "superseding a nonexistent OLD id must error"
    );
    assert_eq!(
        store.count().unwrap(),
        count_before,
        "no note must be created when OLD does not exist"
    );
}

// ── add_edge() ───────────────────────────────────────────────────────────────

#[test]
fn add_edge_valid_kinds_accepted() {
    let store = open_store();
    let (a, _) = store
        .add_note("note", "A", "", &[], &[], None, None)
        .unwrap();
    let (b, _) = store
        .add_note("note", "B", "", &[], &[], None, None)
        .unwrap();

    for kind in ["supersedes", "relates_to", "contradicts"] {
        store
            .add_edge(a, b, kind)
            .unwrap_or_else(|e| panic!("add_edge with kind '{kind}' failed: {e}"));
    }
}

#[test]
fn add_edge_invalid_kind_returns_err() {
    let store = open_store();
    let (a, _) = store
        .add_note("note", "A", "", &[], &[], None, None)
        .unwrap();
    let (b, _) = store
        .add_note("note", "B", "", &[], &[], None, None)
        .unwrap();

    let err = store
        .add_edge(a, b, "invented")
        .expect_err("add_edge with invalid kind must return Err");
    assert!(
        err.to_string().contains("invented"),
        "error message must mention the invalid kind; got: {err}"
    );
}

#[test]
fn add_edge_duplicate_silently_ignored() {
    let store = open_store();
    let (a, _) = store
        .add_note("note", "A", "", &[], &[], None, None)
        .unwrap();
    let (b, _) = store
        .add_note("note", "B", "", &[], &[], None, None)
        .unwrap();

    store.add_edge(a, b, "relates_to").unwrap();
    store.add_edge(a, b, "relates_to").unwrap(); // second call must not error

    assert_eq!(
        count_edges(&store, a, b, "relates_to"),
        1,
        "duplicate edge must not produce a second row"
    );
}

// ── UUID identity + cursor + idempotent apply ────────────────────────────────

#[test]
fn ensure_uuid_backfills_and_is_idempotent() {
    let store = open_store();
    let (id, _) = store
        .add_note("decision", "D", "body", &[], &[], None, None)
        .unwrap();

    // No UUID until first sync.
    assert_eq!(store.uuid_for(id).unwrap(), None);

    let u1 = store.ensure_uuid(id).unwrap();
    assert!(!u1.is_empty());
    // UUIDv7 string form is 36 chars.
    assert_eq!(u1.len(), 36);

    // Re-running keeps the same UUID (idempotent backfill).
    let u2 = store.ensure_uuid(id).unwrap();
    assert_eq!(u1, u2);
    assert_eq!(store.uuid_for(id).unwrap(), Some(u1));
}

#[test]
fn rows_for_sync_assigns_uuids_and_is_text_only() {
    let store = open_store();
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();
    store
        .add_note("note", "Two", "second", &[], &[], None, None)
        .unwrap();

    let rows = store.rows_for_sync(false).unwrap();
    assert_eq!(rows.len(), 2);
    // Every row carries a freshly-assigned UUID; SyncRow has no embedding field
    // at all (text-only by construction).
    for r in &rows {
        assert_eq!(r.uuid.len(), 36);
        assert!(r.remote_id.is_none());
    }
    // Ordered oldest-first so supersede targets precede referrers.
    assert_eq!(rows[0].title, "One");
    assert_eq!(rows[1].title, "Two");
}

#[test]
fn apply_remote_note_is_idempotent_no_dupes() {
    let store = open_store();
    let remote_id = "01890000-0000-7000-8000-000000000001";

    let inserted = store
        .apply_remote_note(
            remote_id,
            "decision",
            "Remote",
            "body",
            None,
            1_700_000_000,
            false,
        )
        .unwrap();
    assert!(inserted, "first apply inserts");

    // Re-applying the same remote_id must NOT create a duplicate.
    let inserted2 = store
        .apply_remote_note(
            remote_id,
            "decision",
            "Remote",
            "body",
            None,
            1_700_000_000,
            false,
        )
        .unwrap();
    assert!(!inserted2, "second apply is a no-op");

    let n: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE remote_id = ?1",
            rusqlite::params![remote_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "exactly one local row for the remote id");
}

#[test]
fn apply_remote_note_tombstone_archives_existing() {
    let store = open_store();
    let remote_id = "01890000-0000-7000-8000-000000000002";

    store
        .apply_remote_note(remote_id, "note", "T", "b", None, 1_700_000_000, false)
        .unwrap();
    let local_id = store.note_id_for_remote_id(remote_id).unwrap().unwrap();
    assert_eq!(store.get(local_id).unwrap().unwrap().status, "active");

    // A pulled tombstone archives the local copy (never un-archives).
    let inserted = store
        .apply_remote_note(remote_id, "note", "T", "b", None, 1_700_000_000, true)
        .unwrap();
    assert!(!inserted);
    assert_eq!(store.get(local_id).unwrap().unwrap().status, "archived");
}

#[test]
fn max_remote_id_is_the_pull_cursor() {
    let store = open_store();

    // Nothing synced yet → no cursor (caller does a full catch-up).
    assert_eq!(store.max_remote_id().unwrap(), None);

    // Record a few cloud ids. UUIDv7 strings sort lexically == time order, so
    // MAX() returns the newest one regardless of insertion order.
    let (a, _) = store
        .add_note("note", "A", "b", &[], &[], None, None)
        .unwrap();
    let (b, _) = store
        .add_note("note", "B", "b", &[], &[], None, None)
        .unwrap();
    let (c, _) = store
        .add_note("note", "C", "b", &[], &[], None, None)
        .unwrap();
    store
        .set_remote_id(b, "01890000-0000-7000-8000-000000000002")
        .unwrap();
    store
        .set_remote_id(a, "01890000-0000-7000-8000-000000000001")
        .unwrap();
    store
        .set_remote_id(c, "01890000-0000-7000-8000-000000000003")
        .unwrap();

    assert_eq!(
        store.max_remote_id().unwrap().as_deref(),
        Some("01890000-0000-7000-8000-000000000003"),
        "cursor must be the max (newest) remote_id"
    );
}

#[test]
fn set_remote_id_records_and_dedupes() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "N", "b", &[], &[], None, None)
        .unwrap();
    store.ensure_uuid(id).unwrap();
    let remote_id = "01890000-0000-7000-8000-0000000000ff";

    assert!(!store.has_remote_id(remote_id).unwrap());
    store.set_remote_id(id, remote_id).unwrap();
    assert!(store.has_remote_id(remote_id).unwrap());
    assert_eq!(store.note_id_for_remote_id(remote_id).unwrap(), Some(id));
}

#[test]
fn add_note_persists_entity_id() {
    let store = open_store();
    let (id, _) = store
        .add_note("decision", "HTTP layer", "use axum", &[], &[], None, None)
        .unwrap();

    let stored: Option<String> = store
        .conn
        .query_row(
            "SELECT entity_id FROM notes WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stored.as_deref(),
        Some("cc308a1ca5d849191e1710cc9def561377a9ef37e4fcb895e5aa3b1896e43603"),
        "the stored column must hold the canonical id"
    );
}

#[test]
fn union_tags_and_files_is_add_wins() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "N", "b", &["alpha"], &["a.rs"], None, None)
        .unwrap();

    let read = |store: &MemoryStore| -> (Option<String>, Option<String>) {
        store
            .conn
            .query_row(
                "SELECT tags, linked_files FROM notes WHERE id = ?1",
                rusqlite::params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };

    // New values are appended; the existing ones survive.
    assert!(
        store
            .union_tags_and_files(id, &["beta".to_string()], &["b.rs".to_string()])
            .unwrap()
    );
    assert_eq!(read(&store).0.as_deref(), Some("alpha,beta"));
    assert_eq!(read(&store).1.as_deref(), Some("a.rs,b.rs"));

    // Nothing new to add: no write, and nothing is dropped.
    assert!(
        !store
            .union_tags_and_files(id, &["alpha".to_string()], &[])
            .unwrap(),
        "a subset must not rewrite the row"
    );
    assert_eq!(read(&store).0.as_deref(), Some("alpha,beta"));
}

/// The union rewrites `tags`, and `tags` is an FTS-indexed column — the
/// AFTER UPDATE trigger must keep the index in step or search goes stale.
#[test]
fn union_tags_keeps_fts_in_sync() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "Findable", "body", &["alpha"], &[], None, None)
        .unwrap();
    store
        .union_tags_and_files(id, &["zetatag".to_string()], &[])
        .unwrap();

    let hits: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM memory_fts WHERE memory_fts MATCH 'zetatag'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1, "the unioned tag must be searchable");
}

/// The entity_id migration runs against a store that predates the column and
/// already holds rows that collide under the new key. It must add the column
/// without aborting, and, per ADR-068's third amendment, backfill every
/// legacy row's `entity_id` (Step A) while leaving the *rows themselves*
/// alone: collapsing duplicates is `spelunk memory dedupe`'s job, not an
/// automatic side effect of opening the store, so Step B must also leave the
/// index non-unique while a duplicate group remains.
#[test]
fn entity_id_migration_backfills_but_does_not_collapse_duplicates() {
    register_sqlite_vec();
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("memory.db");

    // Build a store via the schema-only path (skips the Step A/B pipeline a
    // real `open()` runs), so seeding two duplicate-content rows below isn't
    // rejected by an index a fresh, zero-duplicate store would otherwise have
    // already promoted to UNIQUE. Then take the column away entirely to model
    // a genuinely pre-023 DB.
    {
        let conn = rusqlite::Connection::open(&path).expect("open raw");
        let store = MemoryStore { conn };
        store.migrate().expect("schema migration only");
        for created_at in [1_700_000_001_i64, 1_700_000_002] {
            store
                .add_note_with_created_at(
                    "decision",
                    "same text",
                    "same body",
                    &[],
                    &[],
                    None,
                    "active",
                    created_at,
                )
                .expect("seed duplicate-text note");
        }
        store
            .execute_batch(
                "DROP INDEX idx_notes_entity_id; \
                 ALTER TABLE notes DROP COLUMN entity_id;",
            )
            .expect("drop column to simulate the older schema");
        let has_col: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('notes') WHERE name = 'entity_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_col, 0, "precondition: the column is gone");
    }

    // Re-opening (the real `MemoryStore::open`) re-adds the column (migration
    // 023), then runs Step A (backfill) and Step B (duplicate scan).
    let store = MemoryStore::open(&path).expect("migration must not abort on existing data");

    let rows: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 2, "opening alone must not delete or merge any row");

    let has_col: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('notes') WHERE name = 'entity_id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_col, 1, "the column is added");

    // Step A backfills: no legacy row is left NULL.
    let nulls: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE entity_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        nulls, 0,
        "Step A must backfill entity_id for every legacy row"
    );
    assert_eq!(
        super::super::entity_id::note_entity_id(&store.list(None, 10, true).unwrap()[0]),
        super::super::entity_id::note_entity_id(&store.list(None, 10, true).unwrap()[1]),
        "the two legacy rows do collide under the new key"
    );

    // Step B must not have promoted the index: the two rows above are a
    // duplicate group, so the store stays on the non-unique index until an
    // explicit `spelunk memory dedupe` collapses it.
    let idx_sql: String = store
        .conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_notes_entity_id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !idx_sql.to_uppercase().contains("UNIQUE"),
        "a duplicate group must keep the index non-unique: {idx_sql}"
    );

    // Idempotent: opening again is a no-op, not a duplicate-column error.
    drop(store);
    MemoryStore::open(&path).expect("re-open must be idempotent");
}

/// `note_embeddings` is a `vec0` virtual table, so like the code `embeddings`
/// table it does not honour `INSERT OR REPLACE`: re-embedding an existing
/// `note_id` must overwrite in place (one last-write-wins row), not error or
/// duplicate.
#[test]
fn insert_embedding_replaces_a_repeated_note_id() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "N", "b", &[], &[], None, None)
        .unwrap();

    let dim = crate::embeddings::EMBEDDING_DIM;
    let mut first = vec![0f32; dim];
    first[0] = 1.0;
    let mut second = vec![0f32; dim];
    second[5] = 1.0;

    store
        .insert_embedding(id, &crate::embeddings::vec_to_blob(&first))
        .expect("first note embedding");
    store
        .insert_embedding(id, &crate::embeddings::vec_to_blob(&second))
        .expect("second note embedding (replace)");

    let count: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM note_embeddings WHERE note_id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "a repeated note_id must leave exactly one row");

    let stored: Vec<u8> = store
        .conn
        .query_row(
            "SELECT embedding FROM note_embeddings WHERE note_id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stored,
        crate::embeddings::vec_to_blob(&second),
        "the second embedding must overwrite the first"
    );
}

/// Replacing a `note_id` that has never been embedded must be a harmless
/// no-op DELETE followed by a normal INSERT, not an error — the common case
/// of embedding a note for the first time.
#[test]
fn insert_embedding_of_nonexistent_note_id_is_a_harmless_delete_no_op() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "N", "b", &[], &[], None, None)
        .unwrap();

    let dim = crate::embeddings::EMBEDDING_DIM;
    let mut vector = vec![0f32; dim];
    vector[7] = 1.0;

    store
        .insert_embedding(id, &crate::embeddings::vec_to_blob(&vector))
        .expect("embedding a never-before-embedded note must succeed");

    let count: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM note_embeddings WHERE note_id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "the first embed for a fresh note must land exactly once"
    );
}

/// The strongest test of "joins the existing transaction" vs. "just happens
/// not to error": call `insert_embedding` for a repeated `note_id` from
/// WITHIN a transaction the caller already opened, then roll that outer
/// transaction back. If the delete+insert genuinely joined the caller's
/// transaction, rolling it back must undo both halves, restoring the
/// pre-transaction row exactly.
#[test]
fn insert_embedding_joins_callers_transaction_and_rolls_back_with_it() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "N", "b", &[], &[], None, None)
        .unwrap();

    let dim = crate::embeddings::EMBEDDING_DIM;
    let mut first = vec![0f32; dim];
    first[0] = 1.0;
    store
        .insert_embedding(id, &crate::embeddings::vec_to_blob(&first))
        .expect("seed row (autocommit)");

    let mut second = vec![0f32; dim];
    second[1] = 1.0;

    {
        let tx = store
            .conn
            .unchecked_transaction()
            .expect("caller opens an outer transaction");
        assert!(
            !store.conn.is_autocommit(),
            "precondition: connection must be mid-transaction, exercising the \
             is_autocommit() guard's join branch rather than its own-BEGIN branch"
        );

        store
            .insert_embedding(id, &crate::embeddings::vec_to_blob(&second))
            .expect("replacing inside the caller's open transaction must not nest a BEGIN");

        tx.rollback().expect("roll back the outer transaction");
    }

    let count: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM note_embeddings WHERE note_id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "rollback must not leave the row deleted — the DELETE half of the \
         replace was part of the outer transaction and must roll back with it"
    );

    let stored: Vec<u8> = store
        .conn
        .query_row(
            "SELECT embedding FROM note_embeddings WHERE note_id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stored,
        crate::embeddings::vec_to_blob(&first),
        "rollback must restore the pre-transaction (first) vector — if the \
         delete+insert had committed independently of the caller's \
         transaction, the row would still hold `second` here"
    );
}
