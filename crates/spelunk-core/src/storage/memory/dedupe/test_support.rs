// Shared test-only helpers for `dedupe`'s `tests` and `superseded_by_tests`
// submodules.

use super::MemoryStore;
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

// A store built via the schema-only path: `migrate()` creates the
// (non-unique) `idx_notes_entity_id` but skips the automatic Step A/B
// pipeline a real `MemoryStore::open` runs. `dedupe_entity_ids` itself
// doesn't care whether Step A/B ran: it groups by recomputing
// `note_entity_id` in Rust regardless of the stored column or index
// state, but these tests need to seed duplicate-content rows directly,
// which a real `open()` on a fresh (zero-row, zero-duplicate) store
// would already have promoted to a UNIQUE index, rejecting the seed.
pub(super) fn open_store() -> MemoryStore {
    register_sqlite_vec();
    let conn = rusqlite::Connection::open(std::path::Path::new(":memory:"))
        .expect("open in-memory sqlite");
    let store = MemoryStore {
        conn,
        reembed_needed: None,
        dropped_768: std::cell::Cell::new(false),
    };
    store.run_migrations().expect("schema migration");
    store
}

pub(super) fn note_count(store: &MemoryStore) -> i64 {
    store
        .conn
        .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap()
}

pub(super) fn has_embedding(store: &MemoryStore, note_id: i64) -> bool {
    store.get_embedding(note_id).unwrap().is_some()
}

// Snapshot every column of every row in `table`, ordered by `order_by`,
// as generic SQLite `Value`s. Used to assert a rolled-back or dry-run
// call left the database byte-for-byte unchanged: unlike a row-count or
// single-column check, this catches a regression in *any* column
// (tags, superseded_by, status, entity_id, uuid, remote_id, ...) without
// having to hand-maintain a column list.
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

pub(super) type TableSnapshot = Vec<Vec<rusqlite::types::Value>>;

// Snapshot of `notes` + `memory_edges` + `note_embeddings`, the three
// tables `dedupe_entity_ids` can touch.
pub(super) fn full_db_snapshot(
    store: &MemoryStore,
) -> (TableSnapshot, TableSnapshot, TableSnapshot) {
    (
        full_table_snapshot(store, "notes", "id"),
        full_table_snapshot(store, "memory_edges", "from_id, to_id, kind"),
        full_table_snapshot(store, "note_embeddings", "note_id"),
    )
}

// Expected `Note::superseded_by` for a store-minted rowid.
pub(super) fn sup(id: i64) -> Option<crate::storage::memory::NoteId> {
    Some(crate::storage::memory::NoteId::from_i64(id))
}
