use std::sync::Arc;

use serde_json::json;

use super::support::{
    MockEmbedder, spawn_test_server_with_embed, spawn_test_server_with_embed_and_admission,
};

// ── Embed cancellation on client disconnect / server timeout ─────────────
// (GH#631)
//
// These bind the real router to a real TCP listener and drive it with a
// real HTTP client (same style as the TimeoutLayer tests above), so they
// prove actual wire behaviour: hyper genuinely drops the in-flight
// handler future on disconnect, and that drop must reach into the
// embedder's `embed_with_cancel`  -  modeled here via a fake backend since a
// real `NativeEmbedder` needs model weights this crate doesn't ship.

// An embedder that loops `iterations` times, checking `cancel` before each
// `step`-long sleep and bumping `progress` after it  -  models
// `NativeEmbedder::embed_with_cancel`'s sub-batch loop. Flags
// `observed_cancel` the moment it sees `cancel` set, so a test can assert
// cancellation was actually observed rather than the counter merely
// stopping for an unrelated reason.
//
// Runs the loop in a **detached `tokio::spawn`**, not directly in the
// returned future: this is the load-bearing detail that makes the fake
// reproduce the actual fault rather than paper over it. A plain async
// loop would already stop the instant the handler's future is dropped
// (ordinary Rust cancellation-on-drop  -  the behavior any embedder
// gets for free as long as it doesn't detach its work onto a separate
// task, so there'd be nothing here to test). Dropping a `JoinHandle`
// does **not** abort the task it points to  -  the same
// "detached" property `spawn_blocking` has in `NativeEmbedder`  -  so this
// loop only stops if it observes `cancel` itself, which is exactly what's
// under test.
struct CancelAwareEmbedder {
    iterations: usize,
    step: std::time::Duration,
    dim: usize,
    progress: Arc<std::sync::atomic::AtomicUsize>,
    observed_cancel: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl spelunk_core::embeddings::EmbeddingBackend for CancelAwareEmbedder {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.embed_with_cancel(texts, Arc::new(std::sync::atomic::AtomicBool::new(false)))
            .await
    }

    async fn embed_with_cancel(
        &self,
        texts: &[&str],
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        let iterations = self.iterations;
        let step = self.step;
        let n = texts.len();
        let dim = self.dim;
        let progress = Arc::clone(&self.progress);
        let observed_cancel = Arc::clone(&self.observed_cancel);

        let handle = tokio::spawn(async move {
            for _ in 0..iterations {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    observed_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    anyhow::bail!("embed cancelled");
                }
                tokio::time::sleep(step).await;
                progress.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(vec![vec![0.0_f32; dim]; n])
        });
        handle
            .await
            .map_err(|e| anyhow::anyhow!("embed task panicked: {e}"))?
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

// **T1 (load-bearing):** a client that disconnects mid-embed (here, via its
// own short request timeout) must stop the embedder's progress  -  not let
// it compute to completion for a result nobody reads. This is also the
// empirical proof that hyper drops the in-flight handler future on
// disconnect: on current main (no cancellation wiring), the fake's
// progress counter keeps advancing to 100 regardless of the client giving
// up, because `index_embed` calls a plain `embed()` with no way to signal
// abandonment into the detached work.
#[tokio::test]
async fn client_disconnect_stops_embedder_progress() {
    let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let embedder = crate::EmbedderSlot::ready(Arc::new(CancelAwareEmbedder {
        iterations: 100,
        step: std::time::Duration::from_millis(50),
        dim: 4,
        progress: Arc::clone(&progress),
        observed_cancel: Arc::clone(&observed_cancel),
    }));
    // Generous router-level timeouts: the client's own short timeout below
    // is what triggers the disconnect, not either TimeoutLayer.
    let (base, _db) = spawn_test_server_with_embed(
        embedder,
        std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(60),
    )
    .await;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(200))
        .build()
        .expect("building client with short timeout");
    let result = client
        .post(format!("{base}/v1/projects/timeout-test/index/embed"))
        .json(&json!({
            "chunks": [{"chunk_id": "1", "content": "fn f() {}"}],
        }))
        .send()
        .await;
    assert!(
        result.is_err(),
        "the client's own timeout must abort the connection  -  proves a real \
             disconnect happened, not that the server answered in time"
    );

    // Let the server notice the closed connection and let the fake's loop
    // observe the cancellation flag  -  it only checks between 50ms steps,
    // so a few steps' worth of settling avoids racing the exact instant
    // cancellation takes effect.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let settled = progress.load(std::sync::atomic::Ordering::Relaxed);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let after_wait = progress.load(std::sync::atomic::Ordering::Relaxed);

    assert_eq!(
        settled, after_wait,
        "the embedder must stop making progress once the client disconnects  -  \
             this counter running on to completion (100) is exactly the measured \
             fault (GH#631): a batch computed in full for a \
             result nobody reads"
    );
    assert!(
        observed_cancel.load(std::sync::atomic::Ordering::Relaxed),
        "the embedder must have observed the cancellation flag itself, not just \
             stopped for some unrelated reason"
    );
}

// An embedder that serializes on an internal async mutex (mirroring
// `NativeEmbedder`'s `Arc<Mutex<EmbedderInner>>`) and checks `cancel`
// immediately after acquiring it, before doing any work  -  the "cascade
// killer" check. `iterations_done` is shared across every call through
// this embedder, so if a queued call is cancelled before it starts, it
// contributes nothing to the total.
//
// As with `CancelAwareEmbedder`, the lock-and-loop runs in a **detached
// `tokio::spawn`** so dropping the caller's future (client disconnect)
// doesn't auto-cancel it via ordinary Rust drop semantics  -  only the
// explicit `cancel` check does, matching `NativeEmbedder`'s
// `spawn_blocking`.
struct QueuedCancelEmbedder {
    lock: Arc<tokio::sync::Mutex<()>>,
    iterations: usize,
    step: std::time::Duration,
    dim: usize,
    iterations_done: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl spelunk_core::embeddings::EmbeddingBackend for QueuedCancelEmbedder {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.embed_with_cancel(texts, Arc::new(std::sync::atomic::AtomicBool::new(false)))
            .await
    }

    async fn embed_with_cancel(
        &self,
        texts: &[&str],
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        let lock = Arc::clone(&self.lock);
        let iterations = self.iterations;
        let step = self.step;
        let n = texts.len();
        let dim = self.dim;
        let iterations_done = Arc::clone(&self.iterations_done);

        let handle = tokio::spawn(async move {
            let _guard = lock.lock().await;
            anyhow::ensure!(
                !cancel.load(std::sync::atomic::Ordering::Relaxed),
                "cancelled while queued behind another batch  -  zero forward passes done"
            );
            for _ in 0..iterations {
                anyhow::ensure!(
                    !cancel.load(std::sync::atomic::Ordering::Relaxed),
                    "cancelled mid-batch"
                );
                tokio::time::sleep(step).await;
                iterations_done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(vec![vec![0.0_f32; dim]; n])
        });
        handle
            .await
            .map_err(|e| anyhow::anyhow!("embed task panicked: {e}"))?
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

// **T2 (queue ghost):** two overlapping requests share the same
// mutex-serialized embedder. The first holds the lock and runs to
// completion; the second is abandoned (client-side timeout) while still
// queued waiting for the lock. Once the lock is handed to it, it must do
// zero forward passes  -  proving the "check immediately after acquiring
// the lock" seam kills a ghost before it does any work, which is what
// stops a live retry from queuing behind a ghost batch (the compounding
// cascade this guards against).
#[tokio::test]
async fn queued_request_abandoned_while_waiting_does_zero_forward_passes() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let iterations_done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    const FIRST_ITERATIONS: usize = 5;
    const STEP: std::time::Duration = std::time::Duration::from_millis(60);

    let embedder = crate::EmbedderSlot::ready(Arc::new(QueuedCancelEmbedder {
        lock: Arc::clone(&lock),
        iterations: FIRST_ITERATIONS,
        step: STEP,
        dim: 4,
        iterations_done: Arc::clone(&iterations_done),
    }));
    let (base, _db) = spawn_test_server_with_embed(
        embedder,
        std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(60),
    )
    .await;

    // First request: normal client, no timeout  -  must complete, holding
    // the embedder's internal lock for FIRST_ITERATIONS * STEP.
    let base_a = base.clone();
    let first = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{base_a}/v1/projects/timeout-test/index/embed"))
            .json(&json!({"chunks": [{"chunk_id": "1", "content": "fn a() {}"}]}))
            .send()
            .await
    });

    // Give the first request time to actually acquire the lock and start
    // iterating before the second is sent.
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    // Second request: short client timeout that fires while it is still
    // queued waiting for the lock (well before the first releases it).
    let second_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(60))
        .build()
        .expect("building client with short timeout");
    let second_result = second_client
        .post(format!("{base}/v1/projects/timeout-test/index/embed"))
        .json(&json!({"chunks": [{"chunk_id": "2", "content": "fn b() {}"}]}))
        .send()
        .await;
    assert!(
        second_result.is_err(),
        "the second request's own short timeout must abort its connection while \
             still queued behind the first"
    );

    let first_result = first
        .await
        .expect("first request task panicked")
        .expect("first request should complete normally (not abandoned)");
    assert_eq!(
        first_result.status().as_u16(),
        200,
        "the first (non-abandoned) request must complete successfully"
    );

    // Let the second call's queued `embed_with_cancel` actually get the
    // lock (freed when the first completed) and observe cancellation.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(
        iterations_done.load(std::sync::atomic::Ordering::Relaxed),
        FIRST_ITERATIONS,
        "the second (abandoned-while-queued) request must contribute zero \
             forward passes  -  on current main it would run its own \
             {FIRST_ITERATIONS} iterations once granted the lock, doubling wasted \
             work instead of being killed by the cascade-killer check"
    );
}

// **T3 (server 408):** a server-side timeout (the embed sub-router's own
// `TimeoutLayer`, mirroring `EMBED_REQUEST_TIMEOUT`) must cancel the
// in-flight batch the same way a client disconnect does  -  one fix covers
// both, since both drop the handler future the same way.
#[tokio::test]
async fn server_side_embed_timeout_cancels_in_flight_batch() {
    let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let general_timeout = std::time::Duration::from_secs(60);
    let embed_timeout = std::time::Duration::from_millis(100);

    let embedder = crate::EmbedderSlot::ready(Arc::new(CancelAwareEmbedder {
        iterations: 100,
        step: std::time::Duration::from_millis(50),
        dim: 4,
        progress: Arc::clone(&progress),
        observed_cancel: Arc::clone(&observed_cancel),
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
        "the embed sub-router's own TimeoutLayer must still fire a 408 (same as \
             embed_still_times_out_within_its_own_budget above)"
    );

    // Let the cancellation actually propagate before taking the baseline
    // sample  -  same settling rationale as `client_disconnect_stops_embedder_progress`.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let settled = progress.load(std::sync::atomic::Ordering::Relaxed);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let after_wait = progress.load(std::sync::atomic::Ordering::Relaxed);

    assert_eq!(
        settled, after_wait,
        "a server-side 408 must cancel the in-flight native batch the same way a \
             client disconnect does  -  on current main the ghost batch keeps computing \
             after the 408 response is already sent"
    );
    assert!(
        observed_cancel.load(std::sync::atomic::Ordering::Relaxed),
        "the embedder must have observed the cancellation flag after the 408"
    );
}

// Edge case: cancellation observed on exactly the **last** iteration of a
// batch  -  the boundary the sub-batch/per-chunk checks are meant to catch
// early elsewhere, but here there is no "next" chunk left to abandon into.
// Deterministic (no HTTP, no timing race): a watcher task flips `cancel`
// as soon as `progress` reaches `ITERATIONS - 2`, i.e. once every chunk
// but the last *two* has completed. That leaves a full iteration's sleep
// (`step`) as slack for the watcher to actually act before the check that
// matters: the loop's own check-then-sleep-then-increment body has no
// `.await` between one iteration's increment and the next iteration's
// check, so a watcher targeting `ITERATIONS - 1` directly can never win
// that race under a single-threaded runtime  -  it would only ever be
// woken up (and act) *after* the following check had already run.
// Targeting one iteration earlier gives the watcher the preceding
// iteration's whole `step` duration to act, so the final iteration is the
// one deterministically guaranteed to observe cancellation. Proves the
// loop bails out cleanly (an `Err`, no panic, no double-counted progress)
// rather than e.g. running one past the check or leaving the
// `JoinHandle` unresolved.
#[tokio::test]
async fn cancellation_on_last_chunk_completes_cleanly_no_panic() {
    use spelunk_core::embeddings::EmbeddingBackend;

    const ITERATIONS: usize = 5;
    let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let embedder = CancelAwareEmbedder {
        iterations: ITERATIONS,
        step: std::time::Duration::from_millis(20),
        dim: 4,
        progress: Arc::clone(&progress),
        observed_cancel: Arc::clone(&observed_cancel),
    };

    let watch_progress = Arc::clone(&progress);
    let watch_cancel = Arc::clone(&cancel);
    let watcher = tokio::spawn(async move {
        loop {
            if watch_progress.load(std::sync::atomic::Ordering::Relaxed) >= ITERATIONS - 2 {
                watch_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    });

    let result = embedder
        .embed_with_cancel(&["fn f() {}"], Arc::clone(&cancel))
        .await;
    watcher.await.expect("watcher task panicked");

    assert!(
        result.is_err(),
        "cancellation observed on the final chunk must still bail out cleanly \
             with an error, not silently return a (now-meaningless) success"
    );
    assert_eq!(
        progress.load(std::sync::atomic::Ordering::Relaxed),
        ITERATIONS - 1,
        "the final iteration must be the one that observes cancellation and \
             never runs  -  no off-by-one either completing one extra iteration or \
             stopping one short"
    );
    assert!(
        observed_cancel.load(std::sync::atomic::Ordering::Relaxed),
        "the embedder must have observed the cancellation flag itself on the \
             final iteration"
    );
}

// Edge case explicitly called out alongside T2: a solo request  -  no
// other batch ever holds the embedder, so there is no queue delay for
// the client's disconnect to race against  -  that is abandoned as early
// as physically possible. This is deliberately **not** asserting zero
// forward passes: `queued_request_abandoned_while_waiting_does_zero_forward_passes`
// (T2, above) proves zero waste specifically for a ghost that loses a
// race for the mutex to a live occupier, because the wait for the lock
// gives the disconnect time to land before the ghost's own check runs.
// A solo request has no such delay to exploit: the mutex-acquire check
// fires essentially instantly, almost certainly before the disconnect
// (which has to round-trip a real TCP close) can possibly have
// propagated, so it inevitably starts its first chunk. What's
// guaranteed here is acceptance criterion #1  -  bounded to at most one
// wasted chunk, then stopped for good  -  not criterion #2's "zero,"
// which is scoped to the queued-behind-another-batch case. This test
// pins that distinction down so it isn't mistaken for a regression
// later.
#[tokio::test]
async fn solo_request_disconnected_stops_within_one_chunk_no_contention() {
    let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let embedder = crate::EmbedderSlot::ready(Arc::new(CancelAwareEmbedder {
        iterations: 100,
        // Deliberately long relative to the client's timeout below, so the
        // first check-before-sleep is essentially certain to run before the
        // client would ever have given the loop a chance to advance.
        step: std::time::Duration::from_millis(200),
        dim: 4,
        progress: Arc::clone(&progress),
        observed_cancel: Arc::clone(&observed_cancel),
    }));
    let (base, _db) = spawn_test_server_with_embed(
        embedder,
        std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(60),
    )
    .await;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(10))
        .build()
        .expect("building client with short timeout");
    let result = client
        .post(format!("{base}/v1/projects/timeout-test/index/embed"))
        .json(&json!({
            "chunks": [{"chunk_id": "1", "content": "fn f() {}"}],
        }))
        .send()
        .await;
    assert!(
        result.is_err(),
        "the client's own very short timeout must abort the connection long \
             before the (much longer) embed loop's first sleep completes"
    );

    // Settle past the first step so the in-flight (already-started) chunk
    // finishes, then confirm progress goes no further  -  same
    // settling rationale as `client_disconnect_stops_embedder_progress`.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let settled = progress.load(std::sync::atomic::Ordering::Relaxed);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let after_wait = progress.load(std::sync::atomic::Ordering::Relaxed);

    assert_eq!(
        settled, after_wait,
        "progress must stop for good once cancellation is observed, not merely \
             pause"
    );
    assert!(
        settled <= 1,
        "a solo (uncontended) request must be bounded to at most one wasted \
             chunk's forward pass (acceptance criterion #1)  -  got {settled}"
    );
    assert!(
        observed_cancel.load(std::sync::atomic::Ordering::Relaxed),
        "the embedder must have observed the cancellation flag itself"
    );
}

// The abandon guard must be a no-op when dropped already-disarmed (the
// ordinary "request completed" path, success or a real embed error
// alike): and must be safe to fire on a flag that was *already* true,
// without panicking or otherwise corrupting state. Two independent
// guards sharing one flag is the closest reachable proxy in safe Rust for
// "the guard fires twice": Rust's ownership model makes a literal double
// `Drop::drop` call on one guard instance unreachable, but nothing stops
// two guards (e.g. from two abandonment sources racing) from firing on
// the same shared `Arc<AtomicBool>`.
#[test]
fn embed_abandon_guard_drop_is_idempotent_when_flag_already_set() {
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let disarmed = crate::handlers::index::EmbedAbandonGuard {
        cancel: Arc::clone(&cancel),
        armed: false,
        project_id: "p".to_string(),
        batch_size: 1,
        started: std::time::Instant::now(),
    };
    drop(disarmed);
    assert!(
        !cancel.load(std::sync::atomic::Ordering::Relaxed),
        "a disarmed guard (the normal completed-request path) must never touch \
             the flag"
    );

    let first = crate::handlers::index::EmbedAbandonGuard {
        cancel: Arc::clone(&cancel),
        armed: true,
        project_id: "p".to_string(),
        batch_size: 1,
        started: std::time::Instant::now(),
    };
    drop(first);
    assert!(
        cancel.load(std::sync::atomic::Ordering::Relaxed),
        "an armed guard must set the flag on drop"
    );

    // A second, independent armed guard firing on an already-cancelled flag
    // must not panic and must leave the flag exactly as-is (true).
    let second = crate::handlers::index::EmbedAbandonGuard {
        cancel: Arc::clone(&cancel),
        armed: true,
        project_id: "p".to_string(),
        batch_size: 1,
        started: std::time::Instant::now(),
    };
    drop(second);
    assert!(
        cancel.load(std::sync::atomic::Ordering::Relaxed),
        "a second armed guard firing on an already-set flag must be idempotent, \
             not panic or clear it"
    );
}

// ── ConcurrencyLimitLayer under concurrent load ───────────────────────────

// Proves `tower::limit::ConcurrencyLimitLayer` backpressures concurrent
// requests beyond its cap under real concurrent load, not just that the
// layer is attached.
//
// Deliberately does NOT route through `/explore` or `/llm/complete`: those
// release the concurrency permit as soon as the SSE stream is constructed
// (generation is a detached `tokio::spawn`), so they sit outside what
// `ConcurrencyLimitLayer` can bound: the same gap `llm_generate_with_timeout`
// closes for `TimeoutLayer`.
#[tokio::test]
async fn concurrency_limit_layer_queues_requests_beyond_the_cap() {
    use axum::{Router, routing::get};

    // A trivial handler that blocks until released, wrapped in the same
    // `ConcurrencyLimitLayer` type used by `router`.
    //
    // Uses a `watch` channel (not `Notify`) as the gate: `Notify` only wakes
    // tasks already waiting when fired, so a handler admitted after release
    // would hang; `watch` retains the value for late subscribers.
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    const CONCURRENCY_CAP: usize = 2;
    let started_for_handler = started.clone();
    let app: Router = Router::new()
        .route(
            "/gated",
            get(move || {
                let mut gate_rx = gate_rx.clone();
                let started = started_for_handler.clone();
                async move {
                    started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = gate_rx.wait_for(|released| *released).await;
                    "ok"
                }
            }),
        )
        .layer(tower::limit::ConcurrencyLimitLayer::new(CONCURRENCY_CAP));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .expect("test server crashed");
    });
    let base = format!("http://{addr}");

    // Fire 5 concurrent requests against a concurrency cap of 2. Every
    // handler blocks on `gate` until released, so if the limiter is
    // actually enforcing backpressure, at most CONCURRENCY_CAP of them
    // can be inside the handler (i.e. have incremented `started`) at any
    // one time: the rest must be queued by `tower::limit` waiting for a
    // slot, not admitted straight through.
    const N_REQUESTS: usize = 5;
    let client = reqwest::Client::new();
    let mut handles = Vec::new();
    for _ in 0..N_REQUESTS {
        let client = client.clone();
        let base = base.clone();
        handles.push(tokio::spawn(async move {
            client.get(format!("{base}/gated")).send().await
        }));
    }

    // Give the server plenty of time to admit as many as it will admit
    // while everyone is still gated (blocked mid-handler).
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let admitted_while_gated = started.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        admitted_while_gated, CONCURRENCY_CAP,
        "with a concurrency cap of {CONCURRENCY_CAP} and {N_REQUESTS} concurrent gated \
             requests, exactly {CONCURRENCY_CAP} should be admitted into the handler while \
             the rest queue outside it: got {admitted_while_gated} admitted, which means \
             ConcurrencyLimitLayer is not actually backpressuring concurrent load"
    );

    // Release everyone; the queued requests should now proceed too
    // (including ones admitted after this point, since `watch` retains
    // the value for late subscribers), and all 5 should eventually
    // complete (nothing stuck forever).
    gate_tx.send(true).expect("gate receiver dropped");
    for h in handles {
        let resp = h.await.expect("task panicked").expect("request failed");
        assert_eq!(resp.status().as_u16(), 200);
    }
    assert_eq!(
        started.load(std::sync::atomic::Ordering::SeqCst),
        N_REQUESTS,
        "all requests should eventually be admitted once slots free up"
    );
}

// ── Embed admission control (429 on queue saturation) ────────────────────
//
// The embedder itself is mutex-serialized (one call at a time) by design
// (GPU memory / CPU thread-budget reasons in `spelunk-embed`); these tests
// cover the layer in FRONT of it: `EmbedAdmission`: which bounds how
// many callers may hold a slot waiting their turn before the server sheds
// load with 429 instead of letting a request queue silently past its own
// timeout.

// An embedder that signals `started` the instant it is invoked, then
// blocks until `release` fires: lets a test hold the only admission
// slot open for as long as it needs to prove the *next* request is shed
// immediately rather than queued.
struct GatedEmbedder {
    dim: usize,
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl spelunk_core::embeddings::EmbeddingBackend for GatedEmbedder {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(vec![vec![0.0_f32; self.dim]; texts.len()])
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

// **T1:** with the admission queue's only slot held by an in-flight
// request, a second `/index/embed` call must be shed immediately with
// `429` + the configured `Retry-After`: not queue behind the first and
// wait. Once the first request is released, it must still complete
// normally: admission control sheds excess load, it does not break the
// request that WAS within budget.
#[tokio::test]
async fn index_embed_returns_429_with_retry_after_once_admission_queue_is_saturated() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let embedder = crate::EmbedderSlot::ready(Arc::new(GatedEmbedder {
        dim: 4,
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    }));
    let (base, _db) = spawn_test_server_with_embed_and_admission(
        embedder,
        std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(60),
        crate::EmbedAdmission::new(1, 3),
    )
    .await;

    let first_base = base.clone();
    let first = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{first_base}/v1/projects/timeout-test/index/embed"))
            .json(&json!({"chunks": [{"chunk_id": "1", "content": "fn f() {}"}]}))
            .send()
            .await
    });

    // Wait for the first request to actually be holding the (only) slot
    // before firing the second, so this proves saturation shedding, not
    // an accidental race.
    started.notified().await;

    let second = reqwest::Client::new()
        .post(format!("{base}/v1/projects/timeout-test/index/embed"))
        .json(&json!({"chunks": [{"chunk_id": "2", "content": "fn g() {}"}]}))
        .send()
        .await
        .expect("a saturated queue must respond immediately (429), not hang");

    assert_eq!(
        second.status().as_u16(),
        429,
        "the second request must be shed once the single admission slot is held"
    );
    assert_eq!(
        second
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .expect("429 must carry Retry-After")
            .to_str()
            .unwrap(),
        "3",
        "Retry-After must carry this admission gate's configured value"
    );

    release.notify_one();
    let first_resp = first
        .await
        .expect("first request task panicked")
        .expect("first request must still complete once its slot is released");
    assert_eq!(
        first_resp.status().as_u16(),
        200,
        "the admitted request must succeed normally: shedding excess load must not \
             break the request that was within budget"
    );
}

// Control case: sequential requests within the configured capacity never
// see a 429: the previous test's rejection is specifically about
// exceeding the bound, not embed requests in general.
#[tokio::test]
async fn index_embed_succeeds_normally_when_within_admission_capacity() {
    let embedder = crate::EmbedderSlot::ready(Arc::new(MockEmbedder { dim: 4 }));
    let (base, _db) = spawn_test_server_with_embed_and_admission(
        embedder,
        std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(60),
        crate::EmbedAdmission::new(2, 3),
    )
    .await;

    let client = reqwest::Client::new();
    for chunk_id in ["1", "2", "3"] {
        let resp = client
            .post(format!("{base}/v1/projects/timeout-test/index/embed"))
            .json(&json!({"chunks": [{"chunk_id": chunk_id, "content": "fn f() {}"}]}))
            .send()
            .await
            .expect("request within admission capacity must succeed");
        assert_eq!(
            resp.status().as_u16(),
            200,
            "a request within the admission bound must never be shed with 429"
        );
    }
}

// ── add_note under a saturated embedder (scope check) ────────────────────
//
// `add_note`/`push_memory_batch` are deliberately NOT gated by
// `EmbedAdmission` (see the task's scope note: they "already catch any
// embed error and degrade to text-only storage"). That's true for an
// in-band `Err` from `embed()`, but a saturated embedder doesn't error
// quickly, it just makes the caller wait behind the `Mutex`. This proves
// what actually happens when the embedder is busy longer than the
// general request timeout, under the exact load pattern the embed
// admission control targets for `index_embed`/`search`/`search_notes`.
#[tokio::test]
async fn add_note_under_saturated_embedder_is_cancelled_not_degraded() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let embedder = crate::EmbedderSlot::ready(Arc::new(GatedEmbedder {
        dim: 4,
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    }));
    // A short general request_timeout stands in for "the embedder is
    // busy embedding a large index batch for longer than 30s in
    // production" (`REQUEST_TIMEOUT`).
    let (base, _db) = spawn_test_server_with_embed(
        embedder,
        std::time::Duration::from_millis(200),
        std::time::Duration::from_secs(60),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/projects/timeout-test/memory"))
        .json(&json!({
            "kind": "note",
            "title": "saturation probe",
            "body": "does add_note degrade or get cancelled while the embedder is busy?",
        }))
        .send()
        .await
        .expect("the request itself must complete (timeout layer responds, not a hang)");

    // `add_note`'s own `match embedder.embed(...).await { Err(e) => ... }`
    // arm never runs here: the enclosing `TimeoutLayer` races the whole
    // handler future and cancels it first, so the note is dropped
    // entirely (stored neither with nor without a vector) rather than
    // degrading to text-only. Pre-existing behavior, unchanged by this
    // fix (add_note was never gated before either) - not a regression -
    // but it means the "already degrades gracefully" scope note overstates
    // this specific failure mode; text-only degradation only covers an
    // in-band embed error, not a saturated/slow embedder.
    assert_eq!(
        resp.status().as_u16(),
        408,
        "add_note under a saturated embedder is cancelled by the general request \
             timeout, not degraded to text-only storage - if this ever starts returning \
             201, the scope note's claim has become literally true and should be revisited"
    );

    // The handler future (and the `embed()` call inside it) was already
    // cancelled by the timeout layer above; nothing is waiting on
    // `release` any more. Fire it anyway so a future refactor that makes
    // this not-cancelled can't turn this test into a silent hang.
    release.notify_one();
}
