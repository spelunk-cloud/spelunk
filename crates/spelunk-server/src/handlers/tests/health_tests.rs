use std::sync::Arc;

use axum::body::Body;
use axum::http::{self, Request};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::support::{
    MockEmbedder, get_health_json, make_app, make_app_with_embedder, make_app_with_slot,
};

// GET /v1/health should return JSON with `status`, `version`, and `capabilities`.
#[tokio::test]
async fn health_returns_json_with_capabilities() {
    let (app, _) = make_app(0.92);
    let req = Request::builder()
        .method("GET")
        .uri("/v1/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), http::StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).expect("health must return JSON");
    assert_eq!(json["status"], json!("ok"), "status must be 'ok'");
    assert!(json["version"].is_string(), "version must be a string");
    assert!(
        json["capabilities"].is_array(),
        "capabilities must be an array"
    );
    let caps = json["capabilities"].as_array().unwrap();
    assert!(
        caps.iter().any(|c| c == "memory"),
        "capabilities must include 'memory'"
    );
    let id = json["instance_id"]
        .as_str()
        .expect("instance_id must be a string");
    assert_eq!(
        id.len(),
        36,
        "instance_id must be a UUID v7 (36 chars): {id}"
    );
    assert!(
        json["started_by"].is_null(),
        "started_by must be null in test (None)"
    );
    // make_app has no embedder → disabled.
    assert_eq!(
        json["embedder"]["state"],
        json!("disabled"),
        "embedder.state must be 'disabled' when no embedder is configured"
    );
    // `limits` is always present, even with no embedder: a client needs
    // the request-timeout/batch-count limits before asking if one is ready.
    assert_eq!(
        json["limits"]["embed_request_timeout_secs"],
        json!(crate::EMBED_REQUEST_TIMEOUT.as_secs()),
        "limits.embed_request_timeout_secs must reflect EMBED_REQUEST_TIMEOUT"
    );
    assert_eq!(
        json["limits"]["max_batch_chunks"],
        json!(crate::handlers::MAX_EMBED_BATCH),
        "limits.max_batch_chunks must reflect MAX_EMBED_BATCH"
    );
    assert!(
        json["limits"]["embedder_token_cap"].is_null(),
        "embedder_token_cap must be null with no embedder configured"
    );
}

// GET /v1/health with a mock embedder of dim 4 must report `embedding_dim: 4`.
#[tokio::test]
async fn health_embedding_dim_with_embedder() {
    let app = make_app_with_embedder(4);
    let req = Request::builder()
        .method("GET")
        .uri("/v1/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), http::StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).expect("health must return JSON");
    assert_eq!(
        json["embedding_dim"],
        json!(4),
        "embedding_dim must match the mock embedder dimension (4)"
    );
    // Capabilities must include index.embed when embedder is present.
    let caps = json["capabilities"].as_array().unwrap();
    assert!(
        caps.iter().any(|c| c == "index.embed"),
        "capabilities must include index.embed when embedder is loaded"
    );
    assert_eq!(
        json["embedder"]["state"],
        json!("ready"),
        "embedder.state must be 'ready' when the embedder is loaded"
    );
    // `MockEmbedder` doesn't override `token_cap()`, so it gets the
    // trait's default `None`: same as any non-native backend. Only
    // `NativeEmbedder` has a real, host-derived cap to report.
    assert!(
        json["limits"]["embedder_token_cap"].is_null(),
        "embedder_token_cap must be null for a backend with no known cap"
    );
}

// GET /v1/health with no embedder (the default make_app) must report `embedding_dim: 0`.
#[tokio::test]
async fn health_embedding_dim_without_embedder() {
    let (app, _) = make_app(0.92);
    let req = Request::builder()
        .method("GET")
        .uri("/v1/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), http::StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).expect("health must return JSON");
    assert_eq!(
        json["embedding_dim"],
        json!(0),
        "embedding_dim must be 0 when no embedder is configured"
    );
}

// ── Readiness / warm-up contract ─────────────────────────────────────

// While the embedder is `loading`, `/v1/health` is still live (200), reports
// `embedder.state: "loading"`, withholds the semantic capabilities, and keeps
// `embedding_dim: 0`: i.e. health is live *before* the model is ready.
#[tokio::test]
async fn health_live_while_embedder_loading() {
    let slot = crate::EmbedderSlot::loading();
    let app = make_app_with_slot(4, slot);
    let json = get_health_json(app).await;
    assert_eq!(json["status"], json!("ok"));
    assert_eq!(
        json["embedder"]["state"],
        json!("loading"),
        "state must be 'loading' before the model is published"
    );
    assert_eq!(
        json["embedding_dim"],
        json!(0),
        "embedding_dim must stay 0 until ready"
    );
    let caps = json["capabilities"].as_array().unwrap();
    assert!(
        !caps
            .iter()
            .any(|c| c == "index.embed" || c == "search.semantic"),
        "semantic capabilities must be absent while loading: {caps:?}"
    );
}

// The readiness cell flips `loading → ready`: after `set_ready`, health
// reports `ready`, advertises the caps, and surfaces the real `embedding_dim`.
#[tokio::test]
async fn health_reflects_loading_to_ready_transition() {
    let slot = crate::EmbedderSlot::loading();
    // Before: loading.
    let app = make_app_with_slot(4, slot.clone());
    assert_eq!(
        get_health_json(app).await["embedder"]["state"],
        json!("loading")
    );

    // Publish the backend (as the background load task would).
    slot.set_ready(Arc::new(MockEmbedder { dim: 4 }));

    let app = make_app_with_slot(4, slot);
    let json = get_health_json(app).await;
    assert_eq!(json["embedder"]["state"], json!("ready"));
    assert_eq!(json["embedding_dim"], json!(4));
    let caps = json["capabilities"].as_array().unwrap();
    assert!(caps.iter().any(|c| c == "index.embed"));
}

// A failed load flips `loading → unavailable`, carrying the error detail.
#[tokio::test]
async fn health_reflects_load_failure() {
    let slot = crate::EmbedderSlot::loading();
    slot.set_unavailable("download error: boom");
    let app = make_app_with_slot(4, slot);
    let json = get_health_json(app).await;
    assert_eq!(json["embedder"]["state"], json!("unavailable"));
    assert_eq!(
        json["embedder"]["detail"],
        json!("download error: boom"),
        "detail must carry the failure summary"
    );
    assert_eq!(json["embedding_dim"], json!(0));
}
