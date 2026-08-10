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

// The CLI's version-skew suite replays recorded peer health bodies. Those
// recordings are only evidence of what a peer sends while the live body still
// carries the same keys: once this handler and the recording drift, the replay
// keeps passing against a shape no peer emits. The `handlers.rs` split is
// exactly the kind of change that can drop a key without any test here
// noticing, so the live body is compared to the recording rather than assumed
// equal to it.
#[tokio::test]
async fn live_health_keys_match_the_recorded_peer_fixture() {
    fn keys(value: &Value, at: &str) -> Vec<String> {
        let mut names: Vec<String> = value
            .as_object()
            .unwrap_or_else(|| panic!("`{at}` must be an object"))
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    let recorded: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spelunk-cli/tests/fixtures/skew/health-v0.9.5-ready.json"
    )))
    .expect("recorded fixture must be JSON");

    let live = get_health_json(make_app_with_embedder(4)).await;

    assert_eq!(
        keys(&live, "live body"),
        keys(&recorded, "recorded body"),
        "the live health body and the recorded peer fixture no longer carry the \
         same top-level keys, so the skew replay is asserting against a shape no \
         peer sends"
    );
    for nested in ["embedder", "limits"] {
        assert_eq!(
            keys(&live[nested], nested),
            keys(&recorded[nested], nested),
            "`{nested}` drifted between the live body and the recorded fixture"
        );
    }

    // The CLI's lenient reads distinguish "absent" from "present and null", so
    // an omitted-rather-than-null token cap would silently change which branch
    // a peer without one exercises.
    let (app, _) = make_app(0.92);
    let no_embedder = get_health_json(app).await;
    assert!(
        no_embedder["limits"]
            .as_object()
            .expect("limits must be an object")
            .contains_key("embedder_token_cap"),
        "embedder_token_cap must be emitted as an explicit null, not omitted"
    );
}
