//! End-to-end CLI-to-OSS-server sync test.
//!
//! This is the test that has never existed: it drives a real bound
//! `spelunk-server` instance (plaintext loopback) through the **actual CLI
//! client code**: `spelunk_core::storage::{CloudSyncClient, BatchPushItem}`,
//! the same types `spelunk memory push` / `spelunk sync` use, instead of a
//! hand-rolled request. It exists so a future wire-contract drift between the
//! CLI and this server fails a test instead of shipping silently (as the
//! `POST /memory/batch` 405 did).
//!
//! Coverage:
//! - push a batch of local-shaped memories → stored, per-entry "created".
//! - re-push the same batch → idempotent on `external_id` (server-side
//!   `remote_id`): all "skipped", zero duplicates.
//! - since roundtrip (`?t=`, `spelunk memory since`'s contract): the pushed
//!   entries are retrievable via `GET /memory/since`, proving the
//!   `/memory/batch` literal route did not shadow (or get shadowed by) the
//!   pre-existing `/memory/since` route.
//! - since roundtrip (`?since_id=`, `spelunk sync`'s pull-half contract):
//!   `CloudSyncClient::pull_since` — the same client code `spelunk sync`
//!   uses — retrieves pushed entries via the cursor mode, including an entry
//!   created via the single-note POST route (`remote_id = NULL`), and a
//!   second pull with the advanced cursor returns nothing further.

mod common;

use std::net::SocketAddr;

use serde::Deserialize;
use spelunk_core::storage::{BatchPushItem, CloudSyncClient};
use spelunk_server::router;

/// Bind a real ephemeral loopback listener, serve `state`'s router on it, and
/// return the base URL. Mirrors the bind/serve pattern in `tls_serve.rs` minus
/// TLS: plaintext loopback is the OSS team-server deployment this test
/// targets (ADR-058).
async fn spawn_plaintext_server(state: spelunk_server::AppState) -> String {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral loopback port");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("serve");
    });
    format!("http://{addr}")
}

/// Minimal mirror of the server's `ServerNote` wire shape: just the fields
/// this test asserts on. Matches what `spelunk memory since` (the CLI command
/// that actually targets this endpoint's real `t=`/`Vec<ServerNote>`
/// contract) deserializes.
#[derive(Debug, Deserialize)]
struct SinceNote {
    title: String,
    #[serde(default)]
    tags: Vec<String>,
}

async fn fetch_since(base_url: &str, project_slug: &str, t: i64) -> Vec<SinceNote> {
    let url = format!(
        "{base_url}/v1/projects/{}/memory/since",
        urlencoding_slug(project_slug)
    );
    reqwest::Client::new()
        .get(&url)
        .query(&[("t", t.to_string())])
        .send()
        .await
        .expect("GET /memory/since")
        .error_for_status()
        .expect("since must 200")
        .json()
        .await
        .expect("parse /memory/since response")
}

/// The project slug in these tests has no reserved characters, so a plain
/// pass-through is enough (avoids pulling in a percent-encoding dep just for
/// the test).
fn urlencoding_slug(slug: &str) -> &str {
    slug
}

#[tokio::test]
#[serial_test::serial]
async fn cli_push_then_repush_is_idempotent_then_since_roundtrips() {
    common::register_sqlite_vec();
    let state = common::make_test_state(4, None);
    let base_url = spawn_plaintext_server(state).await;
    // No `/` in the slug: the manual `reqwest` assertion calls below build the
    // URL by hand and don't go through the CLI's percent-encoding helper.
    let project = "acme-widget";

    // Real CLI client code path: the exact type `spelunk memory push` and
    // `spelunk sync` construct.
    let client = CloudSyncClient::new(&base_url, project, None, None)
        .expect("CloudSyncClient::new against a keyless plaintext loopback server");

    let items = |suffix: &str| {
        vec![
            BatchPushItem {
                kind: "decision".into(),
                title: format!("Decision {suffix}"),
                body: Some("why we did it".into()),
                external_id: format!("uuid-a-{suffix}"),
                source_commit: Some("deadbeef".into()),
                vector: None,
                vector_model: None,
                vector_precision: None,
            },
            BatchPushItem {
                kind: "note".into(),
                title: format!("Note {suffix}"),
                body: None,
                external_id: format!("uuid-b-{suffix}"),
                source_commit: None,
                vector: None,
                vector_model: None,
                vector_precision: None,
            },
        ]
    };

    // ── First push: both entries created ────────────────────────────────────
    let res1 = client
        .push_batch(items("1"))
        .await
        .expect("first push_batch must succeed against the OSS server");
    assert_eq!(
        (res1.created, res1.skipped, res1.failed),
        (2, 0, 0),
        "first push must create both entries: {res1:?}"
    );
    for r in &res1.results {
        assert_eq!(r.status, "created", "result: {r:?}");
        assert!(r.id.is_some(), "a created entry must carry a server id");
    }

    // ── Re-push the identical batch: idempotent, no duplicates ──────────────
    let res2 = client
        .push_batch(items("1"))
        .await
        .expect("re-push must succeed (idempotent, not an error)");
    assert_eq!(
        (res2.created, res2.skipped, res2.failed),
        (0, 2, 0),
        "re-push of the same external_ids must skip, not duplicate: {res2:?}"
    );

    // Confirm no duplicates server-side via the list endpoint.
    let list_url = format!("{base_url}/v1/projects/{project}/memory?limit=50");
    let body: serde_json::Value = reqwest::get(&list_url)
        .await
        .expect("GET /memory")
        .json()
        .await
        .expect("parse /memory list");
    let notes = body["entries"]
        .as_array()
        .expect("list envelope must carry an `entries` array");
    assert_eq!(
        notes.len(),
        2,
        "re-push must not create duplicate rows: {notes:?}"
    );

    // ── since roundtrip: the pushed entries are readable back out, and the
    // new /memory/batch literal route did not shadow /memory/since. ────────
    let since_notes = fetch_since(&base_url, project, 0).await;
    assert_eq!(since_notes.len(), 2, "since must return both pushed notes");
    let titles: Vec<&str> = since_notes.iter().map(|n| n.title.as_str()).collect();
    assert!(titles.contains(&"Decision 1"));
    assert!(titles.contains(&"Note 1"));
    // `source_commit` has no dedicated column; it round-trips as a `git:`
    // tag, the same convention `harvested_shas` reads.
    let decision = since_notes
        .iter()
        .find(|n| n.title == "Decision 1")
        .expect("decision note present");
    assert!(
        decision.tags.iter().any(|t| t == "git:deadbeef"),
        "source_commit must round-trip as a git: tag: {:?}",
        decision.tags
    );

    // ── A different, brand-new batch is still additive (not swallowed) ─────
    let res3 = client
        .push_batch(items("2"))
        .await
        .expect("push of a distinct batch must succeed");
    assert_eq!((res3.created, res3.skipped, res3.failed), (2, 0, 0));
}

/// A batch push against an unknown project slug lazily creates the project,
/// exactly like the single-note `add_note` route already does: batch push
/// must not require the project to pre-exist.
#[tokio::test]
#[serial_test::serial]
async fn cli_push_lazily_creates_the_project() {
    common::register_sqlite_vec();
    let state = common::make_test_state(4, None);
    let base_url = spawn_plaintext_server(state).await;

    let client = CloudSyncClient::new(&base_url, "brand-new/project", None, None).unwrap();
    let res = client
        .push_batch(vec![BatchPushItem {
            kind: "note".into(),
            title: "First ever entry".into(),
            body: None,
            external_id: "uuid-first".into(),
            source_commit: None,
            vector: None,
            vector_model: None,
            vector_precision: None,
        }])
        .await
        .expect("push to a not-yet-existing project must succeed (lazy create)");
    assert_eq!((res.created, res.skipped, res.failed), (1, 0, 0));
}

/// Two concurrent, identical batches against a real bound server. `AppState`
/// guards the whole handler body behind one `tokio::Mutex<ServerDb>`, so in
/// practice these fully serialize rather than racing inside SQLite — but
/// that serialization is itself the thing under test: it must produce a
/// clean 1-create/1-skip split with no 500s and no duplicate row, not a
/// crash from two overlapping in-flight requests sharing state.
#[tokio::test]
#[serial_test::serial]
async fn concurrent_identical_batches_settle_without_duplicates_or_500s() {
    common::register_sqlite_vec();
    let state = common::make_test_state(4, None);
    let base_url = spawn_plaintext_server(state).await;
    let project = "concurrent-proj";

    let items = || {
        vec![BatchPushItem {
            kind: "note".into(),
            title: "Racer".into(),
            body: None,
            external_id: "race-1".into(),
            source_commit: None,
            vector: None,
            vector_model: None,
            vector_precision: None,
        }]
    };

    let client_a = CloudSyncClient::new(&base_url, project, None, None).unwrap();
    let client_b = CloudSyncClient::new(&base_url, project, None, None).unwrap();

    let (res_a, res_b) = tokio::join!(client_a.push_batch(items()), client_b.push_batch(items()));

    // Neither call may error (no 500 leaking through as a client-side error).
    let res_a = res_a.expect("first concurrent push must not error/500");
    let res_b = res_b.expect("second concurrent push must not error/500");

    // Between the two racing requests, exactly one create and one skip.
    let total_created = res_a.created + res_b.created;
    let total_skipped = res_a.skipped + res_b.skipped;
    assert_eq!(
        (total_created, total_skipped, res_a.failed + res_b.failed),
        (1, 1, 0),
        "exactly one of the two racing pushes must create, the other must skip: a={res_a:?} b={res_b:?}"
    );

    // The store itself must never end up with two rows for one external_id.
    let list_url = format!("{base_url}/v1/projects/{project}/memory?limit=50");
    let body: serde_json::Value = reqwest::get(&list_url)
        .await
        .expect("GET /memory")
        .json()
        .await
        .expect("parse /memory list");
    let notes = body["entries"]
        .as_array()
        .expect("list envelope must carry an `entries` array");
    assert_eq!(
        notes.len(),
        1,
        "a race on the same external_id must never produce two rows: {notes:?}"
    );
}

// ── `spelunk sync` pull half: CloudSyncClient::pull_since ──────────────────

/// Create a note via the single-note `POST /memory` route (not batch), the
/// same shape a pre-existing note on an OSS server would have: `remote_id`
/// stays NULL, since only a batch push (or a future sync) ever sets it.
async fn post_single_note(base_url: &str, project: &str, title: &str) {
    let url = format!("{base_url}/v1/projects/{project}/memory");
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({"kind": "note", "title": title, "body": ""}))
        .send()
        .await
        .expect("POST /memory");
    assert!(
        resp.status().is_success(),
        "seeding a single note must succeed: {}",
        resp.status()
    );
}

/// The full pull-side round trip through the real CLI client code:
/// `CloudSyncClient::pull_since` must retrieve entries pushed via
/// `POST /memory/batch` (this is the bug the batch-push story didn't fix —
/// pull spoke a different contract than this server's `/memory/since`
/// handler understood). Also covers the `remote_id = NULL` gap: a note
/// created via the single-note POST route (predating any batch push, so it
/// never got a client-supplied `external_id`) must still surface, because
/// `sync_id` — the identity `pull_since` actually cursors on — is minted
/// server-side for every note regardless of how it was created.
#[tokio::test]
#[serial_test::serial]
async fn cli_pull_since_retrieves_pushed_and_legacy_entries_then_cursor_advances() {
    common::register_sqlite_vec();
    let state = common::make_test_state(4, None);
    let base_url = spawn_plaintext_server(state).await;
    let project = "pull-proj";

    // A "legacy" entry: created before any batch push ever touched this
    // project, so it has no `remote_id` — this is the gap the contract
    // mismatch left unhandled.
    post_single_note(&base_url, project, "Legacy note").await;

    let push_client = CloudSyncClient::new(&base_url, project, None, None).unwrap();
    let res = push_client
        .push_batch(vec![BatchPushItem {
            kind: "decision".into(),
            title: "Pushed decision".into(),
            body: Some("why we did it".into()),
            external_id: "uuid-pushed-1".into(),
            source_commit: Some("cafef00d".into()),
            vector: None,
            vector_model: None,
            vector_precision: None,
        }])
        .await
        .expect("push_batch must succeed");
    assert_eq!((res.created, res.skipped, res.failed), (1, 0, 0));

    // ── Pull from scratch (no cursor yet): both entries must come back ─────
    let pull_client = CloudSyncClient::new(&base_url, project, None, None).unwrap();
    let entries = pull_client
        .pull_since(None)
        .await
        .expect("pull_since must succeed against the OSS server");
    assert_eq!(
        entries.len(),
        2,
        "both the legacy (remote_id=NULL) note and the pushed entry must surface: {entries:?}"
    );
    let titles: std::collections::HashSet<&str> =
        entries.iter().map(|e| e.title.as_str()).collect();
    assert!(titles.contains("Legacy note"), "entries: {entries:?}");
    assert!(titles.contains("Pushed decision"), "entries: {entries:?}");

    // Every entry's `id` must be a UUID (the sync_id), not a small integer
    // string — proves the response is the since_id-mode envelope, not the
    // legacy `?t=` shape.
    for e in &entries {
        assert_eq!(e.id.len(), 36, "id must be a UUID: {e:?}");
    }
    assert!(!entries.iter().any(|e| e.is_archived()));

    // source_commit round-trips through the git:<sha> tag convention.
    let pushed = entries
        .iter()
        .find(|e| e.title == "Pushed decision")
        .expect("pushed entry present");
    assert_eq!(pushed.source_commit.as_deref(), Some("cafef00d"));

    // ── Cursor advances: pulling again with the max id seen returns nothing
    // further, proving the cursor is exclusive and doesn't loop forever. ───
    let max_id = entries.iter().map(|e| e.id.as_str()).max().unwrap();
    let further = pull_client
        .pull_since(Some(max_id))
        .await
        .expect("second pull_since must succeed");
    assert!(
        further.is_empty(),
        "re-pulling from the max already-seen id must return nothing further: {further:?}"
    );
}
