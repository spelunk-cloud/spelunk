// Pin the wire shape of the memory read endpoints: their JSON response root
// must be an object envelope, never a bare array. Mirrors the self-hosted
// team server's side of cloud-api's `!body.is_array()` wire-shape test. See
// ADR-076 (memory wire contract) and docs/version-skew.md.

use axum::body::Body;
use axum::http::{self, Request};
use serde_json::json;
use tower::ServiceExt;

use super::support::{get_status_and_json, make_app, make_app_with_embedder, post_note};

// GET /memory returns `{entries, total}`, not a bare `[...]`.
#[tokio::test]
async fn list_notes_returns_object_envelope_not_bare_array() {
    let (app, _dim) = make_app(0.92);
    let (status, body) = post_note(app.clone(), "wire-proj", "A", vec![1.0, 0.0, 0.0, 0.0]).await;
    assert_eq!(status, http::StatusCode::CREATED, "seed: {body}");

    let (status, body) = get_status_and_json(app, "/v1/projects/wire-proj/memory").await;
    assert_eq!(status, http::StatusCode::OK, "body: {body}");
    assert!(
        !body.is_array(),
        "list response root must be an object, not a bare array: {body}"
    );
    assert!(
        body.is_object(),
        "list response root must be an object: {body}"
    );
    let entries = body["entries"]
        .as_array()
        .expect("entries array in envelope");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["title"], json!("A"));
    assert_eq!(body["total"], json!(1));
}

// GET /memory/harvested-shas returns `{shas}`, not a bare `["sha", ...]`.
#[tokio::test]
async fn harvested_shas_returns_object_envelope_not_bare_array() {
    let (app, _dim) = make_app(0.92);
    // Seed the project so the route resolves the handler rather than 404ing
    // on an unknown project before the shape can be observed.
    let (status, body) =
        post_note(app.clone(), "wire-shas-proj", "A", vec![1.0, 0.0, 0.0, 0.0]).await;
    assert_eq!(status, http::StatusCode::CREATED, "seed: {body}");

    let (status, body) =
        get_status_and_json(app, "/v1/projects/wire-shas-proj/memory/harvested-shas").await;
    assert_eq!(status, http::StatusCode::OK, "body: {body}");
    assert!(
        !body.is_array(),
        "harvested-shas response root must be an object, not a bare array: {body}"
    );
    assert!(
        body["shas"].is_array(),
        "harvested-shas envelope must carry a `shas` array: {body}"
    );
}

// POST /memory/search returns `{entries, total}`, not a bare `[...]`.
#[tokio::test]
async fn search_notes_returns_object_envelope_not_bare_array() {
    let app = make_app_with_embedder(4);
    let (status, body) = post_note(
        app.clone(),
        "wire-search-proj",
        "hit",
        vec![1.0, 0.0, 0.0, 0.0],
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "seed: {body}");

    let req = Request::builder()
        .method("POST")
        .uri("/v1/projects/wire-search-proj/memory/search")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({ "query": "q", "limit": 10 })).unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    assert_eq!(status, http::StatusCode::OK, "body: {body}");
    assert!(
        !body.is_array(),
        "search response root must be an object, not a bare array: {body}"
    );
    assert!(
        body["entries"].is_array(),
        "search envelope must carry an `entries` array: {body}"
    );
}
