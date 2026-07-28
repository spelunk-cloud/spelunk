//! Shared test helpers.
#![allow(dead_code)]
//!
//! Import with `mod common;` or `use crate::common::*;` inside integration tests.

use std::sync::OnceLock;

/// Register the sqlite-vec extension exactly once for the test process.
///
/// sqlite3_auto_extension is process-global; calling it more than once per
/// address is a no-op but calling it from multiple threads without
/// synchronisation is UB.  `OnceLock` guarantees single initialisation.
///
/// Tests that open a `Database` or `ServerDb` **must** call this first.
/// Annotate those tests with `#[serial_test::serial]` so the global
/// registration happens before any connection is opened.
pub fn register_sqlite_vec() {
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

/// Open an in-memory `spelunk_core::storage::Database` for tests.
///
/// Calls `register_sqlite_vec()` automatically.
pub fn open_test_db() -> spelunk_core::storage::Database {
    register_sqlite_vec();
    spelunk_core::storage::Database::open(std::path::Path::new(":memory:"))
        .expect("failed to open in-memory database")
}

/// Open an in-memory `spelunk_server::db::ServerDb` for tests.
///
/// Calls `register_sqlite_vec()` automatically.
pub fn open_test_server_db(dim: usize) -> spelunk_server::db::ServerDb {
    register_sqlite_vec();
    spelunk_server::db::ServerDb::open(std::path::Path::new(":memory:"), dim, "test-model")
        .expect("failed to open in-memory server database")
}

/// Build a minimal `AppState` backed by an in-memory DB for integration tests.
pub fn make_test_state(dim: usize, auth_key: Option<String>) -> spelunk_server::AppState {
    let db = open_test_server_db(dim);
    let instance_id = db.get_or_create_instance_id().expect("instance_id in test");
    spelunk_server::AppState {
        db: std::sync::Arc::new(tokio::sync::Mutex::new(db)),
        auth: std::sync::Arc::new(spelunk_server::auth::ApiKeyAuth::new(auth_key)),
        conflict_threshold: spelunk_server::default_conflict_threshold(),
        embedder: spelunk_server::EmbedderSlot::disabled(),
        embed_admission: spelunk_server::EmbedAdmission::new(
            spelunk_server::EMBED_QUEUE_CAPACITY,
            spelunk_server::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        llm: None,
        max_tokens_ceiling: 8192,
        rate_limiter: std::sync::Arc::new(spelunk_server::rate_limiter::RateLimiter::new(1000, 60)),
        instance_id,
        started_by: None,
        relay: spelunk_server::relay::RelayRegistry::new(),
    }
}
