//! `MemoryBackend` over the hosted cloud API's memory routes.
//!
//! Kept a separate type from [`RemoteMemoryBackend`](super::RemoteMemoryBackend)
//! rather than a set of `if peer == cloud` branches inside it: the two peers
//! disagree about routes, verbs and entry shape, and the self-hosted dialect
//! staying a distinct type is what makes "self-hosted `cloud_first` is
//! unchanged" true by construction instead of by care.
//!
//! Every route used here ships on the cloud API today: `get`/`archive` (and
//! `supersede`, which reads both entries first) originally required a
//! project UUID rather than a slug on their two per-entry routes, a
//! constraint the hosted API has since lifted so the project segment now
//! behaves identically to every other memory route (see ADR-005's second
//! amendment).

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::HashSet;

use super::super::backend::{MemoryBackend, NoteInput};
use super::super::memory::{MemoryEdge, Note, NoteId};
use super::encode_project_id;
use wire::*;

mod wire;

/// Entries fetched per page when a filter has to be applied client-side.
const PAGE_SIZE: usize = 200;

/// Ceiling on client-side filtering passes, so a server that ignores `offset`
/// cannot spin this loop forever.
const MAX_PAGES: usize = 500;

pub struct CloudApiMemoryBackend {
    pub client: reqwest::Client,
    pub base_url: String,
    pub project_id: String,
    pub api_key: Option<String>,
}

impl CloudApiMemoryBackend {
    fn url(&self, path: &str) -> String {
        format!(
            "{}/v1/projects/{}/{}",
            self.base_url.trim_end_matches('/'),
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

    /// One page of `GET /memory`, optionally narrowed by a search query.
    async fn page(
        &self,
        query: Option<&str>,
        limit: usize,
        offset: usize,
        include_archived: bool,
    ) -> Result<EntryListResponse> {
        let mut req = self
            .client
            .get(self.url("memory"))
            .query(&[("limit", limit.to_string()), ("offset", offset.to_string())]);
        if let Some(q) = query {
            req = req.query(&[("q", q)]);
        }
        if include_archived {
            req = req.query(&[("archived", "true")]);
        }
        self.authed(req)
            .send()
            .await
            .context("GET /memory")?
            .error_for_status()
            .context("server returned error for GET /memory")?
            .json::<EntryListResponse>()
            .await
            .context("parsing GET /memory response")
    }

    /// Page through the project and keep entries whose `source_commit` starts
    /// with `prefix`.
    ///
    /// The cloud API exposes no server-side `source_commit` filter, so this is
    /// O(entries in project). Correct, but the one place this dialect costs
    /// real efficiency against the team server's indexed lookup.
    async fn filter_by_source_commit(
        &self,
        prefix: &str,
        limit: usize,
        include_archived: bool,
    ) -> Result<Vec<Note>> {
        let mut out = Vec::new();
        for page in 0..MAX_PAGES {
            let resp = self
                .page(None, PAGE_SIZE, page * PAGE_SIZE, include_archived)
                .await?;
            let drained = resp.entries.len();
            for entry in resp.entries {
                if entry
                    .source_commit
                    .as_deref()
                    .is_some_and(|sha| sha.starts_with(prefix))
                {
                    out.push(entry.into_note());
                    if out.len() >= limit {
                        return Ok(out);
                    }
                }
            }
            if drained < PAGE_SIZE {
                break;
            }
        }
        Ok(out)
    }

    /// Read an entry's `external_id`, the only key the batch edge route
    /// accepts.
    async fn external_id_of(&self, id: &NoteId, role: &str) -> Result<String> {
        let entry = self
            .fetch(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("No memory entry with id {id} ({role})."))?;
        entry.external_id.ok_or_else(|| {
            anyhow::anyhow!(
                "memory entry {id} ({role}) has no external_id, so a supersede edge \
                 cannot be addressed to it. Entries created by older clients predate \
                 this key; re-create the entry to supersede it."
            )
        })
    }

    async fn fetch(&self, id: &NoteId) -> Result<Option<EntryResponse>> {
        let resp = self
            .authed(
                self.client
                    .get(self.url(&format!("memory/{}", super::encode_path_segment(id)))),
            )
            .send()
            .await
            .context("GET /memory/{id}")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(
            resp.error_for_status()
                .context("server returned error for GET /memory/{id}")?
                .json::<EntryResponse>()
                .await
                .context("parsing GET /memory/{id} response")?,
        ))
    }
}

#[async_trait]
impl MemoryBackend for CloudApiMemoryBackend {
    /// `external_id` is minted on every add, not only when a supersede is
    /// anticipated: it is the sole key the batch edge route accepts, and it
    /// cannot be assigned retroactively.
    async fn add(&self, input: NoteInput) -> Result<(NoteId, bool)> {
        let body = CreateEntryBody {
            kind: input.kind,
            title: input.title,
            body: Some(input.body),
            external_id: uuid::Uuid::new_v4().to_string(),
            source_commit: input.source_ref,
        };
        let resp = self
            .authed(self.client.post(self.url("memory")))
            .json(&body)
            .send()
            .await
            .context("POST /memory")?;

        // A 409 means "stored, but semantically close to an existing entry":
        // a warning on every other backend, so a warning here too.
        if resp.status() == reqwest::StatusCode::CONFLICT {
            let created = resp
                .json::<EntryResponse>()
                .await
                .context("parsing POST /memory 409 response")?;
            eprintln!("warning: memory entry conflicts with existing entries.");
            return Ok((created.id, true));
        }

        let created = resp
            .error_for_status()
            .context("server returned error for POST /memory")?
            .json::<EntryResponse>()
            .await
            .context("parsing POST /memory response")?;
        Ok((created.id, true))
    }

    async fn search_timeline(
        &self,
        query_blob: &[u8],
        query: &str,
        limit: usize,
    ) -> Result<Vec<Note>> {
        self.search(query_blob, query, limit, None).await
    }

    /// Search and list are the same route, told apart by the presence of `q`;
    /// the server embeds the query.
    async fn search(
        &self,
        _query_blob: &[u8],
        query: &str,
        limit: usize,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        let resp = self.page(Some(query), limit, 0, false).await?;
        Ok(resp
            .entries
            .into_iter()
            .map(EntryResponse::into_note)
            .collect())
    }

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
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        let mut req = self
            .client
            .get(self.url("memory"))
            .query(&[("limit", limit.to_string().as_str()), ("offset", "0")]);
        if let Some(kind) = kind_filter {
            req = req.query(&[("kind", kind)]);
        }
        if include_archived {
            req = req.query(&[("archived", "true")]);
        }
        let resp = self
            .authed(req)
            .send()
            .await
            .context("GET /memory")?
            .error_for_status()
            .context("server returned error for GET /memory")?
            .json::<EntryListResponse>()
            .await
            .context("parsing GET /memory response")?;
        Ok(resp
            .entries
            .into_iter()
            .map(EntryResponse::into_note)
            .collect())
    }

    async fn list_by_source_ref(
        &self,
        source_ref_prefix: &str,
        limit: usize,
        include_archived: bool,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        self.filter_by_source_commit(source_ref_prefix, limit, include_archived)
            .await
    }

    async fn get(&self, id: NoteId) -> Result<Option<Note>> {
        Ok(self.fetch(&id).await?.map(EntryResponse::into_note))
    }

    /// The list route computes the total in the same round trip, so a
    /// single-entry page carries the count without a dedicated stats route.
    async fn count(&self) -> Result<i64> {
        Ok(self.page(None, 1, 0, false).await?.total)
    }

    /// The cloud API archives by `DELETE`; there is no archive sub-route.
    ///
    /// A 404 counts as success, matching `CloudSyncClient::delete_remote`: the
    /// caller asked for the entry to be gone and it is.
    async fn archive(&self, id: NoteId) -> Result<bool> {
        let resp = self
            .authed(
                self.client
                    .delete(self.url(&format!("memory/{}", super::encode_path_segment(&id)))),
            )
            .send()
            .await
            .context("DELETE /memory/{id}")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        resp.error_for_status()
            .context("server returned error for DELETE /memory/{id}")?;
        Ok(true)
    }

    /// Supersession is expressible only as a batch edge keyed by
    /// `external_id`, so both entries are read back for their key before the
    /// edge is posted.
    ///
    /// Both must still be live: an edge naming an already-archived predecessor
    /// comes back unresolved, which is reported as "nothing changed" rather
    /// than as success, matching what the team server returns in that case.
    async fn supersede(&self, old_id: NoteId, new_id: NoteId) -> Result<bool> {
        let from = self.external_id_of(&old_id, "old").await?;
        let to = self.external_id_of(&new_id, "new").await?;

        let body = BatchEdgeBody {
            entries: [],
            edges: vec![BatchEdge {
                from_external_id: from,
                to_external_id: to,
                kind: "supersedes",
            }],
        };
        let resp = self
            .authed(self.client.post(self.url("memory/batch")))
            .json(&body)
            .send()
            .await
            .context("POST /memory/batch")?
            .error_for_status()
            .context("server returned error for POST /memory/batch")?
            .json::<BatchEdgeResult>()
            .await
            .context("parsing POST /memory/batch response")?;

        Ok(resp.edge_applied())
    }

    async fn harvested_shas(&self) -> Result<HashSet<String>> {
        let mut out = HashSet::new();
        for page in 0..MAX_PAGES {
            let resp = self.page(None, PAGE_SIZE, page * PAGE_SIZE, true).await?;
            let drained = resp.entries.len();
            out.extend(resp.entries.into_iter().filter_map(|e| e.source_commit));
            if drained < PAGE_SIZE {
                break;
            }
        }
        Ok(out)
    }

    async fn has_source_ref(&self, sha: &str) -> Result<bool> {
        Ok(!self.filter_by_source_commit(sha, 1, true).await?.is_empty())
    }

    /// Edges are a local-graph-only feature; no remote backend supports them.
    async fn add_edge(&self, _from_id: i64, _to_id: i64, _kind: &str) -> Result<()> {
        Ok(())
    }

    async fn get_edges(&self, _id: i64) -> Result<(Vec<MemoryEdge>, Vec<MemoryEdge>)> {
        Ok((vec![], vec![]))
    }

    fn backend_kind(&self) -> &'static str {
        "cloud-api"
    }
}

#[cfg(test)]
mod tests;
