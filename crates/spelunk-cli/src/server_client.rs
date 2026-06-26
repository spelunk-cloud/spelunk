//! Thin HTTP clients for spelunk-server inference endpoints.
//!
//! `ServerLlmClient`  — calls `POST /v1/projects/{id}/llm/complete` (SSE).
//! `ServerEmbedClient`— calls `POST /v1/projects/{id}/index/embed`  (JSON).
//!
//! These are the ONLY places in spelunk-cli that call AI inference routes.
//! All prompt orchestration remains CLI-side; the server is a raw-inference peer.

use std::sync::Arc;
use std::sync::Mutex;

use anyhow::{Context, Result};
use async_trait::async_trait;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::Serialize;
use uuid::Uuid;

use crate::cli::cmd::auth_api;
use crate::config::Config;
use spelunk_core::config::AuthTokens;

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
    /// Current bearer token + refresh state. `RwLock`-free `Mutex` is fine:
    /// contention is nil (refresh happens at most once per request) and the
    /// critical section is a cheap clone / swap.
    auth: Mutex<BearerState>,
}

/// Mutable bearer state shared across requests so a refreshed token is reused.
struct BearerState {
    /// The token sent as `Authorization: Bearer`. `None` in Tier-0 / unauthed.
    bearer: Option<String>,
    /// WorkOS refresh state, present only when `spelunk login` wrote `[auth]`
    /// tokens. Enables the refresh-on-expiry / refresh-on-401 path; absent for
    /// a bare `server_key` (which cannot be refreshed — the user must re-login).
    refresh: Option<RefreshState>,
}

/// State needed to rotate an expired/rejected WorkOS access token.
///
/// Refresh now goes DIRECTLY to WorkOS (ADR-047), so the WorkOS base URL and the
/// embedded public `client_id` are carried here alongside the rotating tokens.
struct RefreshState {
    tokens: AuthTokens,
    /// WorkOS User Management base URL (`https://api.workos.com` by default).
    workos_url: String,
    /// Embedded WorkOS public `client_id` for the active environment.
    client_id: String,
    /// Where rotated tokens are persisted. `None` ⇒ the global config path
    /// (`~/.config/spelunk/config.toml`); tests inject a temp path.
    config_path: Option<std::path::PathBuf>,
}

impl ServerInferenceClient {
    /// Build from config. Returns `None` when no inference URL is available.
    ///
    /// Uses `Config::resolve_inference_url()` (ADR-004): an auto-discovered
    /// loopback server sets `inference_url` while leaving `server_url` unset, so
    /// inference reaches the server even though memory stays local. An explicit
    /// team `server_url` is used for both.
    pub fn from_config(cfg: &Config) -> Option<Self> {
        let base_url = cfg
            .resolve_inference_url()?
            .trim_end_matches('/')
            .to_string();
        let project_id = cfg.project_id.clone().unwrap_or_default();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("building HTTP client for server inference");

        // Carry WorkOS refresh state only when the bearer comes from `[auth]`
        // (i.e. `server_key` was resolved from the access token). A bare
        // `server_key` / env token is not refreshable here. Refresh targets
        // WorkOS directly (ADR-047): the WorkOS base URL and the embedded public
        // client_id (derived from the default cloud host) are captured here.
        let refresh = cfg
            .auth
            .as_ref()
            .filter(|a| Some(a.access_token.as_str()) == cfg.server_key.as_deref())
            .map(|tokens| RefreshState {
                tokens: tokens.clone(),
                workos_url: auth_api::workos_url(),
                client_id: auth_api::workos_client_id(auth_api::DEFAULT_CLOUD_URL),
                config_path: None,
            });

        Some(Self {
            client,
            base_url,
            project_id,
            auth: Mutex::new(BearerState {
                bearer: cfg.server_key.clone(),
                refresh,
            }),
        })
    }

    /// Test-only constructor wiring an explicit base URL, bearer, and refresh
    /// state (with a temp config path so persistence does not touch the real
    /// `~/.config/spelunk/config.toml`).
    #[cfg(test)]
    fn for_test(
        base_url: &str,
        project_id: &str,
        bearer: Option<String>,
        refresh: Option<(AuthTokens, String, std::path::PathBuf)>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            project_id: project_id.to_string(),
            auth: Mutex::new(BearerState {
                bearer,
                // `workos_url` is the second tuple element (tests point it at a
                // mock WorkOS server); the client_id is a fixed test value.
                refresh: refresh.map(|(tokens, workos_url, config_path)| RefreshState {
                    tokens,
                    workos_url,
                    client_id: "client_test".to_string(),
                    config_path: Some(config_path),
                }),
            }),
        }
    }

    /// Current bearer token, if any.
    fn current_bearer(&self) -> Option<String> {
        self.auth
            .lock()
            .expect("auth mutex poisoned")
            .bearer
            .clone()
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(key) = self.current_bearer() {
            req.header("Authorization", format!("Bearer {key}"))
        } else {
            req
        }
    }

    /// Whether a stored WorkOS access token is at/past expiry (refresh state
    /// present and expired).
    fn access_token_expired(&self) -> bool {
        let guard = self.auth.lock().expect("auth mutex poisoned");
        guard
            .refresh
            .as_ref()
            .is_some_and(|r| r.tokens.is_expired())
    }

    /// Rotate the WorkOS access token DIRECTLY via WorkOS `/authenticate`
    /// (refresh grant, ADR-047), persist the rotated tokens, and update the
    /// in-memory bearer.
    ///
    /// Returns `Ok(true)` when a refresh was performed, `Ok(false)` when there
    /// is no refresh state (a bare `server_key` — nothing to refresh). Errors
    /// carry a clear "re-run `spelunk login`" message.
    async fn refresh_access_token(&self) -> Result<bool> {
        let (refresh_token, workos_url, client_id, config_path) = {
            let guard = self.auth.lock().expect("auth mutex poisoned");
            match &guard.refresh {
                Some(r) => (
                    r.tokens.refresh_token.clone(),
                    r.workos_url.clone(),
                    r.client_id.clone(),
                    r.config_path.clone(),
                ),
                None => return Ok(false),
            }
        };

        let rotated =
            auth_api::refresh_token(&self.client, &workos_url, &client_id, &refresh_token, None)
                .await
                .map_err(|e| {
                    e.context("session expired and token refresh failed — re-run `spelunk login`")
                })?;
        let new_tokens = rotated.into_auth_tokens();

        // Persist rotated tokens so the next process starts authenticated.
        match &config_path {
            Some(p) => spelunk_core::config::save_auth_tokens_to(&new_tokens, p),
            None => spelunk_core::config::save_auth_tokens(&new_tokens),
        }
        .context("persisting refreshed auth tokens")?;

        let mut guard = self.auth.lock().expect("auth mutex poisoned");
        guard.bearer = Some(new_tokens.access_token.clone());
        guard.refresh = Some(RefreshState {
            tokens: new_tokens,
            workos_url,
            client_id,
            config_path,
        });
        Ok(true)
    }

    /// Send a request with WorkOS token management:
    ///   1. If the stored access token is locally expired, refresh first.
    ///   2. Send the request (built fresh by `build` so it can be retried).
    ///   3. On a `401`, refresh once and retry the request a single time.
    ///
    /// `build` is given the base (unauthed) `RequestBuilder` for the URL and
    /// should attach the body; the bearer header is added here so each attempt
    /// uses the current token. One refresh attempt only; on refresh failure the
    /// original error / a clear re-login message is surfaced.
    async fn send_authed(
        &self,
        make_req: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        // Proactive refresh: avoid a guaranteed-401 round-trip when we already
        // know the token is past expiry.
        if self.access_token_expired() {
            self.refresh_access_token().await?;
        }

        let resp = self.authed(make_req()).send().await?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }

        // Reactive refresh on 401, then retry exactly once.
        if self.refresh_access_token().await? {
            return Ok(self.authed(make_req()).send().await?);
        }
        Ok(resp)
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

        let url = self.llm_url();
        let resp = self
            .send_authed(|| self.client.post(&url).json(&body))
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
        let chunk_id = format!("query:{}", Uuid::now_v7());
        let body = EmbedReq {
            chunks: vec![EmbedChunkIn {
                chunk_id: &chunk_id,
                content: text,
            }],
        };

        // Response is raw little-endian f32 bytes (one vector, `dim` floats).
        let url = self.embed_url();
        let bytes = self
            .send_authed(|| self.client.post(&url).json(&body))
            .await
            .context("POST /index/embed (query vector)")?
            .error_for_status()
            .context("spelunk-server returned an error for /index/embed")?
            .bytes()
            .await
            .context("reading /index/embed response")?;

        let expected = spelunk_core::embeddings::EMBEDDING_DIM * 4;
        anyhow::ensure!(
            bytes.len() == expected,
            "embed response is {} bytes, expected {expected} (one {}-dim f32 vector)",
            bytes.len(),
            spelunk_core::embeddings::EMBEDDING_DIM,
        );
        Ok(spelunk_core::embeddings::blob_to_vec(&bytes))
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

        let url = self.search_url();
        let req_body = Req { query, limit, mode };
        let resp: Resp = self
            .send_authed(|| self.client.post(&url).json(&req_body))
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
    use spelunk_core::config::AuthTokens;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn expiring_tokens(expires_at: i64) -> AuthTokens {
        AuthTokens {
            access_token: "at-old".to_string(),
            refresh_token: "rt-old".to_string(),
            expires_at,
            org_id: "org_1".to_string(),
        }
    }

    /// Build an unsigned JWT carrying `exp` and `org_id` so the refresh path's
    /// claim decode resolves the rotated session's expiry and org. The token
    /// string itself doubles as the bearer the retry must send.
    fn jwt(label: &str, org_id: &str, exp: i64) -> String {
        fn b64url(bytes: &[u8]) -> String {
            const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
                out.push(A[((n >> 18) & 0x3f) as usize] as char);
                out.push(A[((n >> 12) & 0x3f) as usize] as char);
                if chunk.len() > 1 {
                    out.push(A[((n >> 6) & 0x3f) as usize] as char);
                }
                if chunk.len() > 2 {
                    out.push(A[(n & 0x3f) as usize] as char);
                }
            }
            out
        }
        // `label` keeps distinct test JWTs textually distinguishable.
        let payload = serde_json::json!({ "exp": exp, "org_id": org_id, "lbl": label }).to_string();
        format!("{}.{}.sig", b64url(b"{}"), b64url(payload.as_bytes()))
    }

    /// A 401 from the inference server triggers exactly one refresh + retry; the
    /// rotated access token is used on the retry and persisted to disk.
    #[tokio::test]
    async fn refresh_on_401_retries_once_and_persists() {
        let inference = MockServer::start().await;
        let cloud = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        let at_new = jwt("new", "org_1", 5_000_000_000);

        // First /search call (with stale bearer) → 401.
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/search"))
            .and(header("authorization", "Bearer at-old"))
            .respond_with(ResponseTemplate::new(401))
            .up_to_n_times(1)
            .mount(&inference)
            .await;

        // The WorkOS refresh exchange rotates the tokens (ADR-047).
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": at_new,
                "refresh_token": "rt-new",
                "organization_id": "org_1",
            })))
            .expect(1)
            .mount(&cloud)
            .await;

        // Retry (with the rotated bearer = the new JWT) → 200.
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/search"))
            .and(header("authorization", format!("Bearer {at_new}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_vector": [0.1_f32, 0.2, 0.3],
                "mode": "semantic",
            })))
            .expect(1)
            .mount(&inference)
            .await;

        let token = expiring_tokens(5_000_000_000); // not locally expired
        let client = ServerInferenceClient::for_test(
            &inference.uri(),
            "proj",
            Some("at-old".to_string()),
            Some((token, cloud.uri(), config_path.clone())),
        );

        let vec = client
            .search_query("hello", "semantic", 5)
            .await
            .expect("search should succeed after one refresh+retry");
        assert_eq!(vec, Some(vec![0.1_f32, 0.2, 0.3]));

        // Rotated tokens were persisted to the injected config path.
        let cfg = spelunk_core::config::Config::load(Some(&config_path)).unwrap();
        assert_eq!(cfg.server_key.as_deref(), Some(at_new.as_str()));
        assert_eq!(cfg.auth.unwrap().refresh_token, "rt-new");
    }

    /// A locally-expired access token is refreshed proactively before the first
    /// send, so the stale token never reaches the server.
    #[tokio::test]
    async fn proactive_refresh_when_locally_expired() {
        let inference = MockServer::start().await;
        let cloud = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        let at_fresh = jwt("fresh", "org_1", 5_000_000_000);

        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": at_fresh,
                "refresh_token": "rt-fresh",
                "organization_id": "org_1",
            })))
            .expect(1)
            .mount(&cloud)
            .await;

        // Only the fresh bearer is ever accepted; the stale one must not appear.
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/search"))
            .and(header(
                "authorization",
                format!("Bearer {at_fresh}").as_str(),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_vector": [1.0_f32],
                "mode": "semantic",
            })))
            .expect(1)
            .mount(&inference)
            .await;

        // expires_at = 0 ⇒ definitely past expiry.
        let token = expiring_tokens(0);
        let client = ServerInferenceClient::for_test(
            &inference.uri(),
            "proj",
            Some("at-old".to_string()),
            Some((token, cloud.uri(), config_path)),
        );

        let vec = client.search_query("q", "semantic", 1).await.unwrap();
        assert_eq!(vec, Some(vec![1.0_f32]));
    }

    /// With no refresh state (bare `server_key`), a 401 is surfaced as-is — no
    /// refresh attempt, no retry.
    #[tokio::test]
    async fn no_refresh_state_surfaces_401() {
        let inference = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/search"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&inference)
            .await;

        let client = ServerInferenceClient::for_test(
            &inference.uri(),
            "proj",
            Some("sk-legacy".to_string()),
            None,
        );
        let err = client.search_query("q", "semantic", 1).await.unwrap_err();
        assert!(err.to_string().contains("/search") || err.to_string().contains("401"));
    }

    /// Loop-safety guard: when the retry *after* a successful refresh also 401s,
    /// the request is NOT refreshed/retried a second time. The retry surfaces the
    /// 401 (one inference call, then exactly one refresh, then exactly one retry,
    /// total two inference hits and one refresh) rather than spinning forever.
    #[tokio::test]
    async fn refresh_retry_caps_at_one_and_does_not_loop() {
        let inference = MockServer::start().await;
        let cloud = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        // EVERY /search call returns 401 — both the original and the retry.
        // `.expect(2)` is the loop guard: a third hit (i.e. a second retry)
        // would make wiremock fail the test on drop.
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/search"))
            .respond_with(ResponseTemplate::new(401))
            .expect(2)
            .mount(&inference)
            .await;

        // The refresh endpoint must be called EXACTLY once. A second refresh
        // (the infinite-loop failure mode) would exceed this and fail the test.
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": jwt("new", "org_1", 5_000_000_000),
                "refresh_token": "rt-new",
                "organization_id": "org_1",
            })))
            .expect(1)
            .mount(&cloud)
            .await;

        let token = expiring_tokens(5_000_000_000); // not locally expired
        let client = ServerInferenceClient::for_test(
            &inference.uri(),
            "proj",
            Some("at-old".to_string()),
            Some((token, cloud.uri(), config_path)),
        );

        // The persistent 401 surfaces as an error, NOT a hang.
        let err = client
            .search_query("q", "semantic", 1)
            .await
            .expect_err("a persistent 401 after one refresh must surface an error");
        assert!(
            err.to_string().contains("/search") || err.to_string().contains("401"),
            "error should reflect the failed /search, got: {err}"
        );
        // Mock `.expect(..)` assertions verify on drop: search hit twice, refresh once.
    }

    /// `from_config` carries refresh state ONLY when the bearer was resolved from
    /// the `[auth]` access token — so a `spelunk login` session can refresh.
    #[test]
    #[serial_test::serial]
    fn from_config_attaches_refresh_state_for_auth_token_bearer() {
        unsafe {
            std::env::remove_var("SPELUNK_SERVER_KEY");
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let tokens = AuthTokens {
            access_token: "at-login".into(),
            refresh_token: "rt-login".into(),
            expires_at: 4_000_000_000,
            org_id: "org_1".into(),
        };
        spelunk_core::config::save_auth_tokens_to(&tokens, &path).unwrap();

        let mut cfg = crate::config::Config::load(Some(&path)).unwrap();
        // from_config needs an inference URL to build a client at all.
        cfg.inference_url = Some("http://127.0.0.1:7777".into());
        let client = ServerInferenceClient::from_config(&cfg).expect("client builds");

        let guard = client.auth.lock().unwrap();
        assert!(
            guard.refresh.is_some(),
            "an [auth]-derived bearer must carry refresh state so it can rotate"
        );
        assert_eq!(guard.bearer.as_deref(), Some("at-login"));
    }

    /// A bare legacy `server_key` (no `[auth]` table) must NOT carry refresh
    /// state — there is nothing to refresh, so a 401 surfaces immediately and we
    /// never call `/v1/auth/token` with a non-existent refresh token. Guards the
    /// "legacy bearer does not attempt refresh" contract at the config boundary.
    #[test]
    #[serial_test::serial]
    fn from_config_no_refresh_state_for_legacy_server_key() {
        unsafe {
            std::env::remove_var("SPELUNK_SERVER_KEY");
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "server_key = \"sk-legacy\"\n").unwrap();

        let mut cfg = crate::config::Config::load(Some(&path)).unwrap();
        cfg.inference_url = Some("http://127.0.0.1:7777".into());
        let client = ServerInferenceClient::from_config(&cfg).expect("client builds");

        let guard = client.auth.lock().unwrap();
        assert!(
            guard.refresh.is_none(),
            "a bare legacy server_key must not be treated as refreshable"
        );
        assert_eq!(guard.bearer.as_deref(), Some("sk-legacy"));
    }

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

    /// The synthetic query `chunk_id` is built from a fresh `uuid` crate v7
    /// UUID. Two calls must differ (so concurrent queries never collide), and
    /// the value must be a real version-7 UUID — the `query:` prefix is what
    /// makes it distinguishable in server logs.
    #[test]
    fn query_chunk_id_is_unique_uuid_v7() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        assert_ne!(a, b, "two query nonces must not collide");
        assert_eq!(a.get_version(), Some(uuid::Version::SortRand));
        let chunk_id = format!("query:{a}");
        assert!(chunk_id.starts_with("query:"));
    }
}
