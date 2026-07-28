// ── Liveness while an embed is in flight ─────────────────────────────────
//
// A liveness probe must never be able to wait on the embedder's
// forward-pass mutex. That mutex is held for a whole batch and is taken
// synchronously from inside an `async fn`, so a probe that waits on it
// blocks a tokio worker instead of yielding it: enough concurrent probes
// park enough workers that unrelated endpoints stop being polled and the
// server reads as unreachable.
//
// `ParkingEmbedder` reproduces the backend's structure rather than its
// timing: `embed()` takes a forward-pass mutex inside `spawn_blocking` and
// parks there on a test-controlled gate, so "an embed is in flight" is a
// state these tests can assert against, with no sleeps and no wall-clock
// race. `cap_location` is the mock's one degree of freedom, and
// `harness_detects_a_cap_read_behind_the_forward_pass_mutex` uses it to
// prove these bounds actually catch the coupling they guard against.
//
// Know what this module does NOT do before you rely on it. Every test here
// runs against `ParkingEmbedder`, so re-coupling the real
// `NativeEmbedder::token_cap()` to its forward-pass mutex leaves all of
// them green: verified by mutation, all pass with the accessor put back
// behind the lock. That is structural and not fixable here, because these
// tests cannot construct a `NativeEmbedder` without a model on disk. What
// this module gates is the server-side property (health, and endpoints
// that need no embedder, stay prompt given a backend whose cap read is
// lock-free) plus the sensitivity of the bound itself. The regression
// guard for the accessor is `spelunk_embed::embedder_native`'s
// `token_cap_returns_while_the_forward_pass_lock_is_held` and
// `token_cap_is_independent_of_embedder_busyness`. If you change the cap's
// storage, those are the tests that must fail.
mod liveness_under_embed {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use axum::{
        body::Body,
        http::{self, Request},
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::super::support::{make_app_with_slot, post_note};

    // ~400x the sub-millisecond at-rest cost and ~19x below the seconds-long
    // stall a whole-batch wait produces, so it can neither flake on a slow
    // runner nor pass while the probe is coupled to the embed lock.
    const LIVENESS_BOUND: Duration = Duration::from_millis(250);
    const MOCK_DIM: usize = 4;
    const MOCK_TOKEN_CAP: usize = 5792;
    const PROJECT: &str = "liveness";

    // Counting rendezvous between the test and the embedder: `queued` rises
    // when a call enters the embedder and starts contending for the
    // forward-pass mutex, `entered` when it has the mutex and is parked.
    struct EmbedGate {
        queued: (Mutex<usize>, Condvar),
        entered: (Mutex<usize>, Condvar),
        released: (Mutex<bool>, Condvar),
    }

    impl EmbedGate {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                queued: (Mutex::new(0), Condvar::new()),
                entered: (Mutex::new(0), Condvar::new()),
                released: (Mutex::new(false), Condvar::new()),
            })
        }

        fn bump(counter: &(Mutex<usize>, Condvar)) {
            let (lock, cv) = counter;
            *lock.lock().expect("gate counter") += 1;
            cv.notify_all();
        }

        fn await_at_least(counter: &(Mutex<usize>, Condvar), target: usize, what: &str) {
            let (lock, cv) = counter;
            let mut count = lock.lock().expect("gate counter");
            while *count < target {
                let (next, timed_out) = cv
                    .wait_timeout(count, Duration::from_secs(10))
                    .expect("gate wait");
                assert!(
                    !timed_out.timed_out(),
                    "timed out waiting for {target} embed call(s) to be {what}"
                );
                count = next;
            }
        }

        fn wait_for_release(&self) {
            let (lock, cv) = &self.released;
            let mut done = lock.lock().expect("release flag");
            while !*done {
                let (next, timed_out) = cv
                    .wait_timeout(done, Duration::from_secs(30))
                    .expect("release wait");
                assert!(
                    !timed_out.timed_out(),
                    "a parked embed was never released by the test"
                );
                done = next;
            }
        }

        fn release(&self) {
            let (lock, cv) = &self.released;
            *lock.lock().expect("release flag") = true;
            cv.notify_all();
        }
    }

    #[derive(Clone, Copy)]
    enum CapLocation {
        // Where the backend keeps it: a plain field, read without touching
        // the forward-pass mutex.
        LockFreeField,
        // The regression: read through the same mutex a batch holds end to
        // end.
        BehindForwardPassLock,
    }

    struct ParkingEmbedder {
        dim: usize,
        token_cap: usize,
        cap_location: CapLocation,
        forward_pass: Arc<Mutex<()>>,
        gate: Arc<EmbedGate>,
    }

    #[async_trait::async_trait]
    impl spelunk_core::embeddings::EmbeddingBackend for ParkingEmbedder {
        async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            let count = texts.len();
            let dim = self.dim;
            let forward_pass = Arc::clone(&self.forward_pass);
            let gate = Arc::clone(&self.gate);
            tokio::task::spawn_blocking(move || {
                EmbedGate::bump(&gate.queued);
                let _guard = forward_pass.lock().expect("forward-pass mutex");
                EmbedGate::bump(&gate.entered);
                gate.wait_for_release();
                vec![vec![0.0_f32; dim]; count]
            })
            .await
            .map_err(|e| anyhow::anyhow!("parking embedder blocking task failed: {e}"))
        }

        fn dimension(&self) -> usize {
            self.dim
        }

        fn token_cap(&self) -> Option<usize> {
            if let CapLocation::BehindForwardPassLock = self.cap_location {
                let _guard = self.forward_pass.lock().ok()?;
            }
            if self.token_cap == 0 {
                None
            } else {
                Some(self.token_cap)
            }
        }
    }

    fn parked_app(cap_location: CapLocation) -> (axum::Router, Arc<EmbedGate>) {
        let gate = EmbedGate::new();
        let embedder = ParkingEmbedder {
            dim: MOCK_DIM,
            token_cap: MOCK_TOKEN_CAP,
            cap_location,
            forward_pass: Arc::new(Mutex::new(())),
            gate: Arc::clone(&gate),
        };
        let app = make_app_with_slot(MOCK_DIM, crate::EmbedderSlot::ready(Arc::new(embedder)));
        (app, gate)
    }

    fn spawn_embed(app: &axum::Router) -> tokio::task::JoinHandle<http::StatusCode> {
        let app = app.clone();
        tokio::spawn(async move {
            let body = json!({"chunks": [{"chunk_id": "c0", "content": "fn parked() {}"}]});
            let req = Request::builder()
                .method("POST")
                .uri(format!("/v1/projects/{PROJECT}/index/embed"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();
            app.oneshot(req).await.unwrap().status()
        })
    }

    // Block until `target` embed call(s) hold the forward-pass mutex, off
    // the async runtime so no worker is parked doing the waiting.
    async fn await_parked(gate: &Arc<EmbedGate>, target: usize) {
        let gate = Arc::clone(gate);
        tokio::task::spawn_blocking(move || {
            EmbedGate::await_at_least(&gate.entered, target, "parked in the embedder");
        })
        .await
        .expect("gate wait task");
    }

    // Block until `target` embed call(s) have entered the embedder, whether
    // or not they have won the forward-pass mutex yet.
    async fn await_queued(gate: &Arc<EmbedGate>, target: usize) {
        let gate = Arc::clone(gate);
        tokio::task::spawn_blocking(move || {
            EmbedGate::await_at_least(&gate.queued, target, "inside the embedder");
        })
        .await
        .expect("gate wait task");
    }

    async fn probe_health(app: &axum::Router) -> (http::StatusCode, Duration, Value) {
        let req = Request::builder()
            .method("GET")
            .uri("/v1/health")
            .body(Body::empty())
            .unwrap();
        let started = Instant::now();
        let resp = app.clone().oneshot(req).await.unwrap();
        let elapsed = started.elapsed();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).expect("health must return JSON");
        (status, elapsed, json)
    }

    // Every field a client reads off `/v1/health`, so a "fix" that drops or
    // nulls one to dodge the lock fails instead of passing.
    fn wire_contract(body: &Value) -> Value {
        json!({
            "status": body["status"],
            "version": body["version"],
            "capabilities": body["capabilities"],
            "instance_id": body["instance_id"],
            "embedding_dim": body["embedding_dim"],
            "embedder_state": body["embedder"]["state"],
            "embed_request_timeout_secs": body["limits"]["embed_request_timeout_secs"],
            "max_batch_chunks": body["limits"]["max_batch_chunks"],
            "embedder_token_cap": body["limits"]["embedder_token_cap"],
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn health_responds_promptly_while_an_embed_is_parked() {
        let (app, gate) = parked_app(CapLocation::LockFreeField);
        let embed = spawn_embed(&app);
        await_parked(&gate, 1).await;

        let (status, elapsed, _) = probe_health(&app).await;
        assert_eq!(
            status,
            http::StatusCode::OK,
            "health must answer while the embedder is busy"
        );
        assert!(
            elapsed < LIVENESS_BOUND,
            "health took {elapsed:?} with an embed parked in the embedder; \
             a liveness probe must not wait on the forward-pass mutex"
        );

        gate.release();
        assert_eq!(embed.await.expect("embed task"), http::StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_health_probes_all_respond_promptly_while_an_embed_is_parked() {
        let (app, gate) = parked_app(CapLocation::LockFreeField);
        let embed = spawn_embed(&app);
        await_parked(&gate, 1).await;

        // More probes than worker threads: if each one parked a worker on
        // the embed lock, the runtime would run out of threads to poll the
        // rest, which is the "server is unreachable" amplification.
        let probes: Vec<_> = (0..8)
            .map(|_| {
                let app = app.clone();
                tokio::spawn(async move { probe_health(&app).await })
            })
            .collect();

        for (i, probe) in probes.into_iter().enumerate() {
            let (status, elapsed, _) = probe.await.expect("probe task");
            assert_eq!(status, http::StatusCode::OK, "concurrent probe {i}");
            assert!(
                elapsed < LIVENESS_BOUND,
                "concurrent probe {i} took {elapsed:?}; concurrent liveness probes must \
                 not exhaust the runtime's workers"
            );
        }

        gate.release();
        assert_eq!(embed.await.expect("embed task"), http::StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn health_body_is_unchanged_while_an_embed_is_parked() {
        let (app, gate) = parked_app(CapLocation::LockFreeField);
        let (_, _, at_rest) = probe_health(&app).await;

        let embed = spawn_embed(&app);
        await_parked(&gate, 1).await;
        let (status, _, while_parked) = probe_health(&app).await;

        assert_eq!(status, http::StatusCode::OK);
        assert_eq!(
            wire_contract(&while_parked),
            wire_contract(&at_rest),
            "the health payload must be byte-identical whether or not the embedder is busy"
        );
        assert_eq!(
            while_parked["limits"]["embedder_token_cap"],
            json!(MOCK_TOKEN_CAP),
            "the advertised cap is a client contract: it must be reported in full while \
             an embed is in flight, not omitted or nulled"
        );

        gate.release();
        assert_eq!(embed.await.expect("embed task"), http::StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_endpoint_that_needs_no_embedder_responds_while_an_embed_is_parked() {
        let (app, gate) = parked_app(CapLocation::LockFreeField);
        let embed = spawn_embed(&app);
        await_parked(&gate, 1).await;

        let req = Request::builder()
            .method("GET")
            .uri("/v1/projects")
            .body(Body::empty())
            .unwrap();
        let started = Instant::now();
        let resp = app.clone().oneshot(req).await.unwrap();
        let elapsed = started.elapsed();

        assert_eq!(resp.status(), http::StatusCode::OK);
        assert!(
            elapsed < LIVENESS_BOUND,
            "GET /v1/projects took {elapsed:?} with an embed parked; an endpoint that \
             never touches the embedder must be unaffected by it"
        );

        gate.release();
        assert_eq!(embed.await.expect("embed task"), http::StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn memory_search_queued_behind_a_parked_embed_still_answers_well_formed() {
        let (app, gate) = parked_app(CapLocation::LockFreeField);
        let (created, _) = post_note(app.clone(), PROJECT, "seed", vec![1.0, 0.0, 0.0, 0.0]).await;
        assert_eq!(created, http::StatusCode::CREATED, "seed the project");

        let embed = spawn_embed(&app);
        await_parked(&gate, 1).await;

        let search_app = app.clone();
        let search = tokio::spawn(async move {
            let body = json!({"query": "seed", "limit": 5});
            let req = Request::builder()
                .method("POST")
                .uri(format!("/v1/projects/{PROJECT}/memory/search"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();
            let resp = search_app.oneshot(req).await.unwrap();
            let retry_after = resp
                .headers()
                .get(http::header::RETRY_AFTER)
                .map(|v| v.to_str().unwrap().to_string());
            (resp.status(), retry_after)
        });

        // `search_notes` embeds its query on the same serialized model, so
        // it must queue behind the in-flight batch. The bar is a
        // well-formed response, not an instant one: wait until it is
        // provably contending for the forward-pass mutex, then let both
        // through.
        await_queued(&gate, 2).await;
        gate.release();

        let (status, retry_after) = search.await.expect("search task");
        let well_formed = status == http::StatusCode::OK
            || (status == http::StatusCode::TOO_MANY_REQUESTS && retry_after.is_some());
        assert!(
            well_formed,
            "memory search during an index must return 200, or 429 with Retry-After; \
             got {status} (Retry-After: {retry_after:?})"
        );

        assert_eq!(embed.await.expect("embed task"), http::StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn health_returns_to_its_at_rest_behaviour_once_the_embed_completes() {
        let (app, gate) = parked_app(CapLocation::LockFreeField);
        let (_, _, at_rest) = probe_health(&app).await;

        let embed = spawn_embed(&app);
        await_parked(&gate, 1).await;
        gate.release();
        assert_eq!(embed.await.expect("embed task"), http::StatusCode::OK);

        let (status, elapsed, after) = probe_health(&app).await;
        assert_eq!(status, http::StatusCode::OK);
        assert!(
            elapsed < LIVENESS_BOUND,
            "health took {elapsed:?} after the embed completed: a leaked lock"
        );
        assert_eq!(
            wire_contract(&after),
            wire_contract(&at_rest),
            "the health payload must match the at-rest baseline once the embed is done"
        );

        // A completed embed must have returned its admission permit; if it
        // leaked, this second request is shed with 429 instead of served.
        assert_eq!(
            spawn_embed(&app).await.expect("embed task"),
            http::StatusCode::OK,
            "a further embed must still be admitted: the completed request's permit \
             must have been returned"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn health_stays_prompt_when_a_parked_embed_is_cancelled() {
        let (app, gate) = parked_app(CapLocation::LockFreeField);
        let embed = spawn_embed(&app);
        await_parked(&gate, 1).await;

        // Client disconnect / timeout: the handler future is dropped while
        // its blocking task is still parked inside the embedder.
        embed.abort();
        assert!(
            embed.await.unwrap_err().is_cancelled(),
            "the embed request was cancelled"
        );

        let (status, elapsed, _) = probe_health(&app).await;
        assert_eq!(status, http::StatusCode::OK);
        assert!(
            elapsed < LIVENESS_BOUND,
            "health took {elapsed:?} after the parked embed was cancelled"
        );

        gate.release();
        let (status, elapsed, _) = probe_health(&app).await;
        assert_eq!(status, http::StatusCode::OK);
        assert!(
            elapsed < LIVENESS_BOUND,
            "health took {elapsed:?} once the abandoned embed drained"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn harness_detects_a_cap_read_behind_the_forward_pass_mutex() {
        let (app, gate) = parked_app(CapLocation::BehindForwardPassLock);
        let embed = spawn_embed(&app);
        await_parked(&gate, 1).await;

        let probe_app = app.clone();
        let probe = tokio::spawn(async move { probe_health(&probe_app).await });
        let outcome = tokio::time::timeout(LIVENESS_BOUND, probe).await;
        assert!(
            outcome.is_err(),
            "a probe that reads the cap through the forward-pass mutex cannot answer \
             while a batch holds it: if this completes in time, the bound above no \
             longer proves anything"
        );

        gate.release();
        assert_eq!(embed.await.expect("embed task"), http::StatusCode::OK);
    }
}
