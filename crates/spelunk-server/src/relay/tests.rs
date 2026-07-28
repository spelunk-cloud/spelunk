// Tests for the ADR-037 P2 local relay (see `mod.rs`'s module docs for the
// full push/pull/SSE contract these exercise).

use super::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn entry(ext: &str) -> RelayPushEntry {
    RelayPushEntry {
        kind: "decision".into(),
        title: "T".into(),
        body: Some("B".into()),
        external_id: ext.into(),
        source_commit: None,
    }
}

// ── item 18: zero registered projects means zero outbound traffic ──────

#[tokio::test]
async fn empty_registry_makes_no_outbound_calls_and_starts_no_sessions() {
    let registry = RelayRegistry::new();
    assert_eq!(registry.session_count().await, 0);

    // Polling an unregistered project must not create a session either.
    let resp = registry.poll("https://team.example", "proj").await;
    assert!(resp.push_results.is_empty());
    assert!(resp.pulled.is_empty());
    assert_eq!(registry.session_count().await, 0);
}

// ── item 12: push reuses CloudSyncClient/BatchPushItem ─────────────────

#[tokio::test]
async fn push_lands_on_the_team_server_and_is_pollable_and_reoffered_until_acked() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0,
            "results": [{"status": "created", "external_id": "e1", "id": "cloud-1"}]
        })))
        .mount(&server)
        .await;
    // No SSE mount: the pull loop's initial catch-up (`/memory/since`)
    // must not block registration or the push itself.
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/since"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"entries": [], "count": 0})),
        )
        .mount(&server)
        .await;

    let registry = RelayRegistry::new();
    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![entry("e1")],
        })
        .await
        .unwrap();

    assert_eq!(registry.session_count().await, 1);

    // The remote push happens in a detached background task; poll until
    // it lands rather than assuming a fixed sleep.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut got = RelayPollResponse::default();
    while std::time::Instant::now() < deadline {
        got = registry.poll(&server.uri(), "proj").await;
        if !got.push_results.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(got.push_results.len(), 1);
    assert_eq!(got.push_results[0].external_id, "e1");
    assert_eq!(got.push_results[0].remote_id.as_deref(), Some("cloud-1"));
    assert_eq!(got.push_results[0].status, "created");
    assert!(got.last_synced_at.is_some());

    // A second poll before any ack must return the SAME result again —
    // this is the fix for the destructive-drain data-loss bug: a poll
    // used to clear the buffer in the same call, so a CLI-side apply
    // failure after this first poll would have permanently stranded the
    // row pending forever (nothing left to retry against).
    let second = registry.poll(&server.uri(), "proj").await;
    assert_eq!(
        second.push_results.len(),
        1,
        "an unacked result must still be offered on the next poll"
    );
    assert_eq!(second.push_results[0].external_id, "e1");

    // Only an explicit ack retires it.
    registry
        .ack(&server.uri(), "proj", &["e1".to_string()], &[])
        .await;
    let third = registry.poll(&server.uri(), "proj").await;
    assert!(
        third.push_results.is_empty(),
        "an acked result must not be offered again"
    );
}

#[tokio::test]
async fn push_with_empty_entries_is_a_noop_no_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/since"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"entries": [], "count": 0})),
        )
        .mount(&server)
        .await;

    let registry = RelayRegistry::new();
    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![],
        })
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;
    let got = registry.poll(&server.uri(), "proj").await;
    assert!(
        got.push_results.is_empty(),
        "empty entries must never reach the batch endpoint, so nothing is stamped: {got:?}"
    );
}

// ── item 12/16: pull catch-up via /memory/since, cursor round-trips ────

#[tokio::test]
async fn registration_seeds_cursor_and_catch_up_advances_it_and_buffers_pulled_rows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/since"))
        .and(wiremock::matchers::query_param(
            "since_id",
            "01890000-0000-7000-8000-000000000001",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [{
                "id": "01890000-0000-7000-8000-000000000002",
                "kind": "note", "title": "Remote",
                "body": "body", "created_at": "2026-06-19T01:00:00Z"
            }],
            "count": 1
        })))
        .mount(&server)
        .await;

    let registry = RelayRegistry::new();
    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: Some("01890000-0000-7000-8000-000000000001".to_string()),
            entries: vec![],
        })
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut got = RelayPollResponse::default();
    while std::time::Instant::now() < deadline {
        got = registry.poll(&server.uri(), "proj").await;
        if !got.pulled.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(got.pulled.len(), 1);
    assert_eq!(
        got.pulled[0].remote_id,
        "01890000-0000-7000-8000-000000000002"
    );
    assert!(!got.pulled[0].archived);
}

// ── founder review (PR #728): pull-side data loss without a restart ────
//
// The bug: `GET /local/relay/poll` used to destructively drain buffered
// pulled rows (`std::mem::take`) while the CLI's `apply_remote_note` call
// can fail (SQLITE_BUSY, a killed process) without re-buffering — and the
// session's pull cursor had already advanced past the row when it was
// first buffered, so a restart-free retry would never re-offer it. This
// pins the fix directly at the relay level, independent of any CLI-side
// failure injection: a poll never clears the buffer by itself, so a CLI
// that never acks (modelling "poll succeeded, the local apply after it
// failed") must see the exact same row again, indefinitely, across many
// polls and additional catch-up cycles — never silently dropped. Fails
// against the pre-fix `drain`-on-poll code (the second poll below would
// return empty), passes after.

#[tokio::test]
async fn a_pulled_row_survives_repeated_polls_when_the_cli_never_acks_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/since"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [{
                "id": "01890000-0000-7000-8000-000000000002",
                "kind": "note", "title": "Remote",
                "body": "body", "created_at": "2026-06-19T01:00:00Z"
            }],
            "count": 1
        })))
        .mount(&server)
        .await;

    let registry = RelayRegistry::new();
    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![],
        })
        .await
        .unwrap();

    // Wait for the initial catch-up to buffer the row.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !registry.poll(&server.uri(), "proj").await.pulled.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the row never arrived to begin with"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Simulate "the CLI polled it, but its local apply failed" by simply
    // never acking, across several more polls (each of which also lets
    // the background pull loop run another catch-up cycle against a
    // cursor that must not have moved past this still-unacked row).
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let got = registry.poll(&server.uri(), "proj").await;
        assert_eq!(
            got.pulled.len(),
            1,
            "an unacked pulled row must never disappear from the buffer"
        );
        assert_eq!(
            got.pulled[0].remote_id,
            "01890000-0000-7000-8000-000000000002"
        );
    }

    // Once the CLI confirms it actually applied the row, it is retired.
    registry
        .ack(
            &server.uri(),
            "proj",
            &[],
            &["01890000-0000-7000-8000-000000000002".to_string()],
        )
        .await;
    let after_ack = registry.poll(&server.uri(), "proj").await;
    assert!(
        after_ack.pulled.is_empty(),
        "an acked pulled row must not be offered again"
    );
}

#[tokio::test]
async fn a_later_stale_since_cursor_never_regresses_a_session_that_moved_past_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/since"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"entries": [], "count": 0})),
        )
        .mount(&server)
        .await;

    let registry = RelayRegistry::new();
    // First registration seeds a cursor ahead of what the second (slower,
    // stale) CLI invocation will offer.
    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: Some("01890000-0000-7000-8000-000000000005".to_string()),
            entries: vec![],
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let session = registry.get_or_create(&server.uri(), "proj").await;
    assert_eq!(
        session.inner.lock().await.cursor.as_deref(),
        Some("01890000-0000-7000-8000-000000000005")
    );

    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: Some("01890000-0000-7000-8000-000000000001".to_string()),
            entries: vec![],
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        session.inner.lock().await.cursor.as_deref(),
        Some("01890000-0000-7000-8000-000000000005"),
        "a stale, earlier cursor must never regress the session's own progress"
    );
}

// ── item 17: one project's relay failure never affects another's ───────

#[tokio::test]
async fn one_sessions_push_failure_does_not_affect_another_sessions_push() {
    let bad_server = MockServer::start().await;
    // No mock mounted at all: every request 404s.
    let good_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0,
            "results": [{"status": "created", "external_id": "e1", "id": "cloud-1"}]
        })))
        .mount(&good_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/since"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"entries": [], "count": 0})),
        )
        .mount(&good_server)
        .await;

    let registry = RelayRegistry::new();
    registry
        .push(RelayPushRequest {
            server_url: bad_server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![entry("e1")],
        })
        .await
        .unwrap();
    registry
        .push(RelayPushRequest {
            server_url: good_server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![entry("e2")],
        })
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut good = RelayPollResponse::default();
    while std::time::Instant::now() < deadline {
        good = registry.poll(&good_server.uri(), "proj").await;
        if !good.push_results.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        good.push_results.len(),
        1,
        "the healthy session's push must land regardless of the other session's failure"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut bad = RelayPollResponse::default();
    while std::time::Instant::now() < deadline {
        bad = registry.poll(&bad_server.uri(), "proj").await;
        if bad.last_error.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        bad.last_error.is_some(),
        "the failing session records its own error instead of panicking or hanging"
    );
    assert_eq!(registry.session_count().await, 2);
}

// ── item 22: no cross-project SSE/pull leakage ──────────────────────────
// Two projects on the SAME team server: a note pushed to one must never
// appear in the other's pulled buffer. `RelayKey` is `(server_url,
// project_id)`, so distinct project ids always get distinct sessions with
// independent cursors/buffers; this pins that at the observable
// push+pull level rather than trusting the key type alone.

#[tokio::test]
async fn pulled_rows_never_leak_across_projects_on_the_same_team_server() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj-x/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0,
            "results": [{"status": "created", "external_id": "ex", "id": "cloud-x"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj-x/memory/since"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [{
                "id": "01890000-0000-7000-8000-0000000000x1",
                "kind": "note", "title": "X-only", "body": "b",
                "created_at": "2026-06-19T01:00:00Z"
            }],
            "count": 1
        })))
        .mount(&server)
        .await;
    // proj-y's own /memory/since must never see proj-x's entry (a distinct
    // mock, scoped to a different path, proves the request itself is
    // correctly project-scoped, not just that this mock happens to return
    // nothing).
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj-y/memory/since"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"entries": [], "count": 0})),
        )
        .mount(&server)
        .await;

    let registry = RelayRegistry::new();
    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj-x".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![entry("ex")],
        })
        .await
        .unwrap();
    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj-y".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![],
        })
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut x = RelayPollResponse::default();
    while std::time::Instant::now() < deadline {
        x = registry.poll(&server.uri(), "proj-x").await;
        if !x.pulled.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(x.pulled.len(), 1, "proj-x must see its own entry");
    assert_eq!(x.pulled[0].title, "X-only");

    tokio::time::sleep(Duration::from_millis(200)).await;
    let y = registry.poll(&server.uri(), "proj-y").await;
    assert!(
        y.pulled.is_empty(),
        "proj-y must never see proj-x's pulled entry: {:?}",
        y.pulled
    );
}

// ── founder review (PR #728): SSE frames decode across chunk boundaries ─
//
// `stream_once` used to decode each raw HTTP chunk in isolation
// (`String::from_utf8_lossy(&chunk)` per iteration, before frame
// boundaries were known), which could corrupt a multi-byte UTF-8
// character or a `Last-Event-ID` value split across two chunks. The fix
// accumulates raw bytes and only decodes once a complete `\n\n`-
// terminated frame has been assembled. This pins the byte-safety
// primitive the fix relies on: `find_double_newline` must locate the
// terminator by raw bytes, never by decoding (which would panic or
// silently corrupt data on a not-yet-complete multi-byte sequence sitting
// at the search boundary).

#[test]
fn find_double_newline_locates_the_terminator_around_a_multibyte_char() {
    // "café" — 'é' is the two-byte UTF-8 sequence 0xC3 0xA9. Split the
    // buffer such that this sequence itself sits right before the
    // terminator, the exact shape a chunk-boundary split could produce.
    let mut buf = b"data: caf\xc3\xa9\n\n".to_vec();
    let pos = find_double_newline(&buf).expect("terminator must be found");
    let frame = String::from_utf8_lossy(&buf[..pos + 2]).into_owned();
    assert_eq!(
        frame, "data: café\n\n",
        "the multibyte character must decode intact"
    );

    // No terminator yet (a chunk boundary landed mid-frame, even mid-
    // character): must not find a false match or panic on invalid UTF-8
    // in the not-yet-complete tail.
    buf.truncate(buf.len() - 2); // drop the "\n\n"
    assert_eq!(find_double_newline(&buf), None);
    let mid_char = &buf[..buf.len() - 1]; // split inside 'é''s 2-byte sequence
    assert_eq!(find_double_newline(mid_char), None);
}

// ── oversized/malformed SSE frame errors instead of growing forever ────
//
// A team `server_url` is whatever a project happens to be configured
// with (cloud-api, another spelunk-server, or, if misconfigured, anything
// else); this pins that a peer sending an unterminated line larger than
// `MAX_SSE_BUFFER_BYTES` makes `stream_once` return an error (which
// `run_pull_loop` already turns into `record_error` + backoff + retry,
// never a panic) instead of buffering without bound for as long as the
// connection stays open.

#[tokio::test]
async fn oversized_sse_frame_without_terminator_errors_instead_of_growing_forever() {
    let server = MockServer::start().await;
    let oversized_line = vec![b'x'; MAX_SSE_BUFFER_BYTES + 4096];
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_bytes(oversized_line),
        )
        .mount(&server)
        .await;

    let session = Arc::new(RelaySession::new(server.uri(), "proj".to_string()));
    let result = stream_once(&session).await;
    assert!(
        result.is_err(),
        "an unterminated frame past the buffer cap must error, not hang or \
         grow without bound"
    );
}

// ── item 13: the reconciler never opens a project's memory.db ──────────
//
// Every public entry point on `RelayRegistry` (`push`, `poll`) takes only
// `server_url` / `project_id` / entry data — never a filesystem path —
// and every type in this module is one of those or wraps `CloudSyncClient`
// (an HTTP client). There is no `MemoryStore`/SQLite-path parameter
// anywhere in this module's public surface for a caller to even supply,
// so a full push+pull round trip (`push_drains_entries_...` and
// `registration_seeds_cursor_and_catch_up_advances_it_...` above)
// completing correctly already proves sync works without this process
// ever being handed — or needing — a `memory.db` path.
