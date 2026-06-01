use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use super::backend::{MemoryBackend, NoteInput};
use super::memory::{MemoryEdge, Note};
use crate::embeddings::blob_to_vec;

/// HTTP client for the spelunk-server REST API.
///
/// All routes are scoped under `/v1/projects/{project_id}/`.
pub struct RemoteMemoryBackend {
    pub client: reqwest::Client,
    pub base_url: String,
    pub project_id: String,
    pub api_key: Option<String>,
}

impl RemoteMemoryBackend {
    fn url(&self, path: &str) -> String {
        format!(
            "{}/v1/projects/{}/{}",
            self.base_url.trim_end_matches('/'),
            self.project_id,
            path
        )
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(key) = &self.api_key {
            req.header("Authorization", format!("Bearer {key}"))
        } else {
            req
        }
    }
}

// ── Wire types (match server JSON schema) ─────────────────────────────────────

#[derive(Serialize)]
struct AddNoteRequest {
    kind: String,
    title: String,
    body: String,
    tags: Vec<String>,
    linked_files: Vec<String>,
    embedding: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_at: Option<i64>,
}

#[derive(Deserialize)]
struct AddNoteResponse {
    id: i64,
    #[serde(default)]
    conflicts: Vec<ConflictInfo>,
}

/// Conflict information returned by the server when a new note is semantically
/// close to an existing active entry (HTTP 409).
#[derive(Debug, Deserialize, Clone)]
pub struct ConflictInfo {
    pub id: i64,
    pub title: String,
    pub similarity: f32,
}

#[derive(Deserialize)]
struct NoteResponse {
    id: i64,
    kind: String,
    title: String,
    body: String,
    tags: Vec<String>,
    linked_files: Vec<String>,
    created_at: i64,
    status: String,
    superseded_by: Option<i64>,
    #[serde(default)]
    source_ref: Option<String>,
    #[serde(default)]
    valid_at: Option<i64>,
    #[serde(default)]
    invalid_at: Option<i64>,
    #[serde(default)]
    distance: Option<f64>,
}

impl From<NoteResponse> for Note {
    fn from(r: NoteResponse) -> Self {
        Note {
            id: r.id,
            kind: r.kind,
            title: r.title,
            body: r.body,
            tags: r.tags,
            linked_files: r.linked_files,
            created_at: r.created_at,
            status: r.status,
            superseded_by: r.superseded_by,
            source_ref: r.source_ref,
            valid_at: r.valid_at,
            invalid_at: r.invalid_at,
            distance: r.distance,
            score: None,
        }
    }
}

#[derive(Serialize)]
struct SearchRequest {
    embedding: Vec<f32>,
    limit: usize,
}

#[derive(Serialize)]
struct SupersedeRequest {
    new_id: i64,
}

#[derive(Deserialize)]
struct BoolResponse {
    changed: bool,
}

#[derive(Deserialize)]
struct CountResponse {
    count: i64,
}

// ── Trait implementation ──────────────────────────────────────────────────────

#[async_trait]
impl MemoryBackend for RemoteMemoryBackend {
    async fn add(&self, input: NoteInput) -> Result<i64> {
        let embedding = input.embedding.as_deref().map(blob_to_vec);
        let body = AddNoteRequest {
            kind: input.kind,
            title: input.title,
            body: input.body,
            tags: input.tags,
            linked_files: input.linked_files,
            embedding,
            source_ref: input.source_ref,
            valid_at: input.valid_at,
        };
        let http_resp = self
            .authed(self.client.post(self.url("memory")))
            .json(&body)
            .send()
            .await
            .context("POST /memory")?;

        let status = http_resp.status();

        // 409 means "stored but conflicting" — treat as success but emit a warning.
        if status == reqwest::StatusCode::CONFLICT {
            let resp = http_resp
                .json::<AddNoteResponse>()
                .await
                .context("parsing POST /memory 409 response")?;
            if !resp.conflicts.is_empty() {
                eprintln!("warning: memory entry conflicts with existing entries:");
                for c in &resp.conflicts {
                    eprintln!(
                        "  · #{} \"{}\" (similarity: {:.2})",
                        c.id, c.title, c.similarity
                    );
                }
            }
            return Ok(resp.id);
        }

        let resp = http_resp
            .error_for_status()
            .context("server returned error for POST /memory")?
            .json::<AddNoteResponse>()
            .await
            .context("parsing POST /memory response")?;
        Ok(resp.id)
    }

    /// Remote backend: timeline search falls back to regular semantic search.
    async fn search_timeline(&self, query_blob: &[u8], limit: usize) -> Result<Vec<Note>> {
        self.search(query_blob, limit, None).await
    }

    async fn search(
        &self,
        query_blob: &[u8],
        limit: usize,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        let embedding = blob_to_vec(query_blob);
        let body = SearchRequest { embedding, limit };
        let resp = self
            .authed(self.client.post(self.url("memory/search")))
            .json(&body)
            .send()
            .await
            .context("POST /memory/search")?
            .error_for_status()
            .context("server returned error for POST /memory/search")?
            .json::<Vec<NoteResponse>>()
            .await
            .context("parsing search response")?;
        Ok(resp.into_iter().map(Into::into).collect())
    }

    /// Remote backend: BM25 text search is not supported — falls back to semantic search.
    async fn search_text(
        &self,
        _query: &str,
        _limit: usize,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        anyhow::bail!(
            "BM25 text search is not supported by the remote memory backend. \
             Use --mode semantic or omit --mode to use the default hybrid mode."
        )
    }

    /// Remote backend: hybrid search falls back to semantic search
    /// (server-side FTS is not available in this client).
    async fn search_hybrid(
        &self,
        query_blob: &[u8],
        _query: &str,
        limit: usize,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        self.search(query_blob, limit, as_of).await
    }

    async fn list(
        &self,
        kind_filter: Option<&str>,
        limit: usize,
        include_archived: bool,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        let mut req = self.client.get(self.url("memory")).query(&[
            ("limit", limit.to_string().as_str()),
            ("archived", if include_archived { "true" } else { "false" }),
        ]);
        if let Some(kind) = kind_filter {
            req = req.query(&[("kind", kind)]);
        }
        if let Some(ts) = as_of {
            req = req.query(&[("as_of", ts.to_string().as_str())]);
        }
        let resp = self
            .authed(req)
            .send()
            .await
            .context("GET /memory")?
            .error_for_status()
            .context("server returned error for GET /memory")?
            .json::<Vec<NoteResponse>>()
            .await
            .context("parsing list response")?;
        Ok(resp.into_iter().map(Into::into).collect())
    }

    async fn get(&self, id: i64) -> Result<Option<Note>> {
        let resp = self
            .authed(self.client.get(self.url(&format!("memory/{id}"))))
            .send()
            .await
            .context("GET /memory/{id}")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let note = resp
            .error_for_status()
            .context("server returned error for GET /memory/{id}")?
            .json::<NoteResponse>()
            .await
            .context("parsing get response")?;
        Ok(Some(note.into()))
    }

    async fn count(&self) -> Result<i64> {
        let resp = self
            .authed(self.client.get(self.url("stats")))
            .send()
            .await
            .context("GET /stats")?
            .error_for_status()
            .context("server returned error for GET /stats")?
            .json::<CountResponse>()
            .await
            .context("parsing stats response")?;
        Ok(resp.count)
    }

    async fn archive(&self, id: i64) -> Result<bool> {
        let resp = self
            .authed(self.client.post(self.url(&format!("memory/{id}/archive"))))
            .send()
            .await
            .context("POST /memory/{id}/archive")?
            .error_for_status()
            .context("server returned error for POST /memory/{id}/archive")?
            .json::<BoolResponse>()
            .await
            .context("parsing archive response")?;
        Ok(resp.changed)
    }

    async fn supersede(&self, old_id: i64, new_id: i64) -> Result<bool> {
        let body = SupersedeRequest { new_id };
        let resp = self
            .authed(
                self.client
                    .post(self.url(&format!("memory/{old_id}/supersede"))),
            )
            .json(&body)
            .send()
            .await
            .context("POST /memory/{id}/supersede")?
            .error_for_status()
            .context("server returned error for POST /memory/{id}/supersede")?
            .json::<BoolResponse>()
            .await
            .context("parsing supersede response")?;
        Ok(resp.changed)
    }

    async fn list_by_source_ref(
        &self,
        source_ref_prefix: &str,
        limit: usize,
        include_archived: bool,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        let req = self.client.get(self.url("memory")).query(&[
            ("limit", limit.to_string().as_str()),
            ("archived", if include_archived { "true" } else { "false" }),
            ("source_ref", source_ref_prefix),
        ]);
        let resp = self
            .authed(req)
            .send()
            .await
            .context("GET /memory (source_ref filter)")?
            .error_for_status()
            .context("server returned error for GET /memory")?
            .json::<Vec<NoteResponse>>()
            .await
            .context("parsing list response")?;
        Ok(resp.into_iter().map(Into::into).collect())
    }

    async fn harvested_shas(&self) -> Result<HashSet<String>> {
        let resp = self
            .authed(self.client.get(self.url("memory/harvested-shas")))
            .send()
            .await
            .context("GET /memory/harvested-shas")?
            .error_for_status()
            .context("server returned error for GET /memory/harvested-shas")?
            .json::<Vec<String>>()
            .await
            .context("parsing harvested-shas response")?;
        Ok(resp.into_iter().collect())
    }

    async fn has_source_ref(&self, sha: &str) -> Result<bool> {
        // Reuse the list endpoint with the full SHA as prefix; if any results come back,
        // this commit has been harvested.
        let notes = self.list_by_source_ref(sha, 1, true, None).await?;
        Ok(!notes.is_empty())
    }

    /// Remote backend: edge mutations are not supported — no-op.
    async fn add_edge(&self, _from_id: i64, _to_id: i64, _kind: &str) -> Result<()> {
        Ok(())
    }

    /// Remote backend: edge queries are not supported — returns empty lists.
    async fn get_edges(&self, _id: i64) -> Result<(Vec<MemoryEdge>, Vec<MemoryEdge>)> {
        Ok((vec![], vec![]))
    }

    fn backend_kind(&self) -> &'static str {
        "remote"
    }
}
