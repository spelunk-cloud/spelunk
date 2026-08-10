// Shared test-only helpers for `entity_id_migration`'s `migration_tests` submodule.

use super::ENTITY_ID_UNIQUE_MARKER;
use crate::storage::memory::MemoryStore;
use rusqlite::OptionalExtension;
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

// Schema-only path: skips the Step A/B pipeline a real `MemoryStore::open`
// runs, so tests can seed rows before driving Step A/B themselves.
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

pub(super) fn marker_exists(store: &MemoryStore) -> bool {
    store
        .conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            rusqlite::params![ENTITY_ID_UNIQUE_MARKER],
            |_| Ok(true),
        )
        .optional()
        .unwrap()
        .is_some()
}

pub(super) fn index_is_unique(store: &MemoryStore) -> bool {
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

pub(super) fn null_entity_id_count(store: &MemoryStore) -> i64 {
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE entity_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
}
