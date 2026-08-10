use axum::body::Body;
use axum::http::{self, Request};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::support::{make_app, make_app_with_embedder, make_app_with_slot, post_embed};

// POST /v1/projects/{slug}/index/embed with no embedder should return 400.
#[tokio::test]
async fn embed_without_embedder_returns_400() {
    let (app, _) = make_app(0.92);
    let body = json!({"chunks": [{"chunk_id": "abc", "content": "fn foo() {}"}]});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/projects/proj/index/embed")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        http::StatusCode::BAD_REQUEST,
        "embed without embedder must return 400"
    );
}

// POST /v1/projects/{slug}/index/embed with >256 chunks should return 413.
#[tokio::test]
async fn embed_batch_too_large_returns_413() {
    let (app, _) = make_app(0.92);
    let chunks: Vec<Value> = (0..=256)
        .map(|i| json!({"chunk_id": format!("c{i}"), "content": "fn foo() {}"}))
        .collect();
    let body = json!({"chunks": chunks});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/projects/proj/index/embed")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        http::StatusCode::PAYLOAD_TOO_LARGE,
        "batch >256 must return 413"
    );
}

// While `loading`, embed endpoints return `503 + Retry-After: 5` and a body
// with `state: "loading"` (transient: the CLI keeps polling).
#[tokio::test]
async fn embed_while_loading_returns_503_retry_after() {
    let app = make_app_with_slot(4, crate::EmbedderSlot::loading());
    let resp = post_embed(app).await;
    assert_eq!(
        resp.status(),
        http::StatusCode::SERVICE_UNAVAILABLE,
        "embed while loading must return 503"
    );
    assert_eq!(
        resp.headers()
            .get(http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("5"),
        "loading 503 must carry Retry-After: 5"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["state"], json!("loading"));
}

// While `unavailable` (load failed), embed endpoints return a terminal `503`
// with `state: "unavailable"` and no `Retry-After` (the CLI stops polling).
#[tokio::test]
async fn embed_while_unavailable_returns_terminal_503() {
    let slot = crate::EmbedderSlot::loading();
    slot.set_unavailable("oom");
    let app = make_app_with_slot(4, slot);
    let resp = post_embed(app).await;
    assert_eq!(resp.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        resp.headers().get(http::header::RETRY_AFTER).is_none(),
        "terminal 503 must not advise a retry"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["state"], json!("unavailable"));
}

// While `disabled`, embed endpoints keep the permanent `400` (unchanged
// behaviour for the genuinely-misconfigured case).
#[tokio::test]
async fn embed_while_disabled_returns_400() {
    let app = make_app_with_slot(4, crate::EmbedderSlot::disabled());
    let resp = post_embed(app).await;
    assert_eq!(
        resp.status(),
        http::StatusCode::BAD_REQUEST,
        "embed while disabled must stay 400"
    );
}

// When `ready`, embed endpoints serve `200`.
#[tokio::test]
async fn embed_while_ready_returns_200() {
    let app = make_app_with_embedder(4);
    let resp = post_embed(app).await;
    assert_eq!(
        resp.status(),
        http::StatusCode::OK,
        "embed while ready must return 200"
    );
}
