use anyhow::{Context, Result};
use async_trait::async_trait;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use std::collections::HashSet;

use super::backend::{MemoryBackend, NoteInput};
use super::memory::{MemoryEdge, Note};
use crate::embeddings::blob_to_vec;

mod sync;
mod wire_types;
pub use sync::{BatchItemResult, BatchPushItem, BatchPushResult, CloudSyncClient, RemoteEntry};
pub use wire_types::ConflictInfo;
use wire_types::*;

/// Characters that must be percent-encoded inside a single URL **path segment**.
///
/// `derive_project_id` produces slugs that contain `/` (`local/<blake3-hex>`,
/// `github.com/owner/repo`). Inserted raw into `/v1/projects/{project_id}/…`
/// the slashes split the segment and break axum routing (→ 404). We percent-encode
/// the slug so the whole slug occupies exactly one captured `{project_id}` segment;
/// axum percent-decodes it back to the original slug server-side, so the
/// persistence key (`projects.slug`, UNIQUE) is unchanged. See spelunk decision #106.
///
/// Mirrors `PROJECT_ID_SEGMENT` / `encode_project_id` in
/// `spelunk-cli/src/server_client.rs` — duplicated here because spelunk-core
/// cannot depend on spelunk-cli.
const PROJECT_ID_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/')
    .add(b'%');

/// Percent-encode a `project_id` slug for safe use as a single URL path segment.
///
/// Only the segment is encoded (not the surrounding URL); `/` → `%2F` etc.
pub(super) fn encode_project_id(project_id: &str) -> String {
    utf8_percent_encode(project_id, PROJECT_ID_SEGMENT).to_string()
}

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
        // Percent-encode the project_id path segment: slugs contain `/`
        // (`local/<hex>`, `github.com/owner/repo`) which would otherwise split
        // the segment and break axum routing → 404. See spelunk decision #106.
        format!(
            "{}/v1/projects/{}/{}",
            self.base_url.trim_end_matches('/'),
            encode_project_id(&self.project_id),
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

// ── Trait implementation ──────────────────────────────────────────────────────

#[async_trait]
impl MemoryBackend for RemoteMemoryBackend {
    async fn add(&self, input: NoteInput) -> Result<(i64, bool)> {
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
            // server.db doesn't enforce this amendment's promoted index, so
            // there is nothing for this backend to detect as a reuse.
            return Ok((resp.id, true));
        }

        let resp = http_resp
            .error_for_status()
            .context("server returned error for POST /memory")?
            .json::<AddNoteResponse>()
            .await
            .context("parsing POST /memory response")?;
        // Server-minted cross-machine id (ADR-059 D2). No local store to persist
        // into on this backend; surface it for diagnostics.
        if let Some(remote_id) = &resp.remote_id {
            tracing::debug!(remote_id, "server assigned remote_id for new memory entry");
        }
        Ok((resp.id, true))
    }

    /// Remote backend: timeline search falls back to regular semantic search.
    async fn search_timeline(
        &self,
        query_blob: &[u8],
        query: &str,
        limit: usize,
    ) -> Result<Vec<Note>> {
        self.search(query_blob, query, limit, None).await
    }

    /// The server has no native embedder client-side hook — it embeds `query`
    /// server-side (see `spelunk-server::handlers::search_notes`). The
    /// pre-computed `query_blob` is what local backends use for KNN; the
    /// remote backend ignores it and sends the raw query text instead, or the
    /// server's required `query: String` field is missing and axum rejects
    /// the request with 422 before the handler ever runs (spelunk#359).
    async fn search(
        &self,
        _query_blob: &[u8],
        query: &str,
        limit: usize,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        let body = SearchRequest {
            query: query.to_string(),
            limit,
        };
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
        query: &str,
        limit: usize,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        self.search(query_blob, query, limit, as_of).await
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

#[cfg(test)]
mod tests;
