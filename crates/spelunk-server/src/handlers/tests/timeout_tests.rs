use std::sync::Arc;

use serde_json::json;

use crate::handlers::{clear_generation_timeout_override, set_generation_timeout_override};

use super::support::{spawn_test_server, spawn_test_server_with_embed};

// A normal (non-exempt, non-streaming) route whose handler outlives the
// injected `TimeoutLayer` budget must be aborted with `408`. Control case
// proving the layer is enforced on the wire, not merely configured.
//
// Uses `add_note` (a synchronous handler awaiting the DB lock) rather than
// `/explore`/`/llm/complete`, which return their SSE `Response` immediately
// and so can't be bound by `TimeoutLayer`. Its DB mutex is held externally
// so `state.db.lock().await` blocks past the injected budget.
// ── TimeoutLayer / SSE exemption ──────────────────────────────────────
//
// These bind the real router (via `router_with_timeout`, injecting a short
// millisecond-scale budget) to a real TCP listener and drive it with a real
// HTTP client, so they prove actual wire behaviour: a connection genuinely
// held open past the timeout window, not just router wiring.

#[tokio::test]
async fn normal_route_exceeding_timeout_returns_408() {
    let request_timeout = std::time::Duration::from_millis(200);
    let (base, db) = spawn_test_server(None, request_timeout).await;

    // Hold the DB mutex for well past the timeout, from outside any
    // request: simulates a slow synchronous handler. `lock_owned`
    // yields a `'static` guard so it can be held across the spawned
    // task's await point.
    let guard = db.lock_owned().await;
    let hold_for = request_timeout * 5;
    let release_task = tokio::spawn(async move {
        tokio::time::sleep(hold_for).await;
        drop(guard);
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/projects/timeout-test/memory"))
        .json(&json!({
            "kind": "note",
            "title": "t",
            "body": "b",
            "embedding": [1.0, 0.0, 0.0, 0.0],
        }))
        .send()
        .await
        .expect("request should complete (with a timeout status), not hang forever");

    assert_eq!(
        resp.status().as_u16(),
        408,
        "a handler that outlives the TimeoutLayer budget must be aborted with 408"
    );

    release_task.await.expect("release task panicked");
}

// ── Generation-side timeout on `/explore` and `/llm/complete` ─────────────
//
// `normal_route_exceeding_timeout_returns_408` proves the router's
// `TimeoutLayer` can't bound these two endpoints. This is the other half:
// proving `llm_generate_with_timeout` actually cuts a hung backend off
// within budget: without it, deleting the `tokio::time::timeout(...)`
// wrapper would compile and pass every other test.

// An LLM backend whose `generate()` never returns and never sends a token:
// models a hung inference backend, the case `llm_generate_with_timeout`
// exists to bound.
struct HangingLlm {
    // Bumped once `generate()` is entered, so tests can assert generation
    // genuinely started before checking it gets cut off.
    entered: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl spelunk_core::llm::LlmBackend for HangingLlm {
    async fn generate(
        &self,
        _messages: &[spelunk_core::llm::Message],
        _max_tokens: usize,
        _tx: tokio::sync::mpsc::Sender<spelunk_core::llm::Token>,
        _json_schema: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        self.entered
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Never returns or drops `_tx` on its own: the only way it
        // completes is by being dropped from outside (the timeout firing).
        std::future::pending::<()>().await;
        unreachable!("pending() never resolves");
    }
}

// `/explore` backed by a `HangingLlm` must still have its connection cut
// off within the generation budget: proving `llm_generate_with_timeout`
// bounds a hung backend, not just that the code compiles.
//
// GOTCHA: without the `tokio::time::timeout` wrapper this test hangs until
// the CI timeout rather than failing fast: a worse failure mode, but
// accepted since the alternative doesn't exercise the wrapper.
#[tokio::test]
async fn explore_cuts_off_hanging_llm_backend() {
    // Millisecond-scale budget via the test-only override. The override is
    // process-wide, so guard with a lock: this test must not run
    // concurrently with anything else spawning `llm_generate_with_timeout`
    // under a different budget.
    static OVERRIDE_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = OVERRIDE_GUARD.lock().await;

    let generation_budget = std::time::Duration::from_millis(150);
    set_generation_timeout_override(generation_budget);
    // Router-level TimeoutLayer set generously long so it can't be what cuts
    // the connection off: isolates the generation-side wrapper.
    let router_timeout = generation_budget * 20;

    let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let llm: Arc<dyn spelunk_core::llm::LlmBackend> = Arc::new(HangingLlm {
        entered: entered.clone(),
    });
    let (base, _db) = spawn_test_server(Some(llm), router_timeout).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/projects/timeout-test/explore"))
        .json(&json!({"question": "q", "context_chunks": [], "max_turns": 1}))
        .send()
        .await
        .expect("SSE connection should open");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "/explore returns its SSE Response immediately regardless of backend \
             state: 200 here is expected and is exactly why the router-level \
             TimeoutLayer can't bound this endpoint (see normal_route_exceeding_timeout_returns_408)"
    );

    // Read the stream until it ends, bounded by a deadline past the
    // generation budget: if the wrapper weren't cutting the backend off,
    // the stream would still be pending when this deadline fires.
    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();
    let overall_deadline = generation_budget * 10;
    let outcome = tokio::time::timeout(overall_deadline, async {
        loop {
            match stream.next().await {
                Some(Ok(_)) => continue, // keep-alive / event; keep draining
                Some(Err(e)) => return Err(format!("stream errored: {e}")),
                None => return Ok(()), // channel closed -> stream ended cleanly
            }
        }
    })
    .await;

    clear_generation_timeout_override();

    assert_eq!(
        entered.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "HangingLlm::generate must have actually been entered: otherwise this \
             test would trivially pass without exercising the timeout wrapper at all"
    );
    match outcome {
        Ok(Ok(())) => {} // stream ended on its own, within the deadline: fixed behaviour
        Ok(Err(e)) => panic!(
            "SSE stream errored instead of ending cleanly once the hung backend's \
                 generation budget elapsed: {e}"
        ),
        Err(_elapsed) => panic!(
            "the SSE connection was still open {overall_deadline:?} after a HangingLlm \
                 backend started generating: llm_generate_with_timeout did not cut it off \
                 within its {generation_budget:?} budget. /explore's TimeoutLayer can't see \
                 spawned generation work, so a hung backend would otherwise hold the \
                 connection open indefinitely."
        ),
    }
}

// `/memory/stream` must survive well past the `TimeoutLayer` budget that
// kills every other route. This is the actual proof the exemption works:
// we hold a real SSE connection open, past the injected timeout window,
// polling for bytes the whole time, and confirm the server never closes
// or resets it (no error, no early EOF) and it is still readable after
// the deadline has elapsed.
#[tokio::test]
async fn memory_stream_survives_past_timeout_window() {
    // Deliberately short so the test doesn't take 30 real seconds: proves
    // the same property the 30s production constant relies on, just on a
    // compressed timescale. The stream handler polls the DB every 1s
    // internally and axum's default SSE keep-alive fires every 15s; ~1.2s
    // total wall-clock keeps this test fast while still running well past
    // a timeout window many multiples shorter.
    let request_timeout = std::time::Duration::from_millis(100);
    let (base, _db) = spawn_test_server(None, request_timeout).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{base}/v1/projects/timeout-test/memory/stream?t=0"))
        .send()
        .await
        .expect("SSE connection should open");
    assert_eq!(resp.status().as_u16(), 200, "stream must open with 200");

    // Read the stream for well past `request_timeout` (12x) and assert
    // every chunk read succeeds: if the TimeoutLayer applied here the
    // connection would be aborted (error / early close) once the budget
    // elapsed, well before this deadline.
    let hold_open_for = request_timeout * 12;
    let deadline = tokio::time::Instant::now() + hold_open_for;
    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            // A real chunk (keep-alive comment or data) arrived: still open.
            Ok(Some(Ok(_))) => continue,
            // Stream ended or errored before the deadline: the connection
            // was closed/reset early, which is exactly what we don't want.
            Ok(Some(Err(e))) => {
                panic!(
                    "SSE stream errored before the hold-open deadline (would indicate \
                         TimeoutLayer incorrectly applied to /memory/stream): {e}"
                );
            }
            Ok(None) => {
                panic!(
                    "SSE stream closed before the hold-open deadline (would indicate \
                         TimeoutLayer incorrectly applied to /memory/stream)"
                );
            }
            // No new chunk within the remaining window: fine, keep-alive
            // interval just hasn't fired again yet; loop will exit once
            // `remaining` hits zero.
            Err(_elapsed) => break,
        }
    }
    // If we got here without panicking, the connection survived the
    // entire hold-open window past the injected timeout.

    // Final check: the connection is still usable: issue one more read
    // with a fresh short timeout and confirm it doesn't immediately EOF.
    match tokio::time::timeout(std::time::Duration::from_millis(1500), stream.next()).await {
        Ok(Some(Ok(_))) => {} // got another keep-alive/data chunk: still alive
        Ok(Some(Err(e))) => panic!("stream errored on final liveness check: {e}"),
        Ok(None) => panic!("stream was closed by the server past the timeout window"),
        Err(_) => {
            // No new byte within 1.5s is acceptable (between keep-alive
            // ticks); what matters is it didn't error/close above.
        }
    }
}

// ── TimeoutLayer / `/index/embed` exemption ───────────────────────────────
//
// Same proof style as the `/memory/stream` exemption above: bind the real
// router with the general and embed timeouts injected independently
// (mirroring the `REQUEST_TIMEOUT` vs `EMBED_REQUEST_TIMEOUT` split) and
// drive it with a real HTTP client.

// An embedder backend that sleeps for a fixed duration before returning a
// zero vector per input: models a slow (e.g. CPU-only, cold-cache, or
// oversized-chunk) embed call on real hardware, the case
// `EMBED_REQUEST_TIMEOUT` exists to accommodate rather than kill.
struct SlowEmbedder {
    delay: std::time::Duration,
    dim: usize,
}

#[async_trait::async_trait]
impl spelunk_core::embeddings::EmbeddingBackend for SlowEmbedder {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        tokio::time::sleep(self.delay).await;
        Ok(texts.iter().map(|_| vec![0.0_f32; self.dim]).collect())
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

// `/index/embed` must survive well past the *general* `TimeoutLayer`
// budget that kills every other synchronous route (proved by
// `normal_route_exceeding_timeout_returns_408` above) as long as it stays
// under its own, separately-injected `embed_request_timeout`: this is
// the actual proof the exemption works, not just that the two constants
// exist. A slow embed call (bounded here, unbounded model inference in
// production) must complete successfully instead of being cut off at the
// general budget.
#[tokio::test]
async fn embed_survives_general_timeout_budget() {
    let general_timeout = std::time::Duration::from_millis(100);
    // Comfortably longer than `general_timeout` but still fast for a
    // test; the embed-specific timeout injected below is longer still.
    let embed_delay = general_timeout * 5;
    let embed_timeout = general_timeout * 20;

    let embedder = crate::EmbedderSlot::ready(Arc::new(SlowEmbedder {
        delay: embed_delay,
        dim: 4,
    }));
    let (base, _db) = spawn_test_server_with_embed(embedder, general_timeout, embed_timeout).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/projects/timeout-test/index/embed"))
        .json(&json!({
            "chunks": [{"chunk_id": "1", "content": "fn f() {}"}],
        }))
        .send()
        .await
        .expect("request should complete (not hang forever)");

    assert_eq!(
        resp.status().as_u16(),
        200,
        "/index/embed must survive a slow embed call that exceeds the general \
             TimeoutLayer budget but stays under its own EMBED_REQUEST_TIMEOUT: a 408 \
             here would mean the exemption isn't wired up (this is the exact field \
             failure this fix addresses: a real embed batch killed at 30s)"
    );
}

// Control case for the test above: with the embed-specific timeout
// injected *shorter* than the slow embed call, `/index/embed` must still
// 408: proving the embed sub-router's `TimeoutLayer` is actually live
// (not simply absent/unbounded), just configured with a different
// budget than the general routes.
#[tokio::test]
async fn embed_still_times_out_within_its_own_budget() {
    let general_timeout = std::time::Duration::from_secs(60); // effectively "not the bottleneck"
    let embed_timeout = std::time::Duration::from_millis(100);
    let embed_delay = embed_timeout * 5;

    let embedder = crate::EmbedderSlot::ready(Arc::new(SlowEmbedder {
        delay: embed_delay,
        dim: 4,
    }));
    let (base, _db) = spawn_test_server_with_embed(embedder, general_timeout, embed_timeout).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/projects/timeout-test/index/embed"))
        .json(&json!({
            "chunks": [{"chunk_id": "1", "content": "fn f() {}"}],
        }))
        .send()
        .await
        .expect("request should complete (with a timeout status), not hang forever");

    assert_eq!(
        resp.status().as_u16(),
        408,
        "/index/embed must still be bounded by its OWN budget: this proves the \
             embed sub-router's TimeoutLayer is live, not that /index/embed is now \
             unbounded"
    );
}

// A normal route (e.g. `/memory`, tested here via `add_note`) must still
// 408 at the *general* budget even when `/index/embed` has been given a
// much longer one: proving the split is a targeted carve-out for
// `/index/embed` specifically, not an accidental widening of the general
// timeout for every route.
#[tokio::test]
async fn other_routes_unaffected_by_longer_embed_budget() {
    let general_timeout = std::time::Duration::from_millis(100);
    let embed_timeout = std::time::Duration::from_secs(60); // deliberately much longer

    let embedder = crate::EmbedderSlot::disabled();
    let (base, db) = spawn_test_server_with_embed(embedder, general_timeout, embed_timeout).await;

    let guard = db.lock_owned().await;
    let hold_for = general_timeout * 5;
    let release_task = tokio::spawn(async move {
        tokio::time::sleep(hold_for).await;
        drop(guard);
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/projects/timeout-test/memory"))
        .json(&json!({
            "kind": "note",
            "title": "t",
            "body": "b",
            "embedding": [1.0, 0.0, 0.0, 0.0],
        }))
        .send()
        .await
        .expect("request should complete (with a timeout status), not hang forever");

    assert_eq!(
        resp.status().as_u16(),
        408,
        "a much longer /index/embed budget must not leak into the general route group"
    );

    release_task.await.expect("release task panicked");
}
