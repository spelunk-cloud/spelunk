//! Cloud two-way sync wire client.
//!
//! Wires the CLI's `sync` / `memory pull` commands to cloud-api primitives that
//! already exist server-side but were previously unreachable from the CLI:
//!
//! - `GET  /v1/projects/{id}/memory/since?since_id=<cursor>` — delta pull.
//! - `POST /v1/projects/{id}/memory/batch`                   — batched delta push.
//!
//! Pull cursor (decision #183): the cursor is the max cloud `remote_id` already
//! synced locally (a UUIDv7), not a wall-clock watermark — clock-drift-free.
//! The server returns entries whose `id` sorts strictly after it.
//!
//! Identity & idempotency: each pushed entry carries its stable UUID as the
//! cloud `external_id`, so the server's batch endpoint dedupes by identity
//! (re-running a sync skips already-present entries — 207 `skipped`). Pulled
//! entries carry the cloud `id` (a UUID), which we record as the local
//! `remote_id` and dedupe on, so a subsequent push of the same entry is a no-op.
//!
//! Embedding conformance: a push is **text-only by default** — the `vector`
//! field is omitted and the server backfills the embedding with its
//! configured model. As a compute/bandwidth optimization, when the
//! destination advertises the `accepts_pushed_vectors` capability the CLI MAY
//! attach the locally-computed full-precision (fp32/896) vector it already
//! holds, tagged with the model and precision the accept side validates. A
//! server without the capability (an older server, or the OSS team server)
//! never receives a vector and re-embeds — so text-only remains the universal
//! fallback.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::encode_project_id;

/// Per-request timeout for the sync HTTP client. `POST /memory/batch` performs
/// server-side embedding backfill, an inference-class operation: a cold embedder
/// plus a large with-vectors payload can legitimately run well past the 30s
/// per-entry CRUD ceiling while still making progress, so the sync path belongs
/// in the inference timeout class instead. This is a client-level timeout, so it
/// also raises the ceiling on `pull_since`/`delete_remote`: strictly better,
/// since only a hung connection ever reaches it and a slow-but-progressing pull
/// is no longer cut short at 30s.
const SYNC_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Precision tag sent alongside a client-pushed memory vector. Memory vectors
/// cross to the cloud, so they are ALWAYS full-precision fp32 — never the
/// int8/halfvec quantisation used for the local code index. The accept side
/// rejects (4xx) any other precision, so it is never sent.
const PUSHED_VECTOR_PRECISION: &str = "fp32";

/// The `vector_model` tag a vector-accepting server validates for exact string
/// equality. It is the model *family* portion of [`crate::embeddings::MODEL_ID`]
/// (`"F2LLM-v2-330M@896"`) with the `@<dim>` suffix stripped — the dimension is
/// carried by the fixed 896-dim contract, and the accept side compares only the
/// family string (its own model constant has no `@<dim>` suffix). Deriving it
/// from `MODEL_ID` keeps a single source of truth for the model identity.
fn pushed_vector_model_tag() -> &'static str {
    let id = crate::embeddings::MODEL_ID;
    id.split('@').next().unwrap_or(id)
}

/// One entry pushed to `POST /memory/batch`.
///
/// `external_id` carries the local entry's stable UUID — the server's
/// idempotency key. `vector`/`vector_model`/`vector_precision` are the optional
/// client-pushed embedding fast path: omitted for a text-only push (the
/// default), populated only for a server advertising `accepts_pushed_vectors`
/// via [`BatchPushItem::maybe_attach_vector`].
#[derive(Debug, Serialize)]
pub struct BatchPushItem {
    pub kind: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Stable cross-store identity → server idempotency key.
    pub external_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<String>,
    /// Locally-computed full-precision (fp32) embedding. Present only when the
    /// destination advertises `accepts_pushed_vectors`; otherwise omitted so the
    /// server re-embeds (text-only fallback).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
    /// Model tag for a pushed `vector` (accept-side field `vector_model`).
    /// Required by the server whenever `vector` is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_model: Option<String>,
    /// Precision of a pushed `vector` (accept-side field `vector_precision`);
    /// always `"fp32"` when present. Required by the server whenever `vector`
    /// is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_precision: Option<String>,
}

impl BatchPushItem {
    /// Attach a locally-computed embedding to this item, gated on the
    /// destination server advertising `accepts_pushed_vectors`.
    ///
    /// When `server_accepts_vectors` is false (an older server, or the OSS team
    /// server) or `vector` is `None` (the local row has no embedding), the item
    /// is left text-only — the server re-embeds. When both hold, the fp32 vector
    /// is attached alongside its model tag and `vector_precision = "fp32"`, and
    /// the server stores it verbatim (no re-embed). Precision is ALWAYS fp32;
    /// reduced precision is never sent because memory vectors cross to the
    /// cloud.
    pub fn maybe_attach_vector(
        mut self,
        server_accepts_vectors: bool,
        vector: Option<Vec<f32>>,
    ) -> Self {
        if let (true, Some(v)) = (server_accepts_vectors, vector) {
            self.vector = Some(v);
            self.vector_model = Some(pushed_vector_model_tag().to_string());
            self.vector_precision = Some(PUSHED_VECTOR_PRECISION.to_string());
        }
        self
    }
}

#[derive(Debug, Serialize)]
struct BatchPushBody {
    entries: Vec<BatchPushItem>,
}

/// Per-entry outcome from the 207 batch response.
#[derive(Debug, Deserialize)]
pub struct BatchItemResult {
    pub status: String,
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
}

/// Aggregate batch push result (207 Multi-Status).
#[derive(Debug, Deserialize)]
pub struct BatchPushResult {
    pub created: u32,
    pub skipped: u32,
    pub failed: u32,
    #[serde(default)]
    pub results: Vec<BatchItemResult>,
}

/// One relationship edge pushed to `POST /memory/batch` via the request's
/// `edges[]` array.
///
/// Each endpoint is addressed by its `external_id` (the entry's stable uuid,
/// the only id the batch edge route resolves), never the machine-local row id.
/// `kind` is a fixed edge kind: sync pushes only `"relates_to"`, since a
/// `supersedes` edge already travels with its entry's lifecycle and
/// `contradicts` is server-generated. Mirrors cloud-api's batch `edges[]`
/// element and `CloudApiMemoryBackend::supersede`'s single-edge post.
#[derive(Debug, Serialize)]
pub struct SyncEdgePush {
    pub from_external_id: String,
    pub to_external_id: String,
    pub kind: &'static str,
}

/// An edge-only batch body: `entries` is required by the route but stays empty,
/// so this posts edges without touching entries.
#[derive(Debug, Serialize)]
struct BatchEdgePushBody {
    entries: [(); 0],
    edges: Vec<SyncEdgePush>,
}

/// Per-edge acknowledgement in the 207 `edges[]` response.
#[derive(Debug, Deserialize)]
struct BatchEdgeAck {
    status: String,
}

/// Result of an edge batch push: the per-edge acknowledgements the server
/// returned in the 207 `edges[]` array.
#[derive(Debug, Deserialize, Default)]
pub struct EdgePushResult {
    #[serde(default)]
    edges: Vec<BatchEdgeAck>,
}

impl EdgePushResult {
    /// Count of edges the server actually stored. An `unresolved` edge (an
    /// endpoint the server does not know yet) is a no-op to retry on a later
    /// sync, not a success, so it is not counted here: this matches
    /// `CloudApiMemoryBackend`'s supersede `edge_applied`.
    pub fn applied(&self) -> usize {
        self.edges
            .iter()
            .filter(|e| matches!(e.status.as_str(), "created" | "applied" | "updated"))
            .count()
    }

    /// Total edges the server acknowledged, applied or not (the length of the
    /// returned `edges[]` array).
    pub fn acknowledged(&self) -> usize {
        self.edges.len()
    }
}

/// One entry returned by `GET /memory/since`.
///
/// Mirrors cloud-api's `EntryResponse`; the embedding vector is never sent by
/// the server, so it is absent here by design.
#[derive(Debug, Deserialize)]
pub struct RemoteEntry {
    pub id: String,
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub source_commit: Option<String>,
    #[serde(default)]
    pub archived_at: Option<String>,
    pub created_at: String,
}

impl RemoteEntry {
    /// Whether the cloud considers this entry archived/tombstoned.
    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }
}

#[derive(Debug, Deserialize)]
struct SinceBody {
    #[serde(default)]
    entries: Vec<RemoteEntry>,
}

/// HTTP client for the cloud two-way sync endpoints.
///
/// Separate from [`super::RemoteMemoryBackend`] because sync speaks a different
/// (batch + delta) wire protocol than the per-entry CRUD trait.
pub struct CloudSyncClient {
    client: reqwest::Client,
    base_url: String,
    project_id: String,
    api_key: Option<String>,
}

impl CloudSyncClient {
    /// Build a client. `project_id` is the server-side project identifier
    /// (a UUID for cloud-api, or a slug for an OSS spelunk-server); it is
    /// percent-encoded into a single path segment either way.
    pub fn new(
        base_url: &str,
        project_id: &str,
        api_key: Option<&str>,
        server_ca: Option<&std::path::Path>,
    ) -> Result<Self> {
        Self::with_timeout(
            base_url,
            project_id,
            api_key,
            server_ca,
            SYNC_REQUEST_TIMEOUT,
        )
    }

    /// Build a client with an explicit per-request timeout. Production always
    /// uses [`SYNC_REQUEST_TIMEOUT`] via [`Self::new`]; a caller passes a short
    /// timeout only to exercise the enforcement path without waiting on the 300s
    /// production ceiling.
    fn with_timeout(
        base_url: &str,
        project_id: &str,
        api_key: Option<&str>,
        server_ca: Option<&std::path::Path>,
        timeout: Duration,
    ) -> Result<Self> {
        // Fail closed before building the client: a bearer must never travel over
        // plaintext http to a non-loopback host. Keyless
        // loopback-dev construction is unaffected: nothing to leak.
        if api_key.is_some() {
            crate::config::validate_transport_url(base_url).map_err(anyhow::Error::msg)?;
        }
        let client = crate::config::apply_server_ca(reqwest::Client::builder(), server_ca)?
            .timeout(timeout)
            .build()
            .context("building sync HTTP client")?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            project_id: project_id.to_string(),
            api_key: api_key.map(str::to_string),
        })
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/v1/projects/{}/{}",
            self.base_url,
            encode_project_id(&self.project_id),
            path
        )
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => req.header("Authorization", format!("Bearer {key}")),
            None => req,
        }
    }

    /// URL for the live-pull SSE endpoint (`GET /memory/stream`), percent-encoding
    /// `project_id` the same way every other request here does.
    ///
    /// This client does not itself hold a streaming connection open (its other
    /// methods are all single-shot request/response); a caller that needs one
    /// builds its own request against this URL (see
    /// `crates/spelunk-server/src/relay.rs`, the only place in the workspace
    /// that does, for ADR-037 P2's local relay role).
    pub fn stream_url(&self) -> String {
        self.url("memory/stream")
    }

    /// Push a batch of text-only entries. Idempotent on `external_id`.
    ///
    /// Returns the server's aggregate result. An empty input is a no-op.
    pub async fn push_batch(&self, entries: Vec<BatchPushItem>) -> Result<BatchPushResult> {
        if entries.is_empty() {
            return Ok(BatchPushResult {
                created: 0,
                skipped: 0,
                failed: 0,
                results: vec![],
            });
        }
        let body = BatchPushBody { entries };
        let resp = self
            .authed(self.client.post(self.url("memory/batch")))
            .json(&body)
            .send()
            .await
            .context("POST /memory/batch")?;

        // The endpoint always returns 207 Multi-Status on success; treat any
        // 2xx as parseable and surface other statuses as errors.
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("POST /memory/batch failed ({status}): {text}");
        }
        resp.json::<BatchPushResult>()
            .await
            .context("parsing /memory/batch response")
    }

    /// Push a batch of relationship edges via the same `POST /memory/batch`
    /// route, carrying them in the request's `edges[]` array with an empty
    /// `entries[]`.
    ///
    /// Idempotent server-side (the batch edge route dedupes on `ON CONFLICT DO
    /// NOTHING`), so a re-push is harmless. An edge naming an endpoint the
    /// server does not know yet comes back `unresolved`, which
    /// [`EdgePushResult::applied`] reports as not-applied rather than an error:
    /// the endpoint just is not synced yet, and a later sync retries it. An
    /// empty input is a no-op with no request.
    pub async fn push_edges(&self, edges: Vec<SyncEdgePush>) -> Result<EdgePushResult> {
        if edges.is_empty() {
            return Ok(EdgePushResult::default());
        }
        let body = BatchEdgePushBody { entries: [], edges };
        let resp = self
            .authed(self.client.post(self.url("memory/batch")))
            .json(&body)
            .send()
            .await
            .context("POST /memory/batch (edges)")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("POST /memory/batch edges failed ({status}): {text}");
        }
        resp.json::<EdgePushResult>()
            .await
            .context("parsing /memory/batch edges response")
    }

    /// Tombstone a cloud entry by its cloud-minted id (`DELETE /memory/{id}`).
    ///
    /// Propagates a local archive to the cloud. Already-archived or
    /// missing entries return 404 server-side, which we treat as success (the
    /// desired end state — gone — already holds), keeping the call idempotent.
    pub async fn delete_remote(&self, remote_id: &str) -> Result<()> {
        let resp = self
            .authed(self.client.delete(self.url(&format!("memory/{remote_id}"))))
            .send()
            .await
            .context("DELETE /memory/{id}")?;
        let status = resp.status();
        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("DELETE /memory/{remote_id} failed ({status}): {text}")
    }

    /// Maximum `limit` the server accepts on `GET /memory/since`
    /// (`ServerDb::notes_since_id`'s `limit.clamp(1, 500)`). A request's
    /// `limit` only needs to satisfy `limit <= MEMORY_SINCE_MAX_LIMIT`; the
    /// server silently clamps down to this value if it's violated. Scoped to
    /// this one endpoint, not a generic page-size ceiling.
    pub const MEMORY_SINCE_MAX_LIMIT: i64 = 500;

    /// `limit` this client requests per `/memory/since` page. Matches the
    /// server's own default (`default_since_limit()` in handlers.rs), so
    /// sending it explicitly behaves identically to omitting the param; sent
    /// explicitly anyway so pagination's "did this page prove nothing
    /// remains" check has a fixed value to compare against that can't
    /// silently drift if the server's own default ever changes. Well under
    /// [`MEMORY_SINCE_MAX_LIMIT`](Self::MEMORY_SINCE_MAX_LIMIT): a larger
    /// per-page request is a pure throughput/latency tradeoff (fewer, bigger
    /// requests vs. more, smaller ones), not a correctness lever — a
    /// pagination loop that stops on a short page is already immune to
    /// backlog size regardless of page size, so there is no correctness
    /// reason to request more, and a smaller page is kinder to slow
    /// connections and unusually large individual entries.
    pub const MEMORY_SINCE_PULL_LIMIT: i64 = 100;

    /// Pull one page of entries after the UUIDv7 cursor `since_id` (the max
    /// cloud `remote_id` already synced locally — decision #183). When
    /// `since_id` is `None` (nothing synced yet) this is a full catch-up
    /// start: the nil UUID `00000000-…` sorts before every UUIDv7, so it
    /// returns from the very beginning.
    ///
    /// Requests [`MEMORY_SINCE_PULL_LIMIT`](Self::MEMORY_SINCE_PULL_LIMIT)
    /// entries and returns at most that many. This method does not paginate
    /// on its own; a caller pulling a store's entire backlog must loop (see
    /// `pull_and_apply_since` in spelunk-cli), calling again with the cursor
    /// advanced to the last entry's `id` until a page comes back shorter than
    /// the requested limit — the definitive "nothing left" signal, including
    /// the empty-page case. A response exactly at the limit does not by
    /// itself prove more remain, but it can never be treated as the last page
    /// either, since the server never returns more than requested even when
    /// more exist.
    ///
    /// A project that does not exist on the server yet (404) is treated as
    /// having nothing to pull, not an error: a spelunk-server-backed project
    /// is only created lazily by the first push to it (`memory/batch`), so a
    /// pull that runs before any push has ever landed for a brand new project
    /// (e.g. `sync`'s own first pull pass, ahead of its own push) must not
    /// fail the whole sync just because nobody has pushed yet.
    pub async fn pull_since(&self, since_id: Option<&str>) -> Result<Vec<RemoteEntry>> {
        // The all-zero UUID precedes every UUIDv7 in `id > $cursor` order, so it
        // is the natural "from the beginning" cursor for a first sync.
        let cursor = since_id.unwrap_or("00000000-0000-0000-0000-000000000000");
        let limit = Self::MEMORY_SINCE_PULL_LIMIT.to_string();
        let resp = self
            .authed(self.client.get(self.url("memory/since")))
            .query(&[("since_id", cursor), ("limit", limit.as_str())])
            .send()
            .await
            .context("GET /memory/since")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(vec![]);
        }
        let body = resp
            .error_for_status()
            .context("server returned error for GET /memory/since")?
            .json::<SinceBody>()
            .await
            .context("parsing /memory/since response")?;
        Ok(body.entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn item(ext: &str) -> BatchPushItem {
        BatchPushItem {
            kind: "decision".into(),
            title: "T".into(),
            body: Some("B".into()),
            external_id: ext.into(),
            source_commit: None,
            vector: None,
            vector_model: None,
            vector_precision: None,
        }
    }

    /// A valid L2-normalised fp32/896 vector (norm == 1.0), the shape a local
    /// `note_embeddings` row holds.
    fn unit_vec_896() -> Vec<f32> {
        let n = spelunk_core_embedding_dim();
        vec![1.0 / (n as f32).sqrt(); n]
    }

    fn spelunk_core_embedding_dim() -> usize {
        crate::embeddings::EMBEDDING_DIM
    }

    // ── request timeout: inference-class, and actually enforced ──────────────

    #[test]
    fn sync_request_timeout_is_inference_class_not_the_old_cap() {
        // Guards against a silent revert to the old 30s per-entry CRUD cap:
        // `/memory/batch` re-embeds server-side, so the client timeout must stay
        // in the inference class.
        assert_eq!(SYNC_REQUEST_TIMEOUT, Duration::from_secs(300));
        assert!(SYNC_REQUEST_TIMEOUT > Duration::from_secs(30));
    }

    #[tokio::test]
    async fn request_timeout_is_actually_enforced_per_request() {
        // A response delayed past the client timeout must surface as a timeout
        // error; the same slow response under a looser timeout must succeed. This
        // proves the per-request timeout is wired to the client, exercised with a
        // short injected value so it runs sub-second, decoupled from the 300s
        // production literal.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(
                ResponseTemplate::new(207)
                    .set_body_json(serde_json::json!({
                        "created": 1, "skipped": 0, "failed": 0,
                        "results": [{"status": "created", "external_id": "e1", "id": "c1"}]
                    }))
                    .set_delay(Duration::from_millis(300)),
            )
            .mount(&server)
            .await;

        let tight = CloudSyncClient::with_timeout(
            &server.uri(),
            "proj",
            None,
            None,
            Duration::from_millis(50),
        )
        .unwrap();
        let err = tight.push_batch(vec![item("e1")]).await.unwrap_err();
        let chain = format!("{err:#}").to_lowercase();
        assert!(
            chain.contains("time"),
            "a response past the client timeout must surface as a timeout: {err:#}"
        );

        let loose = CloudSyncClient::with_timeout(
            &server.uri(),
            "proj",
            None,
            None,
            Duration::from_secs(5),
        )
        .unwrap();
        let res = loose.push_batch(vec![item("e1")]).await.unwrap();
        assert_eq!(res.created, 1);
    }

    /// The push is text-only by default, and carries the fp32/896 vector + model
    /// tag ONLY for a server advertising `accepts_pushed_vectors`.
    #[tokio::test]
    async fn push_batch_is_text_only_no_vector() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": "e1", "id": "cloud-1"}]
            })))
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

        // ── Case A: server WITHOUT the capability → text-only, no vector at all,
        // even though a local vector is available to attach. ──────────────────
        let text_only = item("e1").maybe_attach_vector(false, Some(unit_vec_896()));
        client.push_batch(vec![text_only]).await.unwrap();

        // ── Case B: server WITH the capability → fp32/896 vector + model tag. ──
        let with_vec = item("e2").maybe_attach_vector(true, Some(unit_vec_896()));
        client.push_batch(vec![with_vec]).await.unwrap();

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 2);

        // Case A body: external_id present, no vector/model/precision keys.
        let body_a = String::from_utf8(reqs[0].body.clone()).unwrap();
        assert!(body_a.contains("\"external_id\":\"e1\""), "body: {body_a}");
        assert!(
            !body_a.contains("vector"),
            "no-capability push must be text-only: {body_a}"
        );

        // Case B body: fp32/896 vector + exact model tag + precision "fp32".
        let json_b: serde_json::Value =
            serde_json::from_slice(&reqs[1].body).expect("valid JSON body");
        let entry = &json_b["entries"][0];
        let vec = entry["vector"]
            .as_array()
            .expect("vector must be a JSON array when the server accepts it");
        assert_eq!(
            vec.len(),
            spelunk_core_embedding_dim(),
            "pushed vector must be 896-dim"
        );
        assert!(
            vec.iter().all(|v| v.is_number()),
            "vector components must be fp32 numbers: {entry}"
        );
        // The tag is the model family with no `@<dim>` suffix (accept-side
        // contract); the dim travels separately.
        assert_eq!(entry["vector_model"], "F2LLM-v2-330M");
        assert!(
            entry["vector_model"]
                .as_str()
                .is_some_and(|s| !s.contains('@')),
            "model tag must not carry the @<dim> suffix: {entry}"
        );
        assert_eq!(entry["vector_precision"], "fp32");
    }

    #[tokio::test]
    async fn push_batch_empty_is_noop_no_request() {
        let server = MockServer::start().await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let res = client.push_batch(vec![]).await.unwrap();
        assert_eq!((res.created, res.skipped, res.failed), (0, 0, 0));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    fn relates_edge(from: &str, to: &str) -> SyncEdgePush {
        SyncEdgePush {
            from_external_id: from.into(),
            to_external_id: to.into(),
            kind: "relates_to",
        }
    }

    // A relates_to push must hit the same `/memory/batch` route the entry push
    // uses, but as an edge-only body: `entries` empty, one `edges[]` element
    // keyed by external_id. A 207 `{"edges":[{"status":"created"}]}` counts as
    // applied.
    #[tokio::test]
    async fn push_edges_posts_an_edge_only_relates_to_batch_body() {
        use wiremock::matchers::body_json;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .and(body_json(serde_json::json!({
                "entries": [],
                "edges": [{
                    "from_external_id": "ext-from",
                    "to_external_id": "ext-to",
                    "kind": "relates_to",
                }],
            })))
            .respond_with(
                ResponseTemplate::new(207)
                    .set_body_json(serde_json::json!({"edges": [{"status": "created"}]})),
            )
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let res = client
            .push_edges(vec![relates_edge("ext-from", "ext-to")])
            .await
            .unwrap();
        assert_eq!(res.applied(), 1, "a created edge must count as applied");
    }

    // An edge naming an endpoint the server does not know yet comes back
    // `unresolved`. That is "not yet, retry later", not a failure: the call
    // must succeed and simply not count the edge as applied.
    #[tokio::test]
    async fn an_unresolved_edge_push_is_graceful_and_not_counted_as_applied() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(
                ResponseTemplate::new(207)
                    .set_body_json(serde_json::json!({"edges": [{"status": "unresolved"}]})),
            )
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let res = client
            .push_edges(vec![relates_edge("ext-from", "ext-to")])
            .await
            .expect("an unresolved edge must not surface as an error");
        assert_eq!(res.applied(), 0, "unresolved must not read as applied");
        assert_eq!(
            res.acknowledged(),
            1,
            "the edge was acknowledged, just not resolved"
        );
    }

    #[tokio::test]
    async fn push_edges_empty_is_a_noop_with_no_request() {
        let server = MockServer::start().await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let res = client.push_edges(vec![]).await.unwrap();
        assert_eq!(res.applied(), 0);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn push_batch_reports_skipped_for_idempotent_repush() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 0, "skipped": 1, "failed": 0,
                "results": [{"status": "skipped", "external_id": "e1"}]
            })))
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let res = client.push_batch(vec![item("e1")]).await.unwrap();
        assert_eq!(res.skipped, 1);
        assert_eq!(res.created, 0);
    }

    #[tokio::test]
    async fn pull_since_passes_uuid_cursor_and_parses_entries() {
        let server = MockServer::start().await;
        // The cursor must travel as `since_id` (a UUIDv7), not a timestamp `t`.
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .and(query_param(
                "since_id",
                "01890000-0000-7000-8000-000000000000",
            ))
            .and(query_param(
                "limit",
                CloudSyncClient::MEMORY_SINCE_PULL_LIMIT
                    .to_string()
                    .as_str(),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [{
                    "id": "01890000-0000-7000-8000-000000000001",
                    "kind": "decision", "title": "Remote",
                    "body": "body", "created_at": "2026-06-19T01:00:00Z"
                }],
                "count": 1
            })))
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let entries = client
            .pull_since(Some("01890000-0000-7000-8000-000000000000"))
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "01890000-0000-7000-8000-000000000001");
        assert!(!entries[0].is_archived());
    }

    /// A project not yet created server-side (nobody has ever pushed to it)
    /// must read as "nothing to pull", not an error: the two-phase sync
    /// reconciliation pulls before the round's own push runs, so a brand
    /// new project's very first sync must not fail just because it hasn't
    /// been lazily created by a push yet.
    #[tokio::test]
    async fn pull_since_project_not_found_yet_is_treated_as_empty() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/brand-new-proj/memory/since"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "brand-new-proj", None, None).unwrap();
        let entries = client.pull_since(None).await.unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn memory_since_max_limit_matches_the_server_clamp_ceiling() {
        // `ServerDb::notes_since_id` clamps `limit` to `1..=500`. This is a
        // regression guard on that server-side fact, not a claim about what
        // this client requests (see `MEMORY_SINCE_PULL_LIMIT` for that).
        assert_eq!(CloudSyncClient::MEMORY_SINCE_MAX_LIMIT, 500);
    }

    #[test]
    fn memory_since_pull_limit_matches_the_server_default_and_stays_under_the_max() {
        // 100 matches `default_since_limit()` (handlers.rs) — the value the
        // server already applies with no `limit` param — chosen for slow
        // connections and large entries, not for minimizing round trips.
        // (100 <= 500 is self-evident from this and the sibling test above,
        // so isn't asserted separately: both sides are compile-time
        // constants and clippy rejects an assertion with a constant value.)
        assert_eq!(CloudSyncClient::MEMORY_SINCE_PULL_LIMIT, 100);
    }

    #[tokio::test]
    async fn pull_since_requests_the_page_limit_explicitly() {
        // Sent explicitly (rather than omitted and left to the server's own
        // default) so a caller looping pages has a fixed value to compare
        // each page's length against, insulated from the server's default
        // ever changing. Match on path only (not `limit`) so a wrong/missing
        // value surfaces in the captured request instead of being masked by
        // wiremock's 404-no-match default, which `pull_since` itself treats
        // as an empty-but-successful pull.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "entries": [], "count": 0 })),
            )
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        client.pull_since(None).await.unwrap();
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        let query: std::collections::HashMap<_, _> = reqs[0].url.query_pairs().collect();
        assert_eq!(
            query.get("limit").map(|v| v.as_ref()),
            Some(
                CloudSyncClient::MEMORY_SINCE_PULL_LIMIT
                    .to_string()
                    .as_str()
            )
        );
    }

    #[tokio::test]
    async fn pull_since_none_uses_nil_uuid_cursor() {
        let server = MockServer::start().await;
        // A first sync (no cursor yet) must send the all-zero UUID, which sorts
        // before every UUIDv7 → full catch-up.
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .and(query_param(
                "since_id",
                "00000000-0000-0000-0000-000000000000",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "entries": [], "count": 0 })),
            )
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let entries = client.pull_since(None).await.unwrap();
        assert!(entries.is_empty());
    }

    /// Only 404 (project not yet created) reads as "nothing to pull". A real
    /// server error (500, or any other non-2xx/404 status) must surface as
    /// `Err`, not be swallowed the same way — a caller looping pages (see
    /// `pull_and_apply_since` in spelunk-cli) depends on this distinction to
    /// tell "server has nothing yet" apart from "the request actually
    /// failed mid-pagination".
    #[tokio::test]
    async fn pull_since_server_error_propagates_distinct_from_the_404_empty_case() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let err = client
            .pull_since(None)
            .await
            .expect_err("a 500 must not be treated as an empty page like a 404 is");
        assert!(format!("{err:#}").contains("memory/since"), "err: {err:#}");
    }

    #[tokio::test]
    async fn pull_since_marks_archived_when_archived_at_present() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [{
                    "id": "cloud-2", "kind": "note", "title": "Gone",
                    "archived_at": "2026-06-19T02:00:00Z",
                    "created_at": "2026-06-19T01:00:00Z"
                }],
                "count": 1
            })))
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let entries = client.pull_since(None).await.unwrap();
        assert!(entries[0].is_archived());
    }

    #[tokio::test]
    async fn push_batch_threads_explicit_slug_into_request_path() {
        // An explicit project slug (e.g. from `spelunk sync
        // --project acme/new-app`) must reach the server verbatim in the request
        // path, so the server can lazily create/reuse that project on first sync.
        // The mock only matches the slug-scoped path, so a match proves it.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/acme%2Fnew-app/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": "e1", "id": "cloud-1"}]
            })))
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "acme/new-app", None, None).unwrap();
        let res = client.push_batch(vec![item("e1")]).await.unwrap();
        assert_eq!(res.created, 1);
    }

    // ── transport-scheme guard at construction ───────────────────────────────
    // A bearer must never travel over plaintext http to a non-loopback host;
    // keyless construction is unaffected. Mirrors config::validate_transport_url_*.

    #[test]
    fn new_with_key_rejects_non_loopback_http() {
        let err =
            match CloudSyncClient::new("http://team-server:7777", "proj", Some("secret"), None) {
                Err(e) => e.to_string(),
                Ok(_) => panic!("expected non-loopback plaintext http to be rejected"),
            };
        assert!(err.contains("plaintext http"), "err: {err}");
    }

    #[test]
    fn new_with_key_accepts_https_and_loopback_http() {
        assert!(
            CloudSyncClient::new("https://team-server:7777", "proj", Some("secret"), None).is_ok()
        );
        assert!(
            CloudSyncClient::new("http://127.0.0.1:7777", "proj", Some("secret"), None).is_ok()
        );
        assert!(
            CloudSyncClient::new("http://localhost:7777", "proj", Some("secret"), None).is_ok()
        );
    }

    #[test]
    fn new_with_key_rejects_spoofed_loopback_authorities() {
        for url in [
            "http://127.0.0.1.evil.example",
            "http://127.0.0.1@evil.example",
            "http://127.0.0.1:1234@evil.example",
        ] {
            let err = match CloudSyncClient::new(url, "proj", Some("secret"), None) {
                Err(e) => e.to_string(),
                Ok(_) => panic!("{url} must not be accepted as loopback"),
            };
            assert!(err.contains("plaintext http"), "{url}: {err}");
        }
    }

    #[test]
    fn new_keyless_construction_unaffected_by_transport() {
        // No bearer to leak, so even a non-loopback plaintext dev server is fine.
        assert!(CloudSyncClient::new("http://team-server:7777", "proj", None, None).is_ok());
    }

    #[test]
    fn stream_url_percent_encodes_project_slug() {
        let client = CloudSyncClient::new("https://team.example", "acme/app", None, None).unwrap();
        assert_eq!(
            client.stream_url(),
            "https://team.example/v1/projects/acme%2Fapp/memory/stream"
        );
    }

    #[tokio::test]
    async fn delete_remote_treats_404_as_success() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1/projects/proj/memory/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        // 404 (already gone) is the desired end state → Ok.
        client.delete_remote("missing").await.unwrap();
    }
}
