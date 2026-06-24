//! Cloud two-way sync wire client (ADR-037 D2/D3).
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
//! Embedding conformance (ADR-010/ADR-020, ADR-037 D3): pushes are **text only**
//! — the `vector` field is always omitted and the server backfills with its
//! configured model. There is deliberately no client-vector send path here.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::encode_project_id;

/// One entry pushed to `POST /memory/batch`.
///
/// `external_id` carries the local entry's stable UUID — the server's
/// idempotency key. `vector` is intentionally absent (text-only; ADR-037 D3).
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
    pub fn new(base_url: &str, project_id: &str, api_key: Option<&str>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
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

    /// Tombstone a cloud entry by its cloud-minted id (`DELETE /memory/{id}`).
    ///
    /// Propagates a local archive to the cloud (ADR-037 D2). Already-archived or
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

    /// Pull entries after the UUIDv7 cursor `since_id` (the max cloud
    /// `remote_id` already synced locally — decision #183). When `since_id` is
    /// `None` (nothing synced yet) this is a full catch-up: the
    /// nil UUID `00000000-…` sorts before every UUIDv7, so it returns all
    /// entries.
    pub async fn pull_since(&self, since_id: Option<&str>) -> Result<Vec<RemoteEntry>> {
        // The all-zero UUID precedes every UUIDv7 in `id > $cursor` order, so it
        // is the natural "from the beginning" cursor for a first sync.
        let cursor = since_id.unwrap_or("00000000-0000-0000-0000-000000000000");
        let resp = self
            .authed(self.client.get(self.url("memory/since")))
            .query(&[("since_id", cursor)])
            .send()
            .await
            .context("GET /memory/since")?
            .error_for_status()
            .context("server returned error for GET /memory/since")?
            .json::<SinceBody>()
            .await
            .context("parsing /memory/since response")?;
        Ok(resp.entries)
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
        }
    }

    #[tokio::test]
    async fn push_batch_is_text_only_no_vector() {
        let server = MockServer::start().await;
        // The pushed body must NOT contain a vector/embedding field (ADR-037 D3).
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": "e1", "id": "cloud-1"}]
            })))
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None).unwrap();
        let res = client.push_batch(vec![item("e1")]).await.unwrap();
        assert_eq!(res.created, 1);

        // Inspect the recorded request body — it must carry external_id but no
        // vector/embedding key at all.
        let reqs = server.received_requests().await.unwrap();
        let body = String::from_utf8(reqs[0].body.clone()).unwrap();
        assert!(body.contains("\"external_id\":\"e1\""), "body: {body}");
        assert!(!body.contains("vector"), "push must be text-only: {body}");
        assert!(
            !body.contains("embedding"),
            "push must be text-only: {body}"
        );
    }

    #[tokio::test]
    async fn push_batch_empty_is_noop_no_request() {
        let server = MockServer::start().await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None).unwrap();
        let res = client.push_batch(vec![]).await.unwrap();
        assert_eq!((res.created, res.skipped, res.failed), (0, 0, 0));
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
        let client = CloudSyncClient::new(&server.uri(), "proj", None).unwrap();
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

        let client = CloudSyncClient::new(&server.uri(), "proj", None).unwrap();
        let entries = client
            .pull_since(Some("01890000-0000-7000-8000-000000000000"))
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "01890000-0000-7000-8000-000000000001");
        assert!(!entries[0].is_archived());
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
        let client = CloudSyncClient::new(&server.uri(), "proj", None).unwrap();
        let entries = client.pull_since(None).await.unwrap();
        assert!(entries.is_empty());
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
        let client = CloudSyncClient::new(&server.uri(), "proj", None).unwrap();
        let entries = client.pull_since(None).await.unwrap();
        assert!(entries[0].is_archived());
    }

    #[tokio::test]
    async fn delete_remote_treats_404_as_success() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1/projects/proj/memory/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None).unwrap();
        // 404 (already gone) is the desired end state → Ok.
        client.delete_remote("missing").await.unwrap();
    }
}
