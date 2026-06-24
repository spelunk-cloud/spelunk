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

    let old_id = store
        .add_note("decision", "Old decision", "old body", &[], &[], None, None)
        .unwrap();
    let new_id = store
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

    let old_id = store
        .add_note("note", "Alpha", "body", &[], &[], None, None)
        .unwrap();
    let new_id = store
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

// ── add_edge() ───────────────────────────────────────────────────────────────

#[test]
fn add_edge_valid_kinds_accepted() {
    let store = open_store();
    let a = store
        .add_note("note", "A", "", &[], &[], None, None)
        .unwrap();
    let b = store
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
    let a = store
        .add_note("note", "A", "", &[], &[], None, None)
        .unwrap();
    let b = store
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
    let a = store
        .add_note("note", "A", "", &[], &[], None, None)
        .unwrap();
    let b = store
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

// ── ADR-037 D2: UUID identity + cursor + idempotent apply ────────────────────

#[test]
fn ensure_uuid_backfills_and_is_idempotent() {
    let store = open_store();
    let id = store
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
    // at all (text-only by construction — ADR-037 D3).
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
    let a = store
        .add_note("note", "A", "b", &[], &[], None, None)
        .unwrap();
    let b = store
        .add_note("note", "B", "b", &[], &[], None, None)
        .unwrap();
    let c = store
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
    let id = store
        .add_note("note", "N", "b", &[], &[], None, None)
        .unwrap();
    store.ensure_uuid(id).unwrap();
    let remote_id = "01890000-0000-7000-8000-0000000000ff";

    assert!(!store.has_remote_id(remote_id).unwrap());
    store.set_remote_id(id, remote_id).unwrap();
    assert!(store.has_remote_id(remote_id).unwrap());
    assert_eq!(store.note_id_for_remote_id(remote_id).unwrap(), Some(id));
}
