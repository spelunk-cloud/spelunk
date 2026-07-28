// Real-hardware confirmation that the local server stays usable while it is
// embedding, run against the actual native embedder rather than a mock.
//
// **Not a CI gate.** It is `#[ignore]`d: it needs the F2LLM model artifacts on
// disk (downloading them on first run), real GPU/CPU inference, and it
// measures wall-clock latency, none of which belong on a shared runner. The
// deterministic gate for the same property is
// `handlers::tests::liveness_under_embed` (mock embedder parked on a
// test-controlled signal, no model, no timing race), plus
// `spelunk_embed::embedder_native`'s accessor tests.
//
// Run it with:
//   SPELUNK_SECRET_STORE=file cargo test -p spelunk-server \
//     --test health_under_index_load -- --ignored --nocapture
//
// Tunable via env:
//   SPELUNK_HEALTH_LOAD_REPO    repo to draw real text from (default: this workspace)
//   SPELUNK_HEALTH_LOAD_CHUNKS  how many chunks to embed (default: 2048)
//
// This drives the HTTP surface directly rather than shelling out to the CLI.
// `/v1/health` is the sole endpoint `spelunk server status` reads (it renders
// "reachable" from `instance_id` + `version`), and `POST
// /v1/projects/{id}/memory/search` is the request `spelunk memory search`
// issues, so the assertions below are the same contract those commands see.

#![cfg(feature = "embed-native")]

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

// Every liveness sample must come back inside this. Measured at rest is
// sub-millisecond; a probe that waits on the embedder's forward-pass mutex
// takes seconds.
const LIVENESS_BOUND: Duration = Duration::from_millis(250);
// Gap between liveness samples for the whole embed phase.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
// Chunks per `/index/embed` request, matching what the index phase sends.
const BATCH_CHUNKS: usize = 256;
// Mirrors the server's own non-embed request timeout, which is crate-private.
// Client deadlines are set relative to it so the server always gets to answer
// first: a client-side cutoff would be indistinguishable here from the dropped
// connection this test exists to rule out.
const SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DIM: usize = spelunk_core::embeddings::EMBEDDING_DIM;
const PROJECT: &str = "health-under-load";

fn repo_root() -> PathBuf {
    if let Ok(dir) = std::env::var("SPELUNK_HEALTH_LOAD_REPO") {
        return PathBuf::from(dir);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn chunk_budget() -> usize {
    std::env::var("SPELUNK_HEALTH_LOAD_CHUNKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2048)
}

// Collect real source text from `root`, windowed into chunk-sized pieces.
fn collect_chunks(root: &Path, budget: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if chunks.len() >= budget {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let is_text = matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("rs" | "md" | "toml" | "ts" | "py" | "go")
            );
            if !is_text {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let words: Vec<&str> = content.split_whitespace().collect();
            for window in words.chunks(200) {
                if chunks.len() >= budget {
                    return chunks;
                }
                let text = window.join(" ");
                if text.len() > 32 {
                    chunks.push(text);
                }
            }
        }
    }
    chunks
}

async fn spawn_server(embedder: spelunk_server::EmbedderSlot) -> String {
    common::register_sqlite_vec();
    let db = spelunk_server::db::ServerDb::open(Path::new(":memory:"), DIM, "test-model")
        .expect("open in-memory server db");
    let instance_id = db.get_or_create_instance_id().expect("instance_id");
    let state = spelunk_server::AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        auth: Arc::new(spelunk_server::auth::ApiKeyAuth::new(None)),
        conflict_threshold: spelunk_server::default_conflict_threshold(),
        embedder,
        embed_admission: spelunk_server::EmbedAdmission::new(
            spelunk_server::EMBED_QUEUE_CAPACITY,
            spelunk_server::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        llm: None,
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(spelunk_server::rate_limiter::RateLimiter::new(100_000, 60)),
        instance_id,
        started_by: None,
        relay: spelunk_server::relay::RelayRegistry::new(),
    };
    let app = spelunk_server::router(state);
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

// A single liveness sample: the latency, and the two fields
// `spelunk server status` needs to print a server as reachable.
struct HealthSample {
    latency: Duration,
    instance_id: String,
    version: String,
    token_cap: Value,
}

async fn sample_health(client: &reqwest::Client, base: &str) -> HealthSample {
    let started = Instant::now();
    let resp = client
        .get(format!("{base}/v1/health"))
        .send()
        .await
        .expect("health must always answer: a failed request is the 'unreachable' symptom");
    let latency = started.elapsed();
    assert_eq!(resp.status().as_u16(), 200, "health must return 200");
    let body: Value = resp.json().await.expect("health must return JSON");
    HealthSample {
        latency,
        instance_id: body["instance_id"].as_str().unwrap_or_default().to_string(),
        version: body["version"].as_str().unwrap_or_default().to_string(),
        token_cap: body["limits"]["embedder_token_cap"].clone(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the F2LLM model and real inference hardware; not a CI gate"]
async fn health_and_memory_search_stay_usable_throughout_a_real_index() {
    let embedder = spelunk_server::embed_hub::load_from_hub().expect("load F2LLM-v2-330M");
    let base = spawn_server(spelunk_server::EmbedderSlot::ready(Arc::new(embedder))).await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("http client");

    // Seed the project with an explicit vector so this setup step does not
    // itself queue on the embedder.
    let seed = client
        .post(format!("{base}/v1/projects/{PROJECT}/memory"))
        .json(&json!({
            "kind": "note",
            "title": "seed",
            "body": "seed entry for the concurrent memory search probe",
            "embedding": vec![0.0_f32; DIM],
        }))
        .send()
        .await
        .expect("seed note");
    assert_eq!(seed.status().as_u16(), 201, "seed note must be created");

    let chunks = collect_chunks(&repo_root(), chunk_budget());
    assert!(
        chunks.len() >= BATCH_CHUNKS,
        "need at least one full batch of real text to embed, got {}",
        chunks.len()
    );
    println!("embedding {} chunks from {:?}", chunks.len(), repo_root());

    // The index phase: sequential full-size batches, exactly as `spelunk index`
    // issues them.
    let index_client = client.clone();
    let index_base = base.clone();
    let index = tokio::spawn(async move {
        for batch in chunks.chunks(BATCH_CHUNKS) {
            let body = json!({
                "chunks": batch
                    .iter()
                    .enumerate()
                    .map(|(i, c)| json!({"chunk_id": format!("c{i}"), "content": c}))
                    .collect::<Vec<_>>()
            });
            let resp = index_client
                .post(format!("{index_base}/v1/projects/{PROJECT}/index/embed"))
                .timeout(Duration::from_secs(1800))
                .json(&body)
                .send()
                .await
                .expect("embed batch request");
            assert_eq!(resp.status().as_u16(), 200, "embed batch must succeed");
        }
    });

    // Concurrent `memory search`, honouring Retry-After on a shed 429. It
    // embeds its query on the same serialized model, so it queues rather than
    // returning instantly; the bar is that it always gets a well-formed
    // response and eventually succeeds.
    //
    // Its own client, with a timeout deliberately above the server's own
    // request timeout: a search that queues behind an in-flight batch can
    // legitimately outlast the probe client's short deadline, and cutting it
    // off here would report a client-side deadline as the dropped connection
    // this test exists to rule out.
    let search_client = reqwest::Client::builder()
        .timeout(SERVER_REQUEST_TIMEOUT + Duration::from_secs(15))
        .build()
        .expect("search http client");
    let search_base = base.clone();
    let search = tokio::spawn(async move {
        let mut succeeded = false;
        for _ in 0..20 {
            let resp = search_client
                .post(format!("{search_base}/v1/projects/{PROJECT}/memory/search"))
                .json(&json!({"query": "how does indexing work", "limit": 5}))
                .send()
                .await
                .expect("memory search must return a response, never a dropped connection");
            match resp.status().as_u16() {
                200 => {
                    succeeded = true;
                    break;
                }
                429 => {
                    let retry_after = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .expect("a shed 429 must carry a parseable Retry-After");
                    tokio::time::sleep(Duration::from_secs(retry_after)).await;
                }
                // The server answered, but only after its own request timeout
                // expired while the query waited its turn on the shared model.
                // That is admission-control fairness under a saturated index,
                // a separate concern from the liveness property under test:
                // report it as such rather than as an unreachable server.
                408 => panic!(
                    "memory search was still queued on the embedder when the server's own \
                     request timeout expired. Liveness is not the problem here (the server \
                     answered); this is queue fairness in front of the shared model, and it \
                     needs its own task rather than a wider change here"
                ),
                other => panic!("memory search returned an unexpected status {other}"),
            }
        }
        succeeded
    });

    // Sample liveness for the whole embed phase.
    let mut samples = 0usize;
    let mut max_latency = Duration::ZERO;
    let mut first: Option<(String, String)> = None;
    while !index.is_finished() {
        let sample = sample_health(&client, &base).await;
        samples += 1;
        max_latency = max_latency.max(sample.latency);

        assert!(
            sample.latency < LIVENESS_BOUND,
            "liveness sample {samples} took {:?}, over the {LIVENESS_BOUND:?} bound, \
             while the embedder was busy",
            sample.latency
        );
        assert_eq!(
            sample.instance_id.len(),
            36,
            "every sample must carry the full instance id, the field `server status` \
             renders as reachable"
        );
        assert!(
            !sample.version.is_empty(),
            "every sample must carry the server version"
        );
        assert!(
            sample.token_cap.is_u64(),
            "the advertised token cap must stay populated while the embedder is busy, got {}",
            sample.token_cap
        );
        let identity = (sample.instance_id, sample.version);
        match &first {
            None => first = Some(identity),
            Some(expected) => assert_eq!(
                &identity, expected,
                "server identity must be stable across the run"
            ),
        }

        tokio::time::sleep(SAMPLE_INTERVAL).await;
    }

    index.await.expect("index phase");
    assert!(
        samples >= 4,
        "the embed phase finished too fast to prove anything ({samples} samples); \
         raise SPELUNK_HEALTH_LOAD_CHUNKS"
    );
    assert!(
        search.await.expect("search task"),
        "concurrent memory search must complete successfully within the run"
    );

    println!("liveness samples: {samples}, max latency: {max_latency:?}");
}
