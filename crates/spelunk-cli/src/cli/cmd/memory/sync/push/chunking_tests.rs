// Chunking, resumability, and progress-reporting tests for `super::push_local`.

use super::super::test_support::register_sqlite_vec;
use super::*;

// ── push_local end-to-end: remote_id stamping + idempotent re-sync ─────
// The local-first push path is where the server-minted
// cross-machine id is PERSISTED — stamped onto `notes.remote_id` from the
// 207 batch result — not the `RemoteMemoryBackend::add` debug-log path
// (which is the cloud-first, remote-is-store-of-record case with no local
// row). Locks in that a push stamps `remote_id` and a re-push sends nothing
// (no duplicate cloud writes, no local dupes).

// ── chunking, resumability, and progress (D1 / D4 / D5) ─────────────────

use std::collections::{HashMap, HashSet};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

// Seed `n` distinct live notes: distinct titles give distinct entity_ids (no
// dedupe collision), and each is unstamped so all `n` land in the push set.
fn seed_notes(store: &MemoryStore, n: usize) {
    for i in 0..n {
        store
            .add_note("note", &format!("T{i}"), "body", &[], &[], None, None)
            .unwrap();
    }
}

// Echoes every received entry back as `created` with a distinct cloud id, so
// `push_local` tallies and stamps exactly as a real 207 would, for any chunk
// size (a static body cannot, since the ids it must echo are minted per row).
struct EchoCreated;
impl Respond for EchoCreated {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
        let results: Vec<serde_json::Value> = body["entries"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| {
                        let ext = e["external_id"].as_str().unwrap_or_default();
                        serde_json::json!({
                            "status": "created",
                            "external_id": ext,
                            "id": format!("cloud-{ext}"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": results.len(), "skipped": 0, "failed": 0, "results": results
        }))
    }
}

// First request lands (echoed created), every later request 500s. Models a
// server that accepts the first chunk then falls over. Keyed on call count so
// it does not depend on wiremock's ordering of same-path mocks.
struct FailAfterFirst {
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl Respond for FailAfterFirst {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == 0 {
            EchoCreated.respond(request)
        } else {
            ResponseTemplate::new(500).set_body_string("overloaded")
        }
    }
}

// Echoes `created` for the first `ok` requests, then 500s every later one.
// Unlike `FailAfterFirst`, the failure can be placed on any chunk, so a test
// can prove the interrupted summary counts every chunk that landed before the
// failure and that the loop halts exactly at the failed chunk.
struct FailAfterN {
    ok: usize,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl Respond for FailAfterN {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n < self.ok {
            EchoCreated.respond(request)
        } else {
            ResponseTemplate::new(500).set_body_string("overloaded")
        }
    }
}

#[test]
fn push_batch_chunk_size_constant_is_50() {
    assert_eq!(PUSH_BATCH_CHUNK_SIZE, 50);
}

#[test]
fn chunking_splits_live_set_on_the_constant() {
    let n = PUSH_BATCH_CHUNK_SIZE;
    // (total, expected chunk count, expected last-chunk len).
    let cases = [
        (0usize, 0usize, 0usize),
        (1, 1, 1),
        (n, 1, n),
        (n + 1, 2, 1),
        (3 * n, 3, n),
        (2 * n + 7, 3, 7),
    ];
    for (total, want_chunks, want_last) in cases {
        let items: Vec<usize> = (0..total).collect();
        let chunks: Vec<&[usize]> = items.chunks(PUSH_BATCH_CHUNK_SIZE).collect();
        assert_eq!(chunks.len(), want_chunks, "chunk count for total={total}");
        assert!(
            chunks.iter().all(|c| c.len() <= PUSH_BATCH_CHUNK_SIZE),
            "no chunk exceeds N for total={total}"
        );
        if let Some((last, rest)) = chunks.split_last() {
            assert!(
                rest.iter().all(|c| c.len() == PUSH_BATCH_CHUNK_SIZE),
                "every chunk but the last is exactly N for total={total}"
            );
            assert_eq!(last.len(), want_last, "last-chunk len for total={total}");
        }
        let flat: Vec<usize> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
        assert_eq!(
            flat, items,
            "chunks cover every entry exactly once for total={total}"
        );
    }
}

#[tokio::test]
async fn push_chunks_cover_every_entry_exactly_once() {
    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    seed_notes(&store, 120); // ceil(120 / 50) = 3 chunks

    let want: HashSet<String> = store
        .rows_for_sync(false)
        .unwrap()
        .into_iter()
        .map(|r| r.uuid)
        .collect();
    assert_eq!(want.len(), 120);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(EchoCreated)
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let s = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!(
        (s.attempted, s.created, s.skipped, s.failed),
        (120, 120, 0, 0)
    );
    assert!(s.interrupted.is_none());

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 3, "ceil(120 / 50) POSTs");
    let mut seen: Vec<String> = Vec::new();
    let mut sizes: Vec<usize> = Vec::new();
    for req in &reqs {
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        let entries = body["entries"].as_array().unwrap();
        sizes.push(entries.len());
        for e in entries {
            seen.push(e["external_id"].as_str().unwrap().to_string());
        }
    }
    // A `<= N` check alone would pass an implementation chunking on the wrong
    // size (e.g. 40), so pin the exact per-request counts: full chunks up to
    // the constant, then the remainder. Requests arrive in push order.
    assert_eq!(sizes, vec![50, 50, 20], "exact per-request entry counts");
    assert_eq!(seen.len(), 120, "no entry is pushed more than once");
    assert_eq!(
        seen.into_iter().collect::<HashSet<_>>(),
        want,
        "the union of pushed ids equals the full live set"
    );
}

#[tokio::test]
async fn multi_chunk_push_threads_slug_into_every_request_path() {
    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    seed_notes(&store, 60); // 2 chunks

    let want: HashSet<String> = store
        .rows_for_sync(false)
        .unwrap()
        .into_iter()
        .map(|r| r.uuid)
        .collect();

    // The mock ONLY matches the percent-encoded slug path, so an all-created
    // push proves every chunk (the first included) carried the slug in the
    // path, which is what lazily creates/reuses the project server-side.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/acme%2Fapp/memory/batch"))
        .respond_with(EchoCreated)
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "acme/app", None, None).unwrap();

    let s = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!((s.attempted, s.created), (60, 60));
    assert!(s.interrupted.is_none());

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    let seen: HashSet<String> = reqs
        .iter()
        .flat_map(|r| {
            let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            body["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["external_id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(seen, want, "all entries observed across the batch requests");
}

#[tokio::test]
async fn interrupted_push_stops_and_resumes_from_the_remainder() {
    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    seed_notes(&store, 120); // 3 chunks: 50, 50, 20

    // ── Run 1: chunk 1 lands, chunk 2 fails (500), chunk 3 is never sent. ──
    let server1 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(FailAfterFirst {
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
        .mount(&server1)
        .await;
    let client1 = CloudSyncClient::new(&server1.uri(), "proj", None, None).unwrap();

    let s1 = push_local(&store, &client1, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert!(
        s1.interrupted.is_some(),
        "a mid-push failure marks the summary interrupted"
    );
    assert_eq!(
        (s1.attempted, s1.created, s1.skipped),
        (120, 50, 0),
        "only the first chunk landed"
    );
    assert_eq!(
        server1.received_requests().await.unwrap().len(),
        2,
        "must stop at the first failed chunk, never push the remaining chunk"
    );
    let remaining: Vec<_> = store
        .rows_for_sync(false)
        .unwrap()
        .into_iter()
        .filter(|r| r.remote_id.is_none())
        .collect();
    assert_eq!(
        remaining.len(),
        70,
        "the 50 landed rows are durably stamped"
    );

    // ── Run 2: a healthy server pushes ONLY the remainder. ──
    let server2 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(EchoCreated)
        .mount(&server2)
        .await;
    let client2 = CloudSyncClient::new(&server2.uri(), "proj", None, None).unwrap();

    let s2 = push_local(&store, &client2, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert!(s2.interrupted.is_none());
    assert_eq!(
        (s2.attempted, s2.created),
        (70, 70),
        "resume pushes only the remainder"
    );
    let reqs2 = server2.received_requests().await.unwrap();
    let pushed2: usize = reqs2
        .iter()
        .map(|r| {
            let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            body["entries"].as_array().unwrap().len()
        })
        .sum();
    assert_eq!(
        pushed2, 70,
        "resume re-pushes exactly the previously-unstamped rows"
    );
    assert!(
        store
            .rows_for_sync(false)
            .unwrap()
            .iter()
            .all(|r| r.remote_id.is_some()),
        "every row is stamped after the resume"
    );
    assert_eq!(
        store.count().unwrap(),
        120,
        "no local duplicates introduced"
    );
}

#[tokio::test]
async fn interrupted_on_a_later_chunk_counts_only_the_chunks_that_landed() {
    // The failure lands on chunk 3 of 7: `created` must equal exactly the two
    // full chunks that landed before it (not one, not three), and the loop
    // must issue no request past the failed chunk.
    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    seed_notes(&store, 340); // ceil(340 / 50) = 7 chunks

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(FailAfterN {
            ok: 2,
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let s = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert!(s.interrupted.is_some());
    assert_eq!(
        (s.attempted, s.created, s.skipped),
        (340, 100, 0),
        "created reflects exactly the two chunks that landed before the failure"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        3,
        "halts on the failed 3rd chunk; chunks 4 through 7 are never sent"
    );
    let unstamped = store
        .rows_for_sync(false)
        .unwrap()
        .into_iter()
        .filter(|r| r.remote_id.is_none())
        .count();
    assert_eq!(unstamped, 240, "only the 100 landed rows are stamped");
}

#[tokio::test]
async fn interrupted_push_skips_the_tombstone_delete_pass() {
    // Once a live chunk fails the connection is already failing, so the
    // archived-entry DELETEs must not be issued either. `delete_remote` treats
    // a 404 as success, so only a request-count check catches a regression
    // that dropped the interrupted gate on the tombstone pass.
    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    // Three live, unstamped rows form the single push chunk that will fail.
    seed_notes(&store, 3);
    // Two archived rows that DO carry a remote_id: the tombstone pass would
    // DELETE these if it were not skipped on an interrupted push.
    for tag in ["A0", "A1"] {
        let (id, _) = store
            .add_note("note", tag, "body", &[], &[], None, None)
            .unwrap();
        store.set_remote_id(id, &format!("cloud-{tag}")).unwrap();
        assert!(store.archive(id).unwrap());
    }

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(500).set_body_string("overloaded"))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let s = push_local(&store, &client, true, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert!(s.interrupted.is_some());
    assert_eq!(s.created, 0, "the only live chunk failed");

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        reqs.len(),
        1,
        "only the failing batch POST is sent; no tombstone DELETEs follow"
    );
    assert!(
        reqs[0].url.path().ends_with("/memory/batch"),
        "the single request is the batch push, not a delete"
    );
}

#[tokio::test]
async fn overlapping_repush_tallies_skipped_and_leaves_no_duplicates() {
    // Models the committed-but-unstamped overlap: a prior push persisted rows
    // server-side but the client lost the response (no `remote_id` stamped).
    // On re-push the server dedupes on external_id and returns 207 `skipped`
    // for the overlap; the client must tally those as skipped (not created),
    // stamp them, and add no local duplicates.
    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    seed_notes(&store, 4);

    let rows = store.rows_for_sync(false).unwrap();
    // First two rows are the overlap (already server-side → skipped); the last
    // two are genuinely new (created). Keyed by uuid, so row order is irrelevant.
    let mut status: HashMap<String, &str> = HashMap::new();
    status.insert(rows[0].uuid.clone(), "skipped");
    status.insert(rows[1].uuid.clone(), "skipped");
    status.insert(rows[2].uuid.clone(), "created");
    status.insert(rows[3].uuid.clone(), "created");
    let results: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "status": status[&r.uuid],
                "external_id": r.uuid,
                "id": format!("cloud-{}", r.uuid),
            })
        })
        .collect();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 2, "skipped": 2, "failed": 0, "results": results
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let s = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!((s.attempted, s.created, s.skipped, s.failed), (4, 2, 2, 0));
    assert!(s.interrupted.is_none());
    assert!(
        store
            .rows_for_sync(false)
            .unwrap()
            .iter()
            .all(|r| r.remote_id.is_some()),
        "overlap and new rows both end stamped"
    );
    assert_eq!(
        store.count().unwrap(),
        4,
        "no local duplicates from the round trip"
    );

    let s2 = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!((s2.attempted, s2.already_synced), (0, 4));
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "a fully-stamped re-push sends no batch request"
    );
}

#[tokio::test]
async fn multi_chunk_push_reports_cumulative_progress_after_each_chunk() {
    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    seed_notes(&store, 120); // 3 chunks

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(EchoCreated)
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let mut progress: Vec<(usize, usize)> = Vec::new();
    let s = push_local_reporting(
        &store,
        &client,
        false,
        false,
        &LocalEmbedPolicy::Skip,
        |done, total| {
            progress.push((done, total));
        },
    )
    .await
    .unwrap();

    assert_eq!(progress.len(), 3, "one progress emission per chunk");
    assert!(
        progress.iter().all(|&(done, total)| done <= total),
        "done never exceeds total: {progress:?}"
    );
    assert!(
        progress.windows(2).all(|w| w[0].0 <= w[1].0),
        "cumulative done never regresses across chunks: {progress:?}"
    );
    assert_eq!(
        progress.iter().map(|&(d, _)| d).collect::<Vec<_>>(),
        vec![50, 100, 120],
        "done advances cumulatively by the landed count"
    );
    assert!(progress.iter().all(|&(_, total)| total == 120));
    assert_eq!(
        progress.last().unwrap().0 as u32,
        s.created + s.skipped,
        "final reported done equals created + skipped"
    );
    assert_eq!(s.created + s.skipped, 120);
}

#[tokio::test]
async fn single_chunk_push_is_one_request_and_emits_no_progress() {
    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    seed_notes(&store, PUSH_BATCH_CHUNK_SIZE); // exactly one chunk

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(EchoCreated)
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let mut calls = 0usize;
    let s = push_local_reporting(
        &store,
        &client,
        false,
        false,
        &LocalEmbedPolicy::Skip,
        |_, _| calls += 1,
    )
    .await
    .unwrap();
    assert_eq!(calls, 0, "a single-chunk push emits no progress");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "exactly one POST for a push of N or fewer entries"
    );
    assert_eq!(s.created, PUSH_BATCH_CHUNK_SIZE as u32);
    assert!(
        s.interrupted.is_none(),
        "a clean single-chunk push is never marked interrupted"
    );
}

#[tokio::test]
async fn push_local_stamps_remote_id_and_repush_is_idempotent() {
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

    // Learn the lazily-minted external_ids up front so the mock can echo
    // them back with distinct cloud ids; `ensure_uuid` is idempotent, so the
    // push below re-derives the same uuids.
    let rows = store.rows_for_sync(false).unwrap();
    assert_eq!(rows.len(), 2);
    let (ext_a, ext_b) = (rows[0].uuid.clone(), rows[1].uuid.clone());
    let cloud_a = "01890000-0000-7000-8000-0000000000a1";
    let cloud_b = "01890000-0000-7000-8000-0000000000a2";

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 2, "skipped": 0, "failed": 0,
            "results": [
                {"status": "created", "external_id": ext_a, "id": cloud_a},
                {"status": "created", "external_id": ext_b, "id": cloud_b},
            ]
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    // First push: creates both, persists the server-minted id on each row.
    let s1 = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!((s1.attempted, s1.created, s1.skipped), (2, 2, 0));
    assert_eq!(
        store.note_id_for_remote_id(cloud_a).unwrap(),
        Some(rows[0].local_id)
    );
    assert_eq!(
        store.note_id_for_remote_id(cloud_b).unwrap(),
        Some(rows[1].local_id)
    );
    // The pull cursor is now the newest stamped id.
    assert_eq!(store.max_remote_id().unwrap().as_deref(), Some(cloud_b));

    // Second push: every row carries a `remote_id`, so the live set is empty
    // and no batch request is sent — the re-sync is a no-op. `attempted` must
    // reflect that (not the raw row count), so callers never report "Pushed
    // N" when nothing was sent.
    let s2 = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!((s2.attempted, s2.created, s2.already_synced), (0, 0, 2));
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "re-push must not hit the batch endpoint again"
    );
    // No duplicate local rows introduced by the round trip.
    assert_eq!(store.count().unwrap(), 2);
}
