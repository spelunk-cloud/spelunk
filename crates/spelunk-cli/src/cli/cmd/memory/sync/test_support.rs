// Shared test fixtures for the `sync` module's submodule test suites
// (`super::push`, `super::pull`, `super::round`).

use crate::storage::MemoryStore;

pub(super) fn register_sqlite_vec() {
    use std::sync::OnceLock;
    // `MemoryStore::open` creates a vec0 table, so the extension must be
    // registered before any connection opens.
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

// Spin up a real `spelunk-server` axum router (the production router) on
// an ephemeral loopback port, serving the team-hosting
// `/v1/projects/*/memory*` routes this test's `CloudSyncClient`s talk to.
pub(super) async fn spawn_spelunk_server() -> std::net::SocketAddr {
    register_sqlite_vec();
    let db_dir = tempfile::TempDir::new().unwrap();
    let db = spelunk_server::db::ServerDb::open(&db_dir.path().join("server.db"), 4, "test-model")
        .unwrap();
    let instance_id = db.get_or_create_instance_id().unwrap();
    let state = spelunk_server::AppState {
        db: std::sync::Arc::new(tokio::sync::Mutex::new(db)),
        auth: std::sync::Arc::new(spelunk_server::auth::ApiKeyAuth::new(None)),
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
    };
    let app = spelunk_server::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

// A mocked local spelunk-server standing in for the loopback embedder, wired
// up the way auto-discovery actually finds one: a `server.port` file under
// `SPELUNK_STATE_DIR` pointing at the mock. Going through the real discovery
// path (rather than injecting an `inference_url`) is what makes the
// "embed never reaches the team server_url" tests meaningful, and pins the
// probe to this mock instead of whatever happens to listen on port 7777 on the
// machine running the tests.
//
// Mutates process-global env, so every test using it must be `#[serial]`.
pub(super) struct LoopbackEmbedder {
    pub(super) server: wiremock::MockServer,
    _state_dir: tempfile::TempDir,
    prev_state_dir: Option<std::ffi::OsString>,
    prev_no_server: Option<std::ffi::OsString>,
}

impl Drop for LoopbackEmbedder {
    fn drop(&mut self) {
        unsafe {
            match self.prev_state_dir.take() {
                Some(v) => std::env::set_var("SPELUNK_STATE_DIR", v),
                None => std::env::remove_var("SPELUNK_STATE_DIR"),
            }
            match self.prev_no_server.take() {
                Some(v) => std::env::set_var("SPELUNK_NO_SERVER", v),
                None => std::env::remove_var("SPELUNK_NO_SERVER"),
            }
        }
    }
}

// The fp32 vector `spawn_loopback_embedder`'s `/index/embed` route returns.
// L2-normalised and 896-dim, so it survives the push's own dimension guard.
pub(super) fn stub_vector() -> Vec<f32> {
    let dim = spelunk_core::embeddings::EMBEDDING_DIM;
    vec![1.0 / (dim as f32).sqrt(); dim]
}

// Start a mocked loopback inference server for `project_id` and point
// auto-discovery at it. `failing_title_marker`, when given, makes the embed
// route 500 for any request whose body contains it, so a single row's embed
// failure can be exercised without failing the rest.
pub(super) async fn spawn_loopback_embedder(
    project_id: &str,
    failing_title_marker: Option<&str>,
) -> LoopbackEmbedder {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "0.9.5",
            "capabilities": ["memory", "index.embed", "search.semantic"],
            "instance_id": "00000000-0000-0000-0000-000000000001",
            "started_by": null,
            "embedding_dim": spelunk_core::embeddings::EMBEDDING_DIM,
        })))
        .mount(&server)
        .await;
    let embed_path = format!("/v1/projects/{project_id}/index/embed");
    if let Some(marker) = failing_title_marker {
        Mock::given(method("POST"))
            .and(path(embed_path.clone()))
            .and(body_string_contains(marker))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path(embed_path))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(spelunk_core::embeddings::vec_to_blob(&stub_vector())),
        )
        .mount(&server)
        .await;

    let port = server.address().port();
    let state_dir = tempfile::TempDir::new().unwrap();
    std::fs::write(state_dir.path().join("server.port"), format!("{port}\n")).unwrap();
    let prev_state_dir = std::env::var_os("SPELUNK_STATE_DIR");
    let prev_no_server = std::env::var_os("SPELUNK_NO_SERVER");
    unsafe {
        std::env::set_var("SPELUNK_STATE_DIR", state_dir.path());
        std::env::remove_var("SPELUNK_NO_SERVER");
    }
    LoopbackEmbedder {
        server,
        _state_dir: state_dir,
        prev_state_dir,
        prev_no_server,
    }
}

// Open a fresh local memory store in a new tempdir, returning both (the
// tempdir must be kept alive by the caller for the store's lifetime).
pub(super) fn fresh_store() -> (tempfile::TempDir, MemoryStore) {
    register_sqlite_vec();
    let tmp = tempfile::TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    (tmp, store)
}
