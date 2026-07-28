// Stamping- and counting-honesty tests for `super::push_local`: `remote_id`
// is only stamped for durably-persisted statuses, and tallies reconcile
// against `results[]` rather than trusting the server's aggregate ints.

use super::super::test_support::register_sqlite_vec;
use super::*;

// ── stamping must not trust a non-persisted status ─────────────────────
// A server can return a per-item `id` for an entry alongside a status
// that does not affirm durable persistence (aggregate `created: 0`).
// Stamping `remote_id` anyway would permanently exclude the row from
// `live` on every future push — the data could never be retried. Only
// `created`/`skipped` may stamp; a `failed` item carrying an `id` must be
// left unstamped.
#[tokio::test]
async fn push_local_does_not_stamp_remote_id_for_a_failed_status_item() {
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();

    let rows = store.rows_for_sync(false).unwrap();
    assert_eq!(rows.len(), 1);
    let ext_a = rows[0].uuid.clone();
    // The server hands back an `id` even though the entry was not
    // durably persisted (`created: 0`, status "failed").
    let cloud_a = "01890000-0000-7000-8000-0000000000b1";

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 0, "skipped": 0, "failed": 1,
            "results": [
                {"status": "failed", "external_id": ext_a, "id": cloud_a},
            ]
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let s1 = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!((s1.attempted, s1.created, s1.skipped), (1, 0, 0));

    // The row must NOT carry the id the server handed back — it stays
    // retryable on the next push.
    assert_eq!(store.note_id_for_remote_id(cloud_a).unwrap(), None);
    let rows_after = store.rows_for_sync(false).unwrap();
    assert_eq!(rows_after[0].remote_id, None);

    // A re-push must still consider this row live (not already-synced).
    let live_again: Vec<_> = rows_after
        .iter()
        .filter(|r| !r.archived && r.remote_id.is_none())
        .collect();
    assert_eq!(live_again.len(), 1, "unstamped row must remain retryable");
}

// ── counts must reconcile against results[], not the aggregate ints ────
// `BatchPushResult`'s `created`/`skipped` ints and its `results[]` array
// are independent wire fields — a server can send an aggregate
// `created: 0` for a batch whose `results[]` shows every entry durably
// persisted. A push summary built from the aggregate ints alone would
// read as "nothing landed"; it must instead read the true outcome off
// `results[].status`.
#[tokio::test]
async fn push_local_reconciles_counts_from_results_not_aggregate_ints() {
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();
    store
        .add_note("note", "Two", "second", &[], &[], None, None)
        .unwrap();

    let rows = store.rows_for_sync(false).unwrap();
    let (ext_a, ext_b) = (rows[0].uuid.clone(), rows[1].uuid.clone());
    let cloud_a = "01890000-0000-7000-8000-0000000000c1";
    let cloud_b = "01890000-0000-7000-8000-0000000000c2";

    let server = MockServer::start().await;
    // The aggregate ints understate what happened (`created: 0, skipped:
    // 0`), but `results[]` shows both entries durably persisted.
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 0, "skipped": 0, "failed": 0,
            "results": [
                {"status": "created", "external_id": ext_a, "id": cloud_a},
                {"status": "skipped", "external_id": ext_b, "id": cloud_b},
            ]
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let s1 = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    // Reconciled from `results[]`, not the misleading aggregate zeros.
    assert_eq!(
        (s1.attempted, s1.created, s1.skipped, s1.failed),
        (2, 1, 1, 0)
    );
    assert_eq!(
        store.note_id_for_remote_id(cloud_a).unwrap(),
        Some(rows[0].local_id)
    );
    assert_eq!(
        store.note_id_for_remote_id(cloud_b).unwrap(),
        Some(rows[1].local_id)
    );
}

// ── a failed item must not mask other successes in the same batch ─────
// Mixed outcome: one entry lands, one doesn't. The failed item must stay
// unstamped (retryable) while the successful one is recorded — and the
// summary must show the real partial success, not a false "nothing
// happened" (which is what reading only the aggregate `created` count
// for a batch containing any failure could produce).
#[tokio::test]
async fn push_local_partial_failure_reports_the_real_successes() {
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();
    store
        .add_note("note", "Two", "second", &[], &[], None, None)
        .unwrap();

    let rows = store.rows_for_sync(false).unwrap();
    let (ext_a, ext_b) = (rows[0].uuid.clone(), rows[1].uuid.clone());
    let cloud_a = "01890000-0000-7000-8000-0000000000d1";
    let cloud_b = "01890000-0000-7000-8000-0000000000d2";

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 1,
            "results": [
                {"status": "created", "external_id": ext_a, "id": cloud_a},
                {"status": "failed", "external_id": ext_b, "id": cloud_b},
            ]
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let s1 = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!(
        (s1.attempted, s1.created, s1.skipped, s1.failed),
        (2, 1, 0, 1),
        "attempted must stay 2 (not read as nothing-to-push) and the \
             genuine success must be visible alongside the failure"
    );
    // The successful row is stamped...
    assert_eq!(
        store.note_id_for_remote_id(cloud_a).unwrap(),
        Some(rows[0].local_id)
    );
    // ...the failed one is not, and remains retryable.
    assert_eq!(store.note_id_for_remote_id(cloud_b).unwrap(), None);
    let rows_after = store.rows_for_sync(false).unwrap();
    let live_again: Vec<_> = rows_after
        .iter()
        .filter(|r| !r.archived && r.remote_id.is_none())
        .collect();
    assert_eq!(live_again.len(), 1, "failed row must remain retryable");
}

// ── push_local's counting stays honest on a total-failure batch ────────
// `push_local` itself just reports honest counts (Bug 1/3's fix); it is
// the command layer (`memory_push` / `memory_sync`) that decides whether
// those counts mean "Done"/"Sync complete" or a hard failure, and that
// command-layer decision (the `bail!` that gives the CLI its non-zero
// exit) is covered end to end by the subprocess tests in
// `crates/spelunk-cli/tests/memory_push_sync_total_failure.rs`, not here.
// This test only pins `push_local`'s own return value for the all-failed
// shape those command-layer tests depend on.
#[tokio::test]
async fn push_local_total_failure_reports_zero_created_and_skipped() {
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();
    store
        .add_note("note", "Two", "second", &[], &[], None, None)
        .unwrap();

    let rows = store.rows_for_sync(false).unwrap();
    let (ext_a, ext_b) = (rows[0].uuid.clone(), rows[1].uuid.clone());

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 0, "skipped": 0, "failed": 2,
            "results": [
                {"status": "failed", "external_id": ext_a, "id": serde_json::Value::Null},
                {"status": "failed", "external_id": ext_b, "id": serde_json::Value::Null},
            ]
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let s1 = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!(
        (s1.attempted, s1.created, s1.skipped, s1.failed),
        (2, 0, 0, 2),
        "total failure: attempted > 0 but nothing durably landed"
    );
    // Neither row is stamped — both remain retryable.
    let rows_after = store.rows_for_sync(false).unwrap();
    assert!(rows_after.iter().all(|r| r.remote_id.is_none()));
}
