use axum::http;
use serde_json::json;

use super::support::{list_notes_via_http, make_app, note_item, post_batch};

// Mixed outcomes: a pre-existing external_id (skip) alongside brand-new
// ones (create). Counts and per-item results must align, and result
// order must match input order.
#[tokio::test]
async fn batch_mixed_outcomes_counts_and_order_match() {
    let (app, _dim) = make_app(0.92);
    // Seed one existing note first.
    let (s0, b0) = post_batch(
        app.clone(),
        "mixed-proj",
        json!([note_item("seed", "id-seed")]),
    )
    .await;
    assert_eq!(s0, http::StatusCode::MULTI_STATUS, "seed: {b0}");

    let entries = json!([
        note_item("seed again", "id-seed"),
        note_item("new one", "id-new-1"),
        note_item("new two", "id-new-2"),
    ]);
    let (status, body) = post_batch(app, "mixed-proj", entries).await;
    assert_eq!(status, http::StatusCode::MULTI_STATUS, "body: {body}");
    assert_eq!(body["created"], json!(2));
    assert_eq!(body["skipped"], json!(1));
    assert_eq!(body["failed"], json!(0));

    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["external_id"], json!("id-seed"));
    assert_eq!(results[0]["status"], json!("skipped"));
    assert_eq!(results[1]["external_id"], json!("id-new-1"));
    assert_eq!(results[1]["status"], json!("created"));
    assert_eq!(results[2]["external_id"], json!("id-new-2"));
    assert_eq!(results[2]["status"], json!("created"));
}

// a dedupe-skip must still carry an id
//
// Before this fix, a "skipped" result always carried `id: null`. The
// ADR-037 P2 local relay stamps a pushed row's `remote_id` from this
// response; if a first "created" ack is buffered but the CLI's local
// stamp then fails (e.g. `SQLITE_BUSY`), a later re-push of the same row
// durably lands as "skipped": and with no id to recover from that
// response, the row was stuck outbox-pending forever, not even fixable
// by a manual `spelunk sync` (same code path). The id on a skip must be
// the SAME id the original create was assigned.

#[tokio::test]
async fn batch_skip_dedupe_hit_carries_the_same_id_as_the_original_create() {
    let (app, _dim) = make_app(0.92);
    let (s0, b0) = post_batch(
        app.clone(),
        "skip-id-proj",
        json!([note_item("first", "ext-1")]),
    )
    .await;
    assert_eq!(s0, http::StatusCode::MULTI_STATUS, "seed: {b0}");
    let created_id = b0["results"][0]["id"]
        .as_str()
        .expect("created result must carry an id")
        .to_string();

    let (status, body) = post_batch(
        app,
        "skip-id-proj",
        json!([note_item("first again", "ext-1")]),
    )
    .await;
    assert_eq!(status, http::StatusCode::MULTI_STATUS, "body: {body}");
    assert_eq!(body["results"][0]["status"], json!("skipped"));
    assert_eq!(
        body["results"][0]["id"],
        json!(created_id),
        "a dedupe-skip must carry the same id the original create was assigned, \
             not null, so a caller that lost track of the create can recover it \
             from a plain re-push: {body}"
    );
    assert_eq!(
        created_id.len(),
        36,
        "the id both branches agree on must actually be a sync_id (UUID), \
             not the raw row id: {created_id}"
    );
}

// An external_id repeated WITHIN one batch must not crash the request:
// the first occurrence creates, the second is treated as an idempotent
// skip (matching the across-request idempotency contract) rather than
// hitting the unique index and 500ing the whole batch.
#[tokio::test]
async fn batch_intra_batch_duplicate_external_id_skips_not_500() {
    let (app, _dim) = make_app(0.92);
    let entries = json!([
        note_item("first", "dup-1"),
        note_item("second (same id)", "dup-1"),
    ]);
    let (status, body) = post_batch(app.clone(), "dup-proj", entries).await;
    assert_eq!(
        status,
        http::StatusCode::MULTI_STATUS,
        "an intra-batch duplicate external_id must not 500: {body}"
    );
    assert_eq!(body["created"], json!(1));
    assert_eq!(body["skipped"], json!(1));
    assert_eq!(body["failed"], json!(0));

    let notes = list_notes_via_http(app, "dup-proj").await;
    assert_eq!(
        notes.len(),
        1,
        "exactly one row must exist for the duplicated external_id: {notes:?}"
    );
    assert_eq!(
        notes[0]["title"],
        json!("first"),
        "the FIRST occurrence in the batch wins the row"
    );
}

// Two different projects reusing the same external_id in independent
// batch requests must both create: this is the HTTP-level counterpart
// to `db::tests::remote_id_uniqueness_is_scoped_per_project_not_global`,
// proving the fix end-to-end through the route.
#[tokio::test]
async fn batch_same_external_id_different_projects_both_create() {
    let (app, _dim) = make_app(0.92);
    let (status_a, body_a) =
        post_batch(app.clone(), "proj-alpha", json!([note_item("A", "shared")])).await;
    assert_eq!(
        status_a,
        http::StatusCode::MULTI_STATUS,
        "proj-alpha: {body_a}"
    );
    assert_eq!(body_a["created"], json!(1), "proj-alpha: {body_a}");

    let (status_b, body_b) = post_batch(app, "proj-beta", json!([note_item("B", "shared")])).await;
    assert_eq!(
        status_b,
        http::StatusCode::MULTI_STATUS,
        "a different project reusing the same external_id must not 500: {body_b}"
    );
    assert_eq!(
        body_b["created"],
        json!(1),
        "proj-beta must create its own row, not collide with proj-alpha: {body_b}"
    );
}
