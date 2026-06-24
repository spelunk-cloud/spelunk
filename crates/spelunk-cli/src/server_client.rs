//! Thin HTTP clients for spelunk-server inference endpoints.
//!
//! `ServerLlmClient`  — calls `POST /v1/projects/{id}/llm/complete` (SSE).
//! `ServerEmbedClient`— calls `POST /v1/projects/{id}/index/embed`  (JSON).
//!
//! These are the ONLY places in spelunk-cli that call AI inference routes.
//! All prompt orchestration remains CLI-side; the server is a raw-inference peer.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Characters that must be percent-encoded inside a single URL **path segment**.
///
/// `derive_project_id` produces slugs that contain `/` (`local/<blake3-hex>`,
/// `github.com/owner/repo`). Inserted raw into `/v1/projects/{project_id}/…`
/// the slashes split the segment and break axum routing (→ 404). We percent-encode
/// the slug so the whole slug occupies exactly one captured `{project_id}` segment;
/// axum percent-decodes it back to the original slug server-side, so the
/// persistence key (`projects.slug`, UNIQUE) is unchanged. See spelunk decision #106.
///
/// Set mirrors the WHATWG URL "path" percent-encode set plus the sub-delimiters
/// that would otherwise be interpreted by a router; crucially it includes `/`.
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
pub(crate) fn encode_project_id(project_id: &str) -> String {
    utf8_percent_encode(project_id, PROJECT_ID_SEGMENT).to_string()
}

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct LlmMsg<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct LlmCompleteReq<'a> {
    messages: Vec<LlmMsg<'a>>,
    max_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    json_schema: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct EmbedChunkIn<'a> {
    chunk_id: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct EmbedReq<'a> {
    chunks: Vec<EmbedChunkIn<'a>>,
}

#[derive(Deserialize)]
struct EmbedChunkOut {
    chunk_id: String,
    vector: Vec<f32>,
}

#[derive(Deserialize)]
struct EmbedResp {
    chunks: Vec<EmbedChunkOut>,
}

// ── Public message type (mirrors spelunk_core::llm::Message) ─────────────────

pub struct LlmMessage {
    pub role: String,
    pub content: String,
}

impl LlmMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
}

// ── ServerInferenceClient ─────────────────────────────────────────────────────

/// HTTP client for spelunk-server's inference endpoints.
///
/// Constructed from config when `server_url` is set (Tier 1). Returns `None`
/// in Tier-0 mode so callers can emit the standard locked-feature error.
pub struct ServerInferenceClient {
    client: reqwest::Client,
    base_url: String,
    project_id: String,
    api_key: Option<String>,
}

impl ServerInferenceClient {
    /// Build from config. Returns `None` when `server_url` is not configured.
    pub fn from_config(cfg: &Config) -> Option<Self> {
        let base_url = cfg.server_url.as_deref()?.trim_end_matches('/').to_string();
        let project_id = cfg.project_id.clone().unwrap_or_default();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("building HTTP client for server inference");
        Some(Self {
            client,
            base_url,
            project_id,
            api_key: cfg.server_key.clone(),
        })
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(key) = &self.api_key {
            req.header("Authorization", format!("Bearer {key}"))
        } else {
            req
        }
    }

    fn llm_url(&self) -> String {
        format!(
            "{}/v1/projects/{}/llm/complete",
            self.base_url,
            encode_project_id(&self.project_id)
        )
    }

    fn embed_url(&self) -> String {
        format!(
            "{}/v1/projects/{}/index/embed",
            self.base_url,
            encode_project_id(&self.project_id)
        )
    }

    fn search_url(&self) -> String {
        format!(
            "{}/v1/projects/{}/search",
            self.base_url,
            encode_project_id(&self.project_id)
        )
    }

    /// Call `/llm/complete` and collect the full SSE token stream into a `String`.
    ///
    /// Returns the concatenated completion text (all `token` events joined).
    /// Returns an error if the server returns a non-2xx status or the stream
    /// contains a terminal `error` event.
    pub async fn llm_complete(
        &self,
        messages: &[LlmMessage],
        max_tokens: usize,
        json_schema: Option<serde_json::Value>,
    ) -> Result<String> {
        use futures_util::StreamExt;

        let body = LlmCompleteReq {
            messages: messages
                .iter()
                .map(|m| LlmMsg {
                    role: &m.role,
                    content: &m.content,
                })
                .collect(),
            max_tokens,
            json_schema,
        };

        let resp = self
            .authed(self.client.post(self.llm_url()))
            .json(&body)
            .send()
            .await
            .context("POST /llm/complete")?
            .error_for_status()
            .context("spelunk-server returned an error for /llm/complete")?;

        let mut stream = resp.bytes_stream();
        let mut sse_buf = String::new();
        let mut output = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.context("reading /llm/complete SSE stream")?;
            sse_buf.push_str(&String::from_utf8_lossy(&bytes));

            // Consume complete SSE events (terminated by "\n\n").
            while let Some(pos) = sse_buf.find("\n\n") {
                let event = sse_buf[..pos].to_string();
                sse_buf.drain(..pos + 2);

                for line in event.lines() {
                    let data = match line.strip_prefix("data: ") {
                        Some(d) => d,
                        None => continue,
                    };
                    if data.is_empty() {
                        continue;
                    }
                    let Ok(val) = serde_json::from_str::<serde_json::Value>(data) else {
                        continue;
                    };
                    match val.get("kind").and_then(|k| k.as_str()) {
                        Some("token") => {
                            if let Some(content) = val.get("content").and_then(|c| c.as_str()) {
                                output.push_str(content);
                            }
                        }
                        Some("done") => return Ok(output),
                        Some("error") => {
                            let msg = val
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("unknown error");
                            anyhow::bail!("llm/complete stream error: {msg}");
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(output)
    }

    /// Call `/index/embed` with a synthetic `chunk_id` and return the vector.
    ///
    /// The `chunk_id` is prefixed `query:` per ADR-002 so it is trivially
    /// distinguishable from real chunk ids in server logs.
    pub async fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        let chunk_id = format!("query:{}", uuid_v4_hex());
        let body = EmbedReq {
            chunks: vec![EmbedChunkIn {
                chunk_id: &chunk_id,
                content: text,
            }],
        };

        let resp: EmbedResp = self
            .authed(self.client.post(self.embed_url()))
            .json(&body)
            .send()
            .await
            .context("POST /index/embed (query vector)")?
            .error_for_status()
            .context("spelunk-server returned an error for /index/embed")?
            .json()
            .await
            .context("parsing /index/embed response")?;

        resp.chunks
            .into_iter()
            .find(|c| c.chunk_id == chunk_id)
            .map(|c| c.vector)
            .ok_or_else(|| anyhow::anyhow!("embed response missing chunk_id {chunk_id}"))
    }

    /// Call `POST /v1/projects/{id}/search` to embed a query server-side and
    /// return the query vector for CLI-side KNN.
    ///
    /// The server applies the F2LLM code-retrieval prefix before embedding, so
    /// the caller does not need to know the format.
    ///
    /// Returns `None` when the server responds with `mode: "text"` (no embedding
    /// needed; caller should use FTS instead).
    pub async fn search_query(
        &self,
        query: &str,
        mode: &str,
        limit: usize,
    ) -> Result<Option<Vec<f32>>> {
        #[derive(serde::Serialize)]
        struct Req<'a> {
            query: &'a str,
            limit: usize,
            mode: &'a str,
        }
        #[derive(serde::Deserialize)]
        struct Resp {
            query_vector: Option<Vec<f32>>,
            #[allow(dead_code)]
            mode: String,
        }

        let resp: Resp = self
            .authed(self.client.post(self.search_url()))
            .json(&Req { query, limit, mode })
            .send()
            .await
            .context("POST /search (query vector)")?
            .error_for_status()
            .context("spelunk-server returned an error for /search")?
            .json()
            .await
            .context("parsing /search response")?;

        Ok(resp.query_vector)
    }
}

// ── UUID helper (no dep needed) ───────────────────────────────────────────────

fn uuid_v4_hex() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Cheap pseudo-unique id using time + process id; good enough for a chunk_id prefix.
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    format!("{t:x}{pid:x}")
}

// ── EmbeddingBackend adapter ──────────────────────────────────────────────────

/// Wraps `ServerInferenceClient` and implements the spelunk-core `EmbeddingBackend`
/// trait so that explorer / memory code can embed via spelunk-server without
/// touching `ActiveEmbedder` / reqwest directly.
pub struct ServerEmbedAdapter(pub Arc<ServerInferenceClient>);

#[async_trait]
impl spelunk_core::embeddings::EmbeddingBackend for ServerEmbedAdapter {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            results.push(self.0.embed_text(text).await?);
        }
        Ok(results)
    }

    fn dimension(&self) -> usize {
        spelunk_core::embeddings::EMBEDDING_DIM
    }
}

// ── LlmBackend adapter ────────────────────────────────────────────────────────

/// Wraps `ServerInferenceClient` and implements the spelunk-core `LlmBackend`
/// trait so that explorer / summariser code can call the LLM via spelunk-server.
pub struct ServerLlmAdapter(pub Arc<ServerInferenceClient>);

#[async_trait]
impl spelunk_core::llm::LlmBackend for ServerLlmAdapter {
    async fn generate(
        &self,
        messages: &[spelunk_core::llm::Message],
        max_tokens: usize,
        tx: tokio::sync::mpsc::Sender<spelunk_core::llm::Token>,
        json_schema: Option<serde_json::Value>,
    ) -> Result<()> {
        let server_msgs: Vec<LlmMessage> = messages
            .iter()
            .map(|m| LlmMessage {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();
        let text = self
            .0
            .llm_complete(&server_msgs, max_tokens, json_schema)
            .await?;
        // Send the entire response as a single token (server already collected
        // the SSE stream and returned the completed string).
        let _ = tx.send(text).await;
        Ok(())
    }
}

// ── Tier-0 error helper ───────────────────────────────────────────────────────

/// Return the standard locked-feature error when harvest is attempted without a server.
pub fn harvest_requires_server(server_url: Option<&str>) -> anyhow::Error {
    let tried = server_url
        .map(|u| format!("\n       (Tried: {u} — unreachable)"))
        .unwrap_or_default();
    anyhow::anyhow!(
        "'spelunk memory harvest' requires spelunk-server.\n\
         Set server_url in ~/.config/spelunk/config.toml to enable this feature.{tried}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `derive_local_fallback` produces `local/<blake3-hex>` slugs — the `/`
    /// must become `%2F` so the whole slug occupies one URL path segment
    /// (IMP-1 / spelunk decision #106).
    #[test]
    fn encode_project_id_escapes_local_fallback_slug() {
        let slug = "local/9f2a8b3c4d5e6f70";
        let encoded = encode_project_id(slug);
        assert_eq!(encoded, "local%2F9f2a8b3c4d5e6f70");
    }

    /// `normalise_git_url` produces `github.com/owner/repo` slugs — both `/`
    /// must be escaped so axum routes the whole slug into `{project_id}`.
    #[test]
    fn encode_project_id_escapes_github_remote_slug() {
        let slug = "github.com/BurntSushi/jiff";
        let encoded = encode_project_id(slug);
        assert_eq!(encoded, "github.com%2FBurntSushi%2Fjiff");
    }

    /// Round-trip: percent-decoding the encoded segment must yield the
    /// original slug unchanged, since the slug is the persistence key
    /// (`projects.slug` UNIQUE) and must reach `require_project`/
    /// `upsert_project` exactly as `derive_project_id` produced it.
    #[test]
    fn encode_project_id_round_trips_through_percent_decode() {
        for slug in ["local/9f2a8b3c4d5e6f70", "github.com/BurntSushi/jiff"] {
            let encoded = encode_project_id(slug);
            let decoded = percent_encoding::percent_decode_str(&encoded)
                .decode_utf8()
                .expect("valid UTF-8 after percent-decoding");
            assert_eq!(decoded, slug, "round-trip mismatch for slug {slug:?}");
        }
    }

    /// A slug with no special characters should be left byte-for-byte
    /// identical (no spurious encoding of ordinary path-safe characters).
    #[test]
    fn encode_project_id_leaves_simple_slug_unchanged() {
        assert_eq!(encode_project_id("my-project"), "my-project");
    }
}
