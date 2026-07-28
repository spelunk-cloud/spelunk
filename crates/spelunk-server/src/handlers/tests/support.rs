// Test helpers shared across the `handlers::tests::*` theme modules: app/router
// builders, mock backends reused by more than one theme, and thin HTTP request
// helpers. Single-use mocks stay local to the theme file that needs them.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{self, Request};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::auth::ApiKeyAuth;
use crate::db::ServerDb;
use crate::{AppState, router};

// Register sqlite-vec extension once per test process.
pub(super) fn register_sqlite_vec() {
    use std::sync::OnceLock;
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

pub(super) fn make_app(conflict_threshold: f32) -> (axum::Router, i32) {
    register_sqlite_vec();
    let dim: usize = 4;
    let db = ServerDb::open(std::path::Path::new(":memory:"), dim, "test-model")
        .expect("failed to open in-memory server db");
    let instance_id = db.get_or_create_instance_id().expect("instance_id in test");
    let state = AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        auth: Arc::new(ApiKeyAuth::new(None)),
        conflict_threshold,
        embedder: crate::EmbedderSlot::disabled(),
        embed_admission: crate::EmbedAdmission::new(
            crate::EMBED_QUEUE_CAPACITY,
            crate::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        llm: None,
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(crate::rate_limiter::RateLimiter::new(1000, 60)),
        instance_id,
        started_by: None,
        relay: crate::relay::RelayRegistry::new(),
    };
    (router(state), dim as i32)
}

// POST /v1/projects/{slug}/memory with the given embedding. Returns the response.
pub(super) async fn post_note(
    app: axum::Router,
    slug: &str,
    title: &str,
    embedding: Vec<f32>,
) -> (http::StatusCode, Value) {
    let body = json!({
        "kind": "note",
        "title": title,
        "body": "test body",
        "embedding": embedding,
    });
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/projects/{slug}/memory"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

// A minimal mock embedder that always returns a single zero vector of `dim` dimensions.
// Used to verify that `embedding_dim` is surfaced correctly in the health response.
pub(super) struct MockEmbedder {
    pub(super) dim: usize,
}

#[async_trait::async_trait]
impl spelunk_core::embeddings::EmbeddingBackend for MockEmbedder {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.0_f32; self.dim]).collect())
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

// Build an app with the given embedder slot (dim used only to size the DB).
pub(super) fn make_app_with_slot(dim: usize, embedder: crate::EmbedderSlot) -> axum::Router {
    register_sqlite_vec();
    let db = ServerDb::open(std::path::Path::new(":memory:"), dim, "test-model")
        .expect("failed to open in-memory server db");
    let instance_id = db.get_or_create_instance_id().expect("instance_id in test");
    let state = AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        auth: Arc::new(ApiKeyAuth::new(None)),
        conflict_threshold: 0.92,
        embedder,
        embed_admission: crate::EmbedAdmission::new(
            crate::EMBED_QUEUE_CAPACITY,
            crate::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        llm: None,
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(crate::rate_limiter::RateLimiter::new(1000, 60)),
        instance_id,
        started_by: None,
        relay: crate::relay::RelayRegistry::new(),
    };
    crate::router(state)
}

// Build an app with a ready mock embedder of the given dimension.
pub(super) fn make_app_with_embedder(dim: usize) -> axum::Router {
    make_app_with_slot(
        dim,
        crate::EmbedderSlot::ready(Arc::new(MockEmbedder { dim })),
    )
}

pub(super) async fn get_health_json(app: axum::Router) -> Value {
    let req = Request::builder()
        .method("GET")
        .uri("/v1/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), http::StatusCode::OK, "health must be 200");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("health must return JSON")
}

pub(super) async fn post_embed(app: axum::Router) -> http::Response<Body> {
    let body = json!({"chunks": [{"chunk_id": "abc", "content": "fn foo() {}"}]});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/projects/proj/index/embed")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    app.oneshot(req).await.unwrap()
}

// An LLM backend that immediately closes the token channel: enough to
// exercise routing/rate-limiting without generating real content.
struct NoopLlm;

#[async_trait::async_trait]
impl spelunk_core::llm::LlmBackend for NoopLlm {
    async fn generate(
        &self,
        _messages: &[spelunk_core::llm::Message],
        _max_tokens: usize,
        _tx: tokio::sync::mpsc::Sender<spelunk_core::llm::Token>,
        _json_schema: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

// Build an app with a configured LLM backend and a tight rate limit, for
// exercising `/explore` and `/llm/complete` rate limiting.
pub(super) fn make_app_with_llm_and_limit(max_requests: u32) -> axum::Router {
    register_sqlite_vec();
    let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "test-model")
        .expect("failed to open in-memory server db");
    let instance_id = db.get_or_create_instance_id().expect("instance_id in test");
    let state = AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        auth: Arc::new(ApiKeyAuth::new(None)),
        conflict_threshold: 0.92,
        embedder: crate::EmbedderSlot::disabled(),
        embed_admission: crate::EmbedAdmission::new(
            crate::EMBED_QUEUE_CAPACITY,
            crate::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        llm: Some(Arc::new(NoopLlm)),
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(crate::rate_limiter::RateLimiter::new(max_requests, 60)),
        instance_id,
        started_by: None,
        relay: crate::relay::RelayRegistry::new(),
    };
    router(state)
}

pub(super) async fn post_explore(app: &axum::Router, question: &str) -> http::StatusCode {
    let body = json!({"question": question, "context_chunks": [], "max_turns": 1});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/projects/explore-test/explore")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

// Build an app with an explicit auth key configured (for 401 tests).
pub(super) fn make_app_with_auth_key(key: Option<&str>) -> axum::Router {
    register_sqlite_vec();
    let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "test-model")
        .expect("failed to open in-memory server db");
    let instance_id = db.get_or_create_instance_id().expect("instance_id in test");
    let state = AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        auth: Arc::new(ApiKeyAuth::new(key.map(str::to_string))),
        conflict_threshold: 0.92,
        embedder: crate::EmbedderSlot::disabled(),
        embed_admission: crate::EmbedAdmission::new(
            crate::EMBED_QUEUE_CAPACITY,
            crate::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        llm: None,
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(crate::rate_limiter::RateLimiter::new(1000, 60)),
        instance_id,
        started_by: None,
        relay: crate::relay::RelayRegistry::new(),
    };
    crate::router(state)
}

// POST /v1/projects/{slug}/memory/batch with a raw `entries` JSON value
// (not a typed struct, so malformed/missing-field payloads can be built).
fn batch_request(slug: &str, entries: Value) -> Request<Body> {
    let body = json!({ "entries": entries });
    Request::builder()
        .method("POST")
        .uri(format!("/v1/projects/{slug}/memory/batch"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

pub(super) async fn post_batch(
    app: axum::Router,
    slug: &str,
    entries: Value,
) -> (http::StatusCode, Value) {
    let resp = app.oneshot(batch_request(slug, entries)).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

pub(super) async fn list_notes_via_http(app: axum::Router, slug: &str) -> Vec<Value> {
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/projects/{slug}/memory?limit=100"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub(super) fn note_item(title: &str, external_id: &str) -> Value {
    json!({"kind": "note", "title": title, "external_id": external_id})
}

pub(super) async fn get_status_and_json(app: axum::Router, uri: &str) -> (http::StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

// Bind a real router (with the given injected timeout) to an ephemeral
// TCP port and start serving it in the background. Returns the base URL
// and the shared DB handle (so tests can hold its lock externally to
// simulate a slow synchronous handler).
pub(super) async fn spawn_test_server(
    llm: Option<Arc<dyn spelunk_core::llm::LlmBackend>>,
    request_timeout: std::time::Duration,
) -> (String, Arc<tokio::sync::Mutex<ServerDb>>) {
    register_sqlite_vec();
    let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "test-model")
        .expect("failed to open in-memory server db");
    let instance_id = db.get_or_create_instance_id().expect("instance_id in test");
    // Create the project up front so `/memory/stream` (which 404s on an
    // unknown project) has something valid to stream from.
    db.upsert_project("timeout-test", 4, "test-model")
        .expect("create test project");
    let db = Arc::new(tokio::sync::Mutex::new(db));
    let state = AppState {
        db: db.clone(),
        auth: Arc::new(ApiKeyAuth::new(None)),
        conflict_threshold: 0.92,
        embedder: crate::EmbedderSlot::disabled(),
        embed_admission: crate::EmbedAdmission::new(
            crate::EMBED_QUEUE_CAPACITY,
            crate::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        llm,
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(crate::rate_limiter::RateLimiter::new(1000, 60)),
        instance_id,
        started_by: None,
        relay: crate::relay::RelayRegistry::new(),
    };
    let app = crate::router_with_timeout(state, request_timeout);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("test server crashed");
    });
    (format!("http://{addr}"), db)
}

// Same as [`spawn_test_server`], but with an embedder slot and the
// general/`/index/embed` timeouts injected independently: exists so
// tests can prove `/index/embed` survives past the *general*
// `request_timeout` budget using its own, separately-injected
// `embed_request_timeout` (mirroring the production
// `REQUEST_TIMEOUT`/`EMBED_REQUEST_TIMEOUT` split), without waiting out
// real multi-second budgets.
pub(super) async fn spawn_test_server_with_embed(
    embedder: crate::EmbedderSlot,
    request_timeout: std::time::Duration,
    embed_request_timeout: std::time::Duration,
) -> (String, Arc<tokio::sync::Mutex<ServerDb>>) {
    spawn_test_server_with_embed_and_admission(
        embedder,
        request_timeout,
        embed_request_timeout,
        crate::EmbedAdmission::new(
            crate::EMBED_QUEUE_CAPACITY,
            crate::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
    )
    .await
}

// Same as [`spawn_test_server_with_embed`], but with the embed admission
// gate injected too: exists so tests can prove the `429` shedding
// behaviour with a small, deterministic queue capacity instead of the
// production default.
pub(super) async fn spawn_test_server_with_embed_and_admission(
    embedder: crate::EmbedderSlot,
    request_timeout: std::time::Duration,
    embed_request_timeout: std::time::Duration,
    embed_admission: crate::EmbedAdmission,
) -> (String, Arc<tokio::sync::Mutex<ServerDb>>) {
    register_sqlite_vec();
    let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "test-model")
        .expect("failed to open in-memory server db");
    let instance_id = db.get_or_create_instance_id().expect("instance_id in test");
    db.upsert_project("timeout-test", 4, "test-model")
        .expect("create test project");
    let db = Arc::new(tokio::sync::Mutex::new(db));
    let state = AppState {
        db: db.clone(),
        auth: Arc::new(ApiKeyAuth::new(None)),
        conflict_threshold: 0.92,
        embedder,
        embed_admission,
        llm: None,
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(crate::rate_limiter::RateLimiter::new(1000, 60)),
        instance_id,
        started_by: None,
        relay: crate::relay::RelayRegistry::new(),
    };
    let app = crate::router_with_timeouts(state, request_timeout, embed_request_timeout);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("test server crashed");
    });
    (format!("http://{addr}"), db)
}
