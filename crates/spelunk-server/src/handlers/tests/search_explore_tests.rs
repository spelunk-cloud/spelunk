use axum::body::Body;
use axum::http::{self, Request};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::support::{make_app, make_app_with_llm_and_limit, post_explore, post_note};

// POST /v1/projects/{slug}/memory/search with no embedder should return 400.
#[tokio::test]
async fn search_without_embedder_returns_400() {
    let (app, _) = make_app(0.92);
    // First create the project.
    let _ = post_note(
        app.clone(),
        "search-proj",
        "seed note",
        vec![1.0, 0.0, 0.0, 0.0],
    )
    .await;

    let body = json!({"query": "test query", "limit": 5});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/projects/search-proj/memory/search")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        http::StatusCode::BAD_REQUEST,
        "search without embedder must return 400"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(
        json["error"]["code"],
        json!("bad_request"),
        "error code must be bad_request"
    );
}

// POST /v1/projects/{slug}/explore with no LLM should return 503.
#[tokio::test]
async fn explore_without_llm_returns_503() {
    let (app, _) = make_app(0.92);
    let body = json!({"question": "what does foo do?", "context_chunks": []});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/projects/proj/explore")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        http::StatusCode::SERVICE_UNAVAILABLE,
        "explore without LLM must return 503"
    );
}

// ── /explore rate limiting ─────────────────────────────────────────────

// `/explore` must be rate-limited like `/llm/complete`: once the per-bucket
// budget is exhausted, further calls get 429, not a normal (SSE 200) response.
#[tokio::test]
async fn explore_returns_429_past_rate_limit() {
    let app = make_app_with_llm_and_limit(2);

    let status1 = post_explore(&app, "q1").await;
    let status2 = post_explore(&app, "q2").await;
    let status3 = post_explore(&app, "q3").await;

    assert_eq!(status1, http::StatusCode::OK, "1st call within budget");
    assert_eq!(status2, http::StatusCode::OK, "2nd call within budget");
    assert_eq!(
        status3,
        http::StatusCode::TOO_MANY_REQUESTS,
        "3rd call must exceed the 2-request budget and return 429"
    );
}

// Two different client IPs (via `X-Forwarded-For`) must not share one
// rate-limit bucket: each gets its own budget, so a shared key can't
// collapse every caller onto one global bucket.
#[tokio::test]
async fn explore_rate_limit_keyed_per_client_ip() {
    let app = make_app_with_llm_and_limit(1);

    let body = json!({"question": "q", "context_chunks": [], "max_turns": 1});
    let req_from = |ip: &str| {
        Request::builder()
            .method("POST")
            .uri("/v1/projects/explore-test/explore")
            .header("content-type", "application/json")
            .header("x-forwarded-for", ip)
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    };

    // Client A's first call succeeds and exhausts its (budget=1) bucket.
    let resp_a1 = app.clone().oneshot(req_from("10.0.0.1")).await.unwrap();
    assert_eq!(resp_a1.status(), http::StatusCode::OK);

    // Client A's second call is rate-limited.
    let resp_a2 = app.clone().oneshot(req_from("10.0.0.1")).await.unwrap();
    assert_eq!(resp_a2.status(), http::StatusCode::TOO_MANY_REQUESTS);

    // Client B (different IP) still has its own budget.
    let resp_b1 = app.clone().oneshot(req_from("10.0.0.2")).await.unwrap();
    assert_eq!(
        resp_b1.status(),
        http::StatusCode::OK,
        "a different client IP must not share client A's exhausted bucket"
    );
}
