use axum::http;
use serde_json::json;

use super::support::{get_status_and_json, make_app, post_note};

// ── GET /memory/since: dual mode (`t` legacy vs `since_id` cursor) ──

// Regression: the pre-existing `?t=` mode must still return a bare
// array, unchanged by the new `since_id` mode.
#[tokio::test]
async fn memory_since_t_mode_still_returns_bare_array() {
    let (app, _dim) = make_app(0.92);
    let (status, body) =
        post_note(app.clone(), "since-t-proj", "A", vec![1.0, 0.0, 0.0, 0.0]).await;
    assert_eq!(status, http::StatusCode::CREATED, "seed: {body}");

    let (status, body) =
        get_status_and_json(app, "/v1/projects/since-t-proj/memory/since?t=0").await;
    assert_eq!(status, http::StatusCode::OK, "body: {body}");
    assert!(
        body.is_array(),
        "`t` mode must return a bare array, not an object: {body}"
    );
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["title"], json!("A"));
    assert!(
        body[0].get("entries").is_none(),
        "must not be wrapped in the since_id envelope: {body}"
    );
}

// A request with neither `t` nor `since_id` is a 400, matching the
// pre-existing "missing `t`" contract (now generalized to either param).
#[tokio::test]
async fn memory_since_missing_both_params_returns_400() {
    let (app, _dim) = make_app(0.92);
    // Seed the project first: an unknown project 404s before the
    // t/since_id check ever runs, which would test the wrong thing.
    let (status, body) = post_note(
        app.clone(),
        "since-missing-proj",
        "A",
        vec![1.0, 0.0, 0.0, 0.0],
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "seed: {body}");

    let (status, body) =
        get_status_and_json(app, "/v1/projects/since-missing-proj/memory/since").await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "body: {body}");
}

// `since_id` mode returns `{entries, count}`, with `id` set to the
// note's `sync_id` (a UUID), not its integer note id: this is the
// shape `CloudSyncClient::pull_since`/`RemoteEntry` expects.
#[tokio::test]
async fn memory_since_id_mode_returns_entries_envelope() {
    let (app, _dim) = make_app(0.92);
    let (status, body) =
        post_note(app.clone(), "since-id-proj", "A", vec![1.0, 0.0, 0.0, 0.0]).await;
    assert_eq!(status, http::StatusCode::CREATED, "seed: {body}");

    let (status, body) = get_status_and_json(
        app,
        "/v1/projects/since-id-proj/memory/since?since_id=00000000-0000-0000-0000-000000000000",
    )
    .await;
    assert_eq!(status, http::StatusCode::OK, "body: {body}");
    assert_eq!(body["count"], json!(1), "body: {body}");
    let entries = body["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["title"], json!("A"));
    let id = entries[0]["id"].as_str().expect("id must be a string");
    assert_eq!(
        id.len(),
        36,
        "id must be a UUID (sync_id), not an integer: {id}"
    );
}

// `since_id` takes precedence when both `t` and `since_id` are
// supplied: a `t` far in the past must not switch the response back to
// the bare-array shape.
#[tokio::test]
async fn memory_since_id_takes_precedence_over_t_when_both_given() {
    let (app, _dim) = make_app(0.92);
    let (status, body) = post_note(
        app.clone(),
        "since-both-proj",
        "A",
        vec![1.0, 0.0, 0.0, 0.0],
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "seed: {body}");

    let (status, body) = get_status_and_json(
            app,
            "/v1/projects/since-both-proj/memory/since?t=0&since_id=00000000-0000-0000-0000-000000000000",
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "body: {body}");
    assert!(
        body.get("entries").is_some(),
        "since_id must win over t when both are given: {body}"
    );
}

// The `since_id` cursor is exclusive and advances correctly: pulling
// again with the previous response's max id returns nothing further.
#[tokio::test]
async fn memory_since_id_cursor_advances_and_is_exclusive() {
    let (app, _dim) = make_app(0.92);
    let (status, body) = post_note(
        app.clone(),
        "since-cursor-proj",
        "A",
        vec![1.0, 0.0, 0.0, 0.0],
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "seed: {body}");

    let nil =
        "/v1/projects/since-cursor-proj/memory/since?since_id=00000000-0000-0000-0000-000000000000";
    let (status, body) = get_status_and_json(app.clone(), nil).await;
    assert_eq!(status, http::StatusCode::OK);
    let cursor = body["entries"][0]["id"].as_str().expect("id").to_string();

    let uri = format!("/v1/projects/since-cursor-proj/memory/since?since_id={cursor}");
    let (status, body) = get_status_and_json(app, &uri).await;
    assert_eq!(status, http::StatusCode::OK, "body: {body}");
    assert_eq!(
        body["count"],
        json!(0),
        "re-querying with the last-seen cursor must return nothing further: {body}"
    );
}
