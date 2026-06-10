//! Integration tests for spelunk-server HTTP handlers using axum's oneshot testing.
//!
//! No real TCP socket is opened — requests go directly through the router.
//! sqlite-vec must be registered before any `ServerDb` is opened, so all
//! tests in this file use `#[serial]`.

mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serial_test::serial;
use spelunk_server::auth::ApiKeyAuth;
use spelunk_server::rate_limiter::RateLimiter;
use spelunk_server::{AppState, router};
use std::sync::Arc;
use tower::ServiceExt; // for `.oneshot()`

// ── helpers ──────────────────────────────────────────────────────────────────

fn make_state() -> AppState {
    common::make_test_state(4, None)
}

fn json_body(body: impl serde::Serialize) -> Body {
    Body::from(serde_json::to_vec(&body).unwrap())
}

async fn send(
    state: AppState,
    method: &str,
    uri: &str,
    body: Body,
    content_type: bool,
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    if content_type {
        builder = builder.header("content-type", "application/json");
    }
    let req = builder.body(body).unwrap();
    router(state).oneshot(req).await.unwrap()
}

/// Like [`send`], but allows attaching extra headers (e.g. `Last-Event-ID`).
async fn send_with_headers(
    state: AppState,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let req = builder.body(Body::empty()).unwrap();
    router(state).oneshot(req).await.unwrap()
}

/// Read raw SSE bytes off a streaming response body until at least
/// `min_events` `\n\n`-terminated frames have been seen, or `timeout` elapses.
/// The `memory_stream` handler never closes its body, so callers must bound
/// how much they read.
async fn read_sse_frames(
    resp: axum::response::Response,
    min_events: usize,
    timeout: std::time::Duration,
) -> String {
    use futures_util::StreamExt;
    let mut body = resp.into_body().into_data_stream();
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if buf.matches("\n\n").count() >= min_events {
            break;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, body.next()).await {
            Ok(Some(Ok(chunk))) => buf.push_str(&String::from_utf8_lossy(&chunk)),
            Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
        }
    }
    buf
}

// ── health ───────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn health_returns_ok() {
    let resp = send(make_state(), "GET", "/v1/health", Body::empty(), false).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── list_projects ─────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn list_projects_empty_initially() {
    let resp = send(make_state(), "GET", "/v1/projects", Body::empty(), false).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let projects: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(projects, serde_json::json!([]));
}

// ── add_note + list_notes ─────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn add_note_creates_project_automatically() {
    let state = make_state();
    let payload = serde_json::json!({
        "kind": "decision",
        "title": "Use SQLite",
        "body": "Simpler than Postgres for local use.",
        "embedding": [0.1_f32, 0.2, 0.3, 0.4],
    });

    let resp = send(
        state.clone(),
        "POST",
        "/v1/projects/test-project/memory",
        json_body(&payload),
        true,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Project should now exist.
    let resp2 = send(state, "GET", "/v1/projects", Body::empty(), false).await;
    let bytes = axum::body::to_bytes(resp2.into_body(), usize::MAX)
        .await
        .unwrap();
    let projects: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(projects.as_array().unwrap().len(), 1);
    assert_eq!(projects[0]["slug"], "test-project");
}

#[tokio::test]
#[serial]
async fn list_notes_returns_added_note() {
    let state = make_state();
    let payload = serde_json::json!({
        "kind": "note",
        "title": "First note",
        "body": "Some context.",
        "embedding": [1.0_f32, 0.0, 0.0, 0.0],
    });
    send(
        state.clone(),
        "POST",
        "/v1/projects/proj/memory",
        json_body(&payload),
        true,
    )
    .await;

    let resp = send(
        state,
        "GET",
        "/v1/projects/proj/memory",
        Body::empty(),
        false,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let notes: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(notes.as_array().unwrap().len(), 1);
    assert_eq!(notes[0]["title"], "First note");
    assert_eq!(notes[0]["status"], "active");
}

// ── get_note ──────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn get_note_returns_404_for_unknown_id() {
    let state = make_state();
    // Create project first.
    send(
        state.clone(),
        "POST",
        "/v1/projects/p/memory",
        json_body(serde_json::json!({"kind":"note","title":"x","embedding":[0.0_f32,0.0,0.0,0.0]})),
        true,
    )
    .await;

    let resp = send(
        state,
        "GET",
        "/v1/projects/p/memory/9999",
        Body::empty(),
        false,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── archive + supersede ───────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn archive_note_hides_it_from_list() {
    let state = make_state();
    let add =
        serde_json::json!({"kind":"decision","title":"Arch","embedding":[0.0_f32,0.0,0.0,0.0]});
    let resp = send(
        state.clone(),
        "POST",
        "/v1/projects/q/memory",
        json_body(&add),
        true,
    )
    .await;
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = created["id"].as_i64().unwrap();

    let archive_resp = send(
        state.clone(),
        "POST",
        &format!("/v1/projects/q/memory/{id}/archive"),
        Body::empty(),
        false,
    )
    .await;
    assert_eq!(archive_resp.status(), StatusCode::OK);

    // Default list excludes archived.
    let list_resp = send(state, "GET", "/v1/projects/q/memory", Body::empty(), false).await;
    let bytes = axum::body::to_bytes(list_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let notes: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(notes.as_array().unwrap().is_empty());
}

// ── delete ────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn delete_note_removes_it() {
    let state = make_state();
    let add = serde_json::json!({"kind":"note","title":"Gone","embedding":[0.0_f32,0.0,0.0,0.0]});
    let resp = send(
        state.clone(),
        "POST",
        "/v1/projects/r/memory",
        json_body(&add),
        true,
    )
    .await;
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = created["id"].as_i64().unwrap();

    let del = send(
        state.clone(),
        "DELETE",
        &format!("/v1/projects/r/memory/{id}"),
        Body::empty(),
        false,
    )
    .await;
    assert_eq!(del.status(), StatusCode::OK);

    let get = send(
        state,
        "GET",
        &format!("/v1/projects/r/memory/{id}"),
        Body::empty(),
        false,
    )
    .await;
    assert_eq!(get.status(), StatusCode::NOT_FOUND);
}

// ── auth middleware ───────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn protected_endpoint_rejects_missing_token() {
    let state = common::make_test_state(4, Some("secret".into()));
    let resp = send(state, "GET", "/v1/projects", Body::empty(), false).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[serial]
async fn protected_endpoint_accepts_correct_token() {
    let state = common::make_test_state(4, Some("secret".into()));
    let req = Request::builder()
        .method("GET")
        .uri("/v1/projects")
        .header("Authorization", "Bearer secret")
        .body(Body::empty())
        .unwrap();
    let resp = router(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── search ────────────────────────────────────────────────────────────────────

/// memory/search now accepts `{"query": String}` and requires an embedder.
/// Without an embedder, the endpoint must return 400.
#[tokio::test]
#[serial]
async fn search_without_embedder_returns_400() {
    let state = make_state(); // no embedder configured

    // Create the project first.
    send(
        state.clone(),
        "POST",
        "/v1/projects/s/memory",
        json_body(
            serde_json::json!({"kind":"note","title":"alpha","embedding":[1.0_f32,0.0,0.0,0.0]}),
        ),
        true,
    )
    .await;

    // Query with text query — must return 400 (no embedder).
    let search_payload = serde_json::json!({
        "query": "how does authentication work",
        "limit": 2,
    });
    let resp = send(
        state,
        "POST",
        "/v1/projects/s/memory/search",
        json_body(&search_payload),
        true,
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "search without embedder must return 400"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["code"], "bad_request");
}

// ── /memory/since ─────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn since_endpoint_returns_entries_after_timestamp() {
    use spelunk_server::db::ServerDb;

    common::register_sqlite_vec();

    // Build a DB and insert two notes with known timestamps.
    let db = ServerDb::open(std::path::Path::new(":memory:"), 4).expect("open in-memory server db");

    // Insert a project manually so we control created_at timing.
    db.conn
        .execute(
            "INSERT INTO projects (slug, embedding_dim) VALUES ('ts-proj', 4)",
            [],
        )
        .unwrap();
    let project_id = db.conn.last_insert_rowid();

    // Note at t=1000.
    db.conn
        .execute(
            "INSERT INTO notes (project_id, kind, title, body, created_at) VALUES (?1, 'note', 'old note', '', 1000)",
            rusqlite::params![project_id],
        )
        .unwrap();

    // Note at t=2000.
    db.conn
        .execute(
            "INSERT INTO notes (project_id, kind, title, body, created_at) VALUES (?1, 'note', 'new note', '', 2000)",
            rusqlite::params![project_id],
        )
        .unwrap();

    let instance_id = db.get_or_create_instance_id().expect("instance_id in test");
    let state = AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        auth: Arc::new(ApiKeyAuth::new(None)),
        conflict_threshold: spelunk_server::default_conflict_threshold(),
        embedder: None,
        llm: None,
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(RateLimiter::new(1000, 60)),
        instance_id,
        started_by: None,
    };

    // Query with t=1500 — should return only the note at t=2000.
    let resp = send(
        state,
        "GET",
        "/v1/projects/ts-proj/memory/since?t=1500",
        Body::empty(),
        false,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let notes: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let arr = notes.as_array().expect("expected array");
    assert_eq!(
        arr.len(),
        1,
        "expected exactly 1 note after t=1500; got {arr:?}"
    );
    assert_eq!(arr[0]["title"], "new note");
    assert_eq!(arr[0]["created_at"], 2000);
}

// ── stats ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn project_stats_returns_correct_counts() {
    let state = make_state();
    send(
        state.clone(),
        "POST",
        "/v1/projects/t/memory",
        json_body(serde_json::json!({"kind":"note","title":"a","embedding":[0.0_f32,0.0,0.0,0.0]})),
        true,
    )
    .await;

    let resp = send(state, "GET", "/v1/projects/t/stats", Body::empty(), false).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let stats: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(stats["count"], 1);
    assert_eq!(stats["total"], 1);
    assert_eq!(stats["embedding_dim"], 4);
}

// ── memory_stream (SSE, ADR-026) ───────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn memory_stream_replays_with_event_framing_and_kind_filter() {
    let state = make_state();

    // Seed two notes of different kinds before opening the stream so they're
    // picked up by the initial "since 0" poll (?t= unset, no Last-Event-ID —
    // but seeding happens before the connection opens, so use ?t=0 to force
    // a full replay from id 0).
    send(
        state.clone(),
        "POST",
        "/v1/projects/p/memory",
        json_body(
            serde_json::json!({"kind":"intent","title":"intent note","embedding":[0.0_f32,0.0,0.0,0.0]}),
        ),
        true,
    )
    .await;
    send(
        state.clone(),
        "POST",
        "/v1/projects/p/memory",
        json_body(
            serde_json::json!({"kind":"decision","title":"decision note","embedding":[0.0_f32,0.0,0.0,0.0]}),
        ),
        true,
    )
    .await;

    // ?t=0 with no Last-Event-ID: notes_since(0,1) finds the first note (id
    // 1), so since_id = 0 and both notes replay on the first poll.
    let resp = send(
        state,
        "GET",
        "/v1/projects/p/memory/stream?t=0&kind=intent",
        Body::empty(),
        false,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let raw = read_sse_frames(resp, 1, std::time::Duration::from_secs(5)).await;

    // Only the `intent` note should pass the kind filter.
    assert!(raw.contains("event: memory.created"), "raw: {raw}");
    assert!(raw.contains("id: seq-0000001"), "raw: {raw}");
    assert!(raw.contains("intent note"), "raw: {raw}");
    assert!(
        !raw.contains("decision note"),
        "kind filter should drop the decision note: {raw}"
    );
}

#[tokio::test]
#[serial]
async fn memory_stream_emits_archived_event() {
    let state = make_state();

    let create = send(
        state.clone(),
        "POST",
        "/v1/projects/p/memory",
        json_body(
            serde_json::json!({"kind":"note","title":"to archive","embedding":[0.0_f32,0.0,0.0,0.0]}),
        ),
        true,
    )
    .await;
    let bytes = axum::body::to_bytes(create.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let note_id = created["id"].as_i64().expect("note id");

    send(
        state.clone(),
        "POST",
        &format!("/v1/projects/p/memory/{note_id}/archive"),
        Body::empty(),
        false,
    )
    .await;

    let resp = send(
        state,
        "GET",
        "/v1/projects/p/memory/stream?t=0",
        Body::empty(),
        false,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let raw = read_sse_frames(resp, 1, std::time::Duration::from_secs(5)).await;
    assert!(raw.contains("event: memory.archived"), "raw: {raw}");
    assert!(raw.contains(&format!("id: seq-{note_id:07}")), "raw: {raw}");
}

#[tokio::test]
#[serial]
async fn memory_stream_resumes_from_last_event_id_without_gap_or_dup() {
    let state = make_state();

    // First note exists before the client ever connects.
    send(
        state.clone(),
        "POST",
        "/v1/projects/p/memory",
        json_body(
            serde_json::json!({"kind":"note","title":"note one","embedding":[0.0_f32,0.0,0.0,0.0]}),
        ),
        true,
    )
    .await;

    // Pretend a previous connection already consumed note 1 (`seq-0000001`)
    // and disconnected. A second note is written after that.
    send(
        state.clone(),
        "POST",
        "/v1/projects/p/memory",
        json_body(
            serde_json::json!({"kind":"note","title":"note two","embedding":[0.0_f32,0.0,0.0,0.0]}),
        ),
        true,
    )
    .await;

    let resp = send_with_headers(
        state,
        "GET",
        "/v1/projects/p/memory/stream",
        &[("Last-Event-ID", "seq-0000001")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let raw = read_sse_frames(resp, 1, std::time::Duration::from_secs(5)).await;

    // Only note two (id 2) should be replayed — no gap (note one missing
    // would be a gap; note one re-appearing would be a dup).
    assert!(raw.contains("id: seq-0000002"), "raw: {raw}");
    assert!(raw.contains("note two"), "raw: {raw}");
    assert!(
        !raw.contains("note one"),
        "resume from seq-0000001 must not re-send note one: {raw}"
    );
    assert!(
        !raw.contains("seq-0000001"),
        "resume from seq-0000001 must not re-send id seq-0000001: {raw}"
    );
}
