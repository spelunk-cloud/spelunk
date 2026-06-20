use anyhow::{Context, Result};
use async_trait::async_trait;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use std::collections::HashSet;
use std::path::Path;

use super::backend::{MemoryBackend, NoteInput};
use super::memory::{MemoryEdge, Note};
use crate::embeddings::blob_to_vec;

mod wire_types;
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
fn encode_project_id(project_id: &str) -> String {
    utf8_percent_encode(project_id, PROJECT_ID_SEGMENT).to_string()
}

/// HTTP client for the spelunk-server REST API.
///
/// All routes are scoped under `/v1/projects/{project_id}/`.
///
/// When `slug_to_resolve` is `Some`, the first network call will transparently
/// resolve the human slug to a UUID via `GET /v1/projects` (ADR-005). The
/// resolved UUID is cached in-process via `resolved_uuid` so resolution only
/// happens once per `RemoteMemoryBackend` instance.
pub struct RemoteMemoryBackend {
    pub client: reqwest::Client,
    pub base_url: String,
    /// The raw project identifier as supplied by the user. May be a UUID or a
    /// human slug. When it is a slug (and the server is not loopback), the
    /// first call to `effective_project_id()` resolves it to a UUID.
    pub project_id: String,
    pub api_key: Option<String>,
    /// When `project_id` is a slug (non-UUID) and `base_url` is non-loopback,
    /// this holds the resolved UUID after the first network call. Populated
    /// lazily by `effective_project_id()`.
    resolved_uuid: std::sync::Arc<tokio::sync::OnceCell<String>>,
    /// `.spelunk/` directory for cache read/write. `None` skips caching.
    spelunk_dir: Option<std::path::PathBuf>,
}

impl RemoteMemoryBackend {
    /// Construct a backend. Use this instead of struct literal when you want
    /// slug→UUID resolution (ADR-005).
    ///
    /// Pass `spelunk_dir` (the `.spelunk/` directory of the project) to enable
    /// the slug-resolution cache. If the configured `project_id` is already a
    /// UUID, no network call is ever made regardless of other parameters.
    pub fn new(
        client: reqwest::Client,
        base_url: String,
        project_id: String,
        api_key: Option<String>,
        spelunk_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            client,
            base_url,
            project_id,
            api_key,
            resolved_uuid: std::sync::Arc::new(tokio::sync::OnceCell::new()),
            spelunk_dir,
        }
    }

    /// Return the project id to use in URLs.
    ///
    /// - If `project_id` is already a UUID → return it directly (zero-cost, ADR-005 D5).
    /// - If the server is loopback → return `project_id` as-is (D6).
    /// - Otherwise → resolve the slug to a UUID via `GET /v1/projects`, caching
    ///   the result in `resolved_uuid` (D2, D3, D4).
    async fn effective_project_id(&self) -> Result<String> {
        // Fast path: already a UUID
        if crate::config::looks_like_uuid(&self.project_id) {
            return Ok(self.project_id.clone());
        }
        // Loopback guard: loopback servers accept arbitrary slugs
        if crate::config::is_loopback_url(&self.base_url) {
            return Ok(self.project_id.clone());
        }
        // Lazy resolve: only hit the network once per instance.
        // Clone fields to avoid lifetime issues with the async closure borrowing &self.
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let slug = self.project_id.clone();
        let spelunk_dir = self.spelunk_dir.clone();
        let uuid = self
            .resolved_uuid
            .get_or_try_init(|| async move {
                resolve_cloud_project_uuid(
                    &client,
                    &base_url,
                    api_key.as_deref(),
                    &slug,
                    spelunk_dir.as_deref(),
                )
                .await
            })
            .await?;
        Ok(uuid.clone())
    }

    async fn url(&self, path: &str) -> Result<String> {
        // Percent-encode the project_id path segment: slugs contain `/`
        // (`local/<hex>`, `github.com/owner/repo`) which would otherwise split
        // the segment and break axum routing → 404. See spelunk decision #106.
        let pid = self.effective_project_id().await?;
        Ok(format!(
            "{}/v1/projects/{}/{}",
            self.base_url.trim_end_matches('/'),
            encode_project_id(&pid),
            path
        ))
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(key) = &self.api_key {
            req.header("Authorization", format!("Bearer {key}"))
        } else {
            req
        }
    }
}

// ── Slug→UUID resolution (ADR-005) ───────────────────────────────────────────

/// Cache file: `.spelunk/cloud-project-id.lock`
///
/// Format (TOML):
/// ```toml
/// # Auto-generated by spelunk. Do not edit. Safe to delete — will be regenerated.
/// slug = "spelunk"
/// uuid = "018f4e2a-1234-7abc-8def-000000000001"
/// ```
fn cache_path(spelunk_dir: &Path) -> std::path::PathBuf {
    spelunk_dir.join("cloud-project-id.lock")
}

/// Try to read a cached UUID from `.spelunk/cloud-project-id.lock`.
///
/// Returns `Some(uuid_string)` only when the cached `slug` matches `current_slug`.
/// If the slug changed (user updated config), the stale cache is discarded.
fn read_cache(spelunk_dir: &Path, current_slug: &str) -> Option<String> {
    let path = cache_path(spelunk_dir);
    let content = std::fs::read_to_string(&path).ok()?;
    let table: toml::Table = toml::from_str(&content).ok()?;
    let cached_slug = table.get("slug")?.as_str()?;
    if cached_slug != current_slug {
        return None; // slug changed — discard
    }
    Some(table.get("uuid")?.as_str()?.to_string())
}

/// Write the resolved UUID to `.spelunk/cloud-project-id.lock`.
fn write_cache(spelunk_dir: &Path, slug: &str, uuid: &str) {
    let path = cache_path(spelunk_dir);
    let content = format!(
        "# Auto-generated by spelunk. Do not edit. Safe to delete — will be regenerated.\nslug = \"{slug}\"\nuuid = \"{uuid}\"\n"
    );
    // Non-fatal: log and continue if the write fails (e.g. read-only fs).
    if let Err(e) = std::fs::write(&path, content) {
        tracing::warn!("could not write slug cache to {}: {e}", path.display());
    }
}

/// Resolve a human slug to its UUID via `GET /v1/projects`.
///
/// Call this when `project_id` in config is **not** already a UUID
/// and `server_url` is **not** a loopback address (loopback servers accept
/// arbitrary slugs directly — see ADR-005, D6).
///
/// Cache behaviour (D4):
/// - Read `.spelunk/cloud-project-id.lock`; if `slug` matches, return cached UUID.
/// - `SPELUNK_NO_SLUG_CACHE=1` bypasses the cache and always resolves fresh.
/// - On success, write the cache.
///
/// The `spelunk_dir` parameter is the `.spelunk/` directory in the project root
/// (same directory as `.spelunk/config.toml`). Pass `None` to skip caching.
pub async fn resolve_cloud_project_uuid(
    client: &reqwest::Client,
    server_url: &str,
    api_key: Option<&str>,
    slug: &str,
    spelunk_dir: Option<&Path>,
) -> Result<String> {
    // Cache bypass env var
    let no_cache = std::env::var("SPELUNK_NO_SLUG_CACHE")
        .map(|v| v == "1")
        .unwrap_or(false);

    // Try cache
    if !no_cache {
        if let Some(dir) = spelunk_dir {
            if let Some(cached_uuid) = read_cache(dir, slug) {
                tracing::debug!("slug cache hit: {slug} → {cached_uuid}");
                return Ok(cached_uuid);
            }
        }
    }

    // Fetch project list
    let url = format!("{}/v1/projects", server_url.trim_end_matches('/'));
    let mut req = client.get(&url);
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("server returned error for GET {url}"))?
        .json::<CloudProjectListResponse>()
        .await
        .context("parsing GET /v1/projects response")?;

    // Find matching slug
    let matched = resp.projects.into_iter().find(|p| {
        p.slug.as_deref() == Some(slug)
    });

    let item = matched.ok_or_else(|| {
        // Extract the host for the error message
        let host = server_url
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .split('/')
            .next()
            .unwrap_or(server_url);
        anyhow::anyhow!(
            "project slug \"{slug}\" not found on {host}.\n       \
             Run 'spelunk projects list' or check .spelunk/config.toml."
        )
    })?;

    let uuid_str = item.id;

    // Write cache
    if !no_cache {
        if let Some(dir) = spelunk_dir {
            write_cache(dir, slug, &uuid_str);
        }
    }

    Ok(uuid_str)
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
            .authed(self.client.post(self.url("memory").await?))
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
            .authed(self.client.post(self.url("memory/search").await?))
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
        let mut req = self.client.get(self.url("memory").await?).query(&[
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
            .authed(self.client.get(self.url(&format!("memory/{id}")).await?))
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
            .authed(self.client.get(self.url("stats").await?))
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
            .authed(self.client.post(self.url(&format!("memory/{id}/archive")).await?))
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
                    .post(self.url(&format!("memory/{old_id}/supersede")).await?),
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
        let req = self.client.get(self.url("memory").await?).query(&[
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
            .authed(self.client.get(self.url("memory/harvested-shas").await?))
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
