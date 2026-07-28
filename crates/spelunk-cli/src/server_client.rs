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
    /// `true` when `base_url` came from an explicitly configured team
    /// `server_url` rather than loopback auto-discovery (which populates
    /// `inference_url` while leaving `server_url` unset, ADR-004). An
    /// inference error against an explicit remote must name `base_url`
    /// instead of pointing at `spelunk server logs`, which only reads the
    /// local auto-daemon's log.
    is_explicit_remote: bool,
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
/// Refresh now goes DIRECTLY to WorkOS, so the WorkOS base URL and the
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
    ///
    /// The bearer is resolved per-origin via `Config::bearer_for` (ADR-071
    /// D2): a self-hosted server never receives a cloud `[auth]` token meant
    /// for a different origin, and vice versa.
    pub fn from_config(cfg: &Config) -> Option<Self> {
        let base_url = cfg
            .resolve_inference_url()?
            .trim_end_matches('/')
            .to_string();
        let bearer = cfg
            .bearer_for(&base_url)
            .expect("resolving per-server bearer credential");
        Some(Self::build(cfg, base_url, bearer))
    }

    /// Same as [`from_config`](Self::from_config) but with an injected
    /// [`SecretStore`](spelunk_core::config::secret_store::SecretStore), so
    /// in-process tests can exercise bearer resolution without touching the
    /// real default secret store.
    #[cfg(test)]
    fn from_config_with_store(
        cfg: &Config,
        store: &dyn spelunk_core::config::secret_store::SecretStore,
    ) -> Option<Self> {
        let base_url = cfg
            .resolve_inference_url()?
            .trim_end_matches('/')
            .to_string();
        let bearer = cfg
            .bearer_for_with_store(&base_url, store)
            .expect("resolving per-server bearer credential");
        Some(Self::build(cfg, base_url, bearer))
    }

    /// Shared construction once `base_url` and `bearer` are resolved.
    fn build(cfg: &Config, base_url: String, bearer: Option<String>) -> Self {
        if let Err(msg) = spelunk_core::config::validate_transport_url(&base_url) {
            // Fail loudly and immediately: the alternative is silently sending a
            // bearer token in the clear. No opt-out: the fix is always "use
            // https, or loopback".
            eprintln!("error: {msg}");
            std::process::exit(2);
        }
        let project_id = cfg.project_id.clone().unwrap_or_default();
        let client = spelunk_core::config::apply_server_ca(
            reqwest::Client::builder(),
            cfg.server_ca.as_deref().map(std::path::Path::new),
        )
        .expect("applying custom CA for server inference")
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .expect("building HTTP client for server inference");

        // Carry WorkOS refresh state only when the resolved bearer came from
        // `[auth]`, i.e. `base_url`'s origin is the cloud kind (ADR-071 D2).
        // A self-hosted server-key / env token is not refreshable here.
        // Refresh targets WorkOS directly: the WorkOS base URL and the
        // embedded public client_id (derived from the default cloud host)
        // are captured here.
        let refresh = cfg
            .auth
            .as_ref()
            .filter(|a| Some(a.access_token.as_str()) == bearer.as_deref())
            .map(|tokens| RefreshState {
                tokens: tokens.clone(),
                workos_url: auth_api::workos_url(),
                client_id: auth_api::workos_client_id(auth_api::DEFAULT_CLOUD_URL),
                config_path: None,
            });

        Self {
            client,
            base_url,
            project_id,
            // Mirrors `Config::resolve_inference_url`'s own fallback exactly:
            // `base_url` came from `server_url` iff `inference_url` was unset.
            // Since the 2026-07-23 ADR-004 revision,
            // `effective_config` CAN set `inference_url` even when
            // `server_url` is ALSO set (the `local_first` case: an explicit
            // `server_url` there is a sync replica only, never the inference
            // target) — so `cfg.server_url.is_some()` alone is no longer a
            // reliable signal of "base_url is the explicit remote".
            is_explicit_remote: cfg.inference_url.is_none() && cfg.server_url.is_some(),
            auth: Mutex::new(BearerState { bearer, refresh }),
        }
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
            is_explicit_remote: false,
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

    /// Mark this test client as reached via an explicit remote `server_url`
    /// (not loopback auto-discovery), for tests covering the scoped
    /// inference-error hint.
    #[cfg(test)]
    fn with_explicit_remote(mut self) -> Self {
        self.is_explicit_remote = true;
        self
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
    /// (refresh grant), persist the rotated tokens, and update the
    /// in-memory bearer.
    ///
    /// Returns `Ok(true)` when a refresh was performed, `Ok(false)` when there
    /// is no refresh state (a bare `server_key` — nothing to refresh). Errors
    /// carry a clear "re-run `spelunk login`" message.
    async fn refresh_access_token(&self) -> Result<bool> {
        let (refresh_token, org_id, workos_url, client_id, config_path) = {
            let guard = self.auth.lock().expect("auth mutex poisoned");
            match &guard.refresh {
                Some(r) => (
                    r.tokens.refresh_token.clone(),
                    r.tokens.org_id.clone(),
                    r.workos_url.clone(),
                    r.client_id.clone(),
                    r.config_path.clone(),
                ),
                None => return Ok(false),
            }
        };

        // Re-send the active org so a prior `org switch` survives rotation
        // instead of reverting to the account's default org (see auth_api::
        // ensure_fresh_token, which has the same requirement).
        let rotated = auth_api::refresh_token(
            &self.client,
            &workos_url,
            &client_id,
            &refresh_token,
            auth_api::org_id_for_refresh(&org_id),
        )
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
        let resp = self
            .send_authed(|| self.client.post(&url).json(&body))
            .await
            .context("POST /index/embed (query vector)")?;
        // Surface the server's structured reason (e.g. embedder still loading /
        // failed to load) instead of a bare "HTTP status 503" so a memory search
        // against a warming-up local server gets actionable guidance.
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let remote_url = self.is_explicit_remote.then_some(self.base_url.as_str());
            anyhow::bail!(
                "{}",
                server_inference_error("/index/embed", status, &text, remote_url)
            );
        }
        let bytes = resp
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

/// Format a non-2xx spelunk-server inference response into an actionable error.
///
/// The server returns a `{ error, state, detail }` JSON body for an unready
/// embedder (state `loading`/`unavailable`). `reqwest::error_for_status` throws
/// that body away and yields a bare "HTTP status 503", so we parse it here and
/// append a next-step hint.
///
/// `remote_url` is `Some` when this client reached the server via an explicit
/// `server_url` (not loopback auto-discovery). The `unavailable` hint must
/// then name that server instead of pointing at `spelunk server logs`, which
/// only reads the local auto-daemon's log and would show clean logs for a
/// failure that lives on the remote server.
fn server_inference_error(
    endpoint: &str,
    status: reqwest::StatusCode,
    body: &str,
    remote_url: Option<&str>,
) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let field = |k: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    };
    let reason = field("detail").or_else(|| field("error"));
    let hint = match field("state") {
        Some("loading") => " Retry shortly (`spelunk server status`).".to_string(),
        Some("unavailable") => match remote_url {
            Some(url) => format!(" Check the logs for team server {url}."),
            None => " See `spelunk server logs`.".to_string(),
        },
        _ => String::new(),
    };
    match reason {
        Some(reason) => format!("spelunk-server {endpoint} returned {status}: {reason}.{hint}"),
        None => format!("spelunk-server {endpoint} returned {status}.{hint}"),
    }
}

// ── Tier-0 error helper ───────────────────────────────────────────────────────

/// Return the locked-feature error when harvest is attempted without a server.
///
/// Harvest needs inference only, so a local `spelunk server start` suffices.
/// See `capability::inference_server_required_message`.
pub fn harvest_requires_server() -> anyhow::Error {
    anyhow::anyhow!(crate::capability::inference_server_required_message(
        "memory harvest"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelunk_core::config::AuthTokens;
    use wiremock::matchers::{body_string_contains, header, method, path};
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

        // The WorkOS refresh exchange rotates the tokens.
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
        // Inject an in-memory secret store so the test never touches the real
        // OS keychain (DI; cf. config.rs tests). #473 only isolated the
        // spawned-binary integration tests in tests/*, not these in-process ones.
        let cfg = spelunk_core::config::Config::load_with_store(
            Some(&config_path),
            &spelunk_core::config::secret_store::MemoryStore::default(),
        )
        .unwrap();
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

        // The refresh request must carry the stored `org_1` scope, so it never
        // silently reverts to the account's default org on rotation.
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .and(body_string_contains("organization_id=org_1"))
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
    /// the `[auth]` access token, so a `spelunk login` session can refresh. That
    /// only happens for a cloud-origin target (ADR-071 D2); a self-hosted origin
    /// never resolves to the cloud token, whatever `[auth]` holds.
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

        let store = spelunk_core::config::secret_store::MemoryStore::default();
        let mut cfg = crate::config::Config::load_with_store(Some(&path), &store).unwrap();
        // The cloud kind only applies for the cloud origin (ADR-071 D2).
        cfg.inference_url = Some(auth_api::DEFAULT_CLOUD_URL.to_string());
        let client =
            ServerInferenceClient::from_config_with_store(&cfg, &store).expect("client builds");

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

        let store = spelunk_core::config::secret_store::MemoryStore::default();
        let mut cfg = crate::config::Config::load_with_store(Some(&path), &store).unwrap();
        cfg.inference_url = Some("http://127.0.0.1:7777".into());
        let client =
            ServerInferenceClient::from_config_with_store(&cfg, &store).expect("client builds");

        let guard = client.auth.lock().unwrap();
        assert!(
            guard.refresh.is_none(),
            "a bare legacy server_key must not be treated as refreshable"
        );
        assert_eq!(guard.bearer.as_deref(), Some("sk-legacy"));
    }

    // `is_explicit_remote` keys off whether `base_url` resolved from
    // `server_url` (never off what host it resolves to): an explicitly
    // configured `server_url = http://127.0.0.1:PORT` is still "explicit"
    // even though the host is loopback. `spelunk server logs` only ever
    // reads the fixed auto-daemon log path and cannot tell this loopback
    // address was hand-configured, so the inference-error hint must still
    // name it. Mirrors
    // `capability::tier::tests::tier_explicit_remote_url_is_explicit_even_when_host_is_loopback`,
    // which pins the same invariant on the `Tier` side of this contract.
    //
    // `mode: CloudFirst` is required here since the 2026-07-23 ADR-004
    // revision: `resolve_inference_url` only falls back to
    // `server_url` in `cloud_first` (in `local_first`, the default this test
    // used to rely on, a bare `server_url` no longer resolves to any
    // inference `base_url` at all — see
    // `from_config_local_first_with_only_server_url_set_has_no_inference_target`
    // below for that regression).
    #[test]
    #[serial_test::serial]
    fn from_config_is_explicit_remote_true_for_explicitly_configured_loopback_url() {
        unsafe {
            std::env::remove_var("SPELUNK_SERVER_KEY");
        }
        let cfg = crate::config::Config {
            server_url: Some("http://127.0.0.1:9797".to_string()),
            project_id: Some("proj".to_string()),
            mode: Some(spelunk_core::config::SyncMode::CloudFirst),
            ..Default::default()
        };
        // Inject an in-memory secret store so this test never touches the
        // real OS keychain (DI; cf. the `refresh_on_401_retries_once_and_persists`
        // comment above and config.rs tests). This test previously called
        // the production `from_config` entry point directly, which resolves
        // the bearer via `Config::bearer_for` against the *real* default
        // secret store; on macOS in a headless session that keychain
        // lookup blocks indefinitely instead of failing fast, which is what
        // made this test (and the whole module) appear to hang "even in
        // isolation" without `SPELUNK_SECRET_STORE=file` set in the
        // environment.
        let store = spelunk_core::config::secret_store::MemoryStore::default();
        let client =
            ServerInferenceClient::from_config_with_store(&cfg, &store).expect("client builds");
        assert!(
            client.is_explicit_remote,
            "an explicitly configured server_url must count as explicit even when it is loopback"
        );
    }

    /// Regression guard: with only `server_url` set (no
    /// `inference_url`) and no explicit `mode`, the config defaults to
    /// `local_first`, and `local_first` must NOT resolve any inference
    /// `base_url` from `server_url`. `from_config` must return `None` rather
    /// than silently building a client aimed at `server_url` (which is what
    /// produced 404s against a cloud `server_url`'s nonexistent
    /// `/index/embed` route).
    #[test]
    #[serial_test::serial]
    fn from_config_local_first_with_only_server_url_set_has_no_inference_target() {
        unsafe {
            std::env::remove_var("SPELUNK_SERVER_KEY");
        }
        let cfg = crate::config::Config {
            server_url: Some("https://api.spelunk.cloud".to_string()),
            project_id: Some("proj".to_string()),
            mode: None,
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_mode(),
            spelunk_core::config::SyncMode::LocalFirst
        );
        let store = spelunk_core::config::secret_store::MemoryStore::default();
        assert!(
            ServerInferenceClient::from_config_with_store(&cfg, &store).is_none(),
            "local_first must not build an inference client aimed at a bare server_url"
        );
    }

    /// End-to-end regression test for the founder's own manual repro
    /// (2026-07-23): `local_first`, `server_url` set to a
    /// cloud host, no explicit `mode` → embedding must reach the LOCAL
    /// loopback embedder, never the configured `server_url`. Modelled at this
    /// layer by setting `inference_url` directly to a mocked loopback server
    /// (what `Tier::effective_config` does once it has probed one, tested
    /// separately in `capability::tier`) alongside a `server_url` pointed at
    /// an address nothing mounts anything on: an accidental fallback would
    /// surface as a hard connection error here, never a silent pass.
    #[tokio::test]
    #[serial_test::serial]
    async fn embed_text_local_first_uses_loopback_not_configured_server_url() {
        unsafe {
            std::env::remove_var("SPELUNK_SERVER_KEY");
        }
        let loopback = MockServer::start().await;
        let dim = spelunk_core::embeddings::EMBEDDING_DIM;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/index/embed"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(spelunk_core::embeddings::vec_to_blob(&vec![0.25_f32; dim])),
            )
            .mount(&loopback)
            .await;

        let cfg = crate::config::Config {
            inference_url: Some(loopback.uri()),
            server_url: Some("https://cloud.invalid.example:1".to_string()),
            project_id: Some("proj".to_string()),
            mode: None, // defaults to local_first because server_url is set
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_mode(),
            spelunk_core::config::SyncMode::LocalFirst
        );

        let store = spelunk_core::config::secret_store::MemoryStore::default();
        let client = ServerInferenceClient::from_config_with_store(&cfg, &store)
            .expect("client must build from inference_url (the loopback server)");
        assert!(
            !client.is_explicit_remote,
            "base_url resolved from inference_url, not server_url"
        );

        let vec = client.embed_text("hello").await.expect(
            "embedding must reach the local loopback server, not the unroutable \
             cloud server_url",
        );
        assert_eq!(vec.len(), dim);
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

    // ── transport-scheme validation ──────────────────────────────────────────
    //
    // `from_config` hard-exits the process on an invalid (non-loopback http://)
    // inference URL, so the exit path itself isn't exercised in-process here
    // (that would kill the test binary). These tests instead cover the pure
    // validator directly (used identically by `capability::probe::probe_url`) and
    // confirm `from_config` still builds normally for every URL shape the
    // validator accepts.

    #[test]
    fn transport_validator_rejects_non_loopback_http() {
        let err = spelunk_core::config::validate_transport_url("http://team-server:7777")
            .expect_err("non-loopback http:// must be rejected");
        assert!(err.contains("loopback"));
        assert!(err.contains("https"));
    }

    #[test]
    #[serial_test::serial]
    fn from_config_accepts_loopback_http_inference_url() {
        unsafe {
            std::env::remove_var("SPELUNK_SERVER_KEY");
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let store = spelunk_core::config::secret_store::MemoryStore::default();
        let mut cfg = crate::config::Config::load_with_store(Some(&path), &store).unwrap();
        cfg.inference_url = Some("http://127.0.0.1:7777".into());
        assert!(
            ServerInferenceClient::from_config_with_store(&cfg, &store).is_some(),
            "loopback http:// inference URL must be accepted"
        );
    }

    #[test]
    #[serial_test::serial]
    fn from_config_accepts_https_inference_url() {
        unsafe {
            std::env::remove_var("SPELUNK_SERVER_KEY");
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let store = spelunk_core::config::secret_store::MemoryStore::default();
        let mut cfg = crate::config::Config::load_with_store(Some(&path), &store).unwrap();
        cfg.inference_url = Some("https://team-server:7777".into());
        assert!(
            ServerInferenceClient::from_config_with_store(&cfg, &store).is_some(),
            "https:// inference URL (any host) must be accepted"
        );
    }

    // ── server_inference_error ───────────────────────────────────────────────

    #[test]
    fn inference_error_surfaces_loading_detail_and_retry_hint() {
        let body = serde_json::json!({
            "error": "embedder warming up, retry shortly",
            "state": "loading",
            "detail": "downloading model (42%)",
        })
        .to_string();
        let msg = server_inference_error(
            "/index/embed",
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            &body,
            None,
        );
        assert!(msg.contains("downloading model (42%)"), "got: {msg}");
        assert!(msg.contains("spelunk server status"), "got: {msg}");
        // The bare status is no longer the whole story.
        assert!(msg.contains("503"), "got: {msg}");
    }

    #[test]
    fn inference_error_surfaces_unavailable_loopback_points_at_logs() {
        // Loopback auto-discovery: the failing embedder IS the local daemon,
        // so `spelunk server logs` is the right place to look.
        let body = serde_json::json!({
            "error": "embedder unavailable",
            "state": "unavailable",
            "detail": "OOM loading GGUF",
        })
        .to_string();
        let msg = server_inference_error(
            "/index/embed",
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            &body,
            None,
        );
        assert!(msg.contains("OOM loading GGUF"), "got: {msg}");
        assert!(msg.contains("spelunk server logs"), "got: {msg}");
    }

    #[test]
    fn inference_error_surfaces_unavailable_remote_names_that_server_never_local_logs() {
        // Explicit server_url: `spelunk server logs` reads the LOCAL daemon's
        // log, which is clean when the failure lives on the team server. The
        // error must name the probed server instead.
        let body = serde_json::json!({
            "error": "embedder unavailable",
            "state": "unavailable",
            "detail": "OOM loading GGUF",
        })
        .to_string();
        let msg = server_inference_error(
            "/index/embed",
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            &body,
            Some("https://team.example:7777"),
        );
        assert!(msg.contains("OOM loading GGUF"), "got: {msg}");
        assert!(msg.contains("https://team.example:7777"), "got: {msg}");
        assert!(
            !msg.contains("spelunk server logs"),
            "must not point a remote failure at local logs: {msg}"
        );
    }

    #[test]
    fn inference_error_falls_back_when_body_not_json() {
        // A non-JSON body (proxy error page, empty) still yields a clean line.
        let msg = server_inference_error(
            "/index/embed",
            reqwest::StatusCode::BAD_GATEWAY,
            "<html>502</html>",
            None,
        );
        assert!(msg.contains("/index/embed"), "got: {msg}");
        assert!(msg.contains("502"), "got: {msg}");
    }

    /// End-to-end: a 503 from `/index/embed` must surface the server's `detail`
    /// (not a bare "HTTP status 503") when embedding a query vector.
    #[tokio::test]
    async fn embed_text_surfaces_server_detail_on_503() {
        let inference = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/index/embed"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "error": "embedder warming up, retry shortly",
                "state": "loading",
                "detail": "loading F2LLM weights",
            })))
            .mount(&inference)
            .await;

        let client =
            ServerInferenceClient::for_test(&inference.uri(), "proj", Some("sk".into()), None);
        let err = client
            .embed_text("hello")
            .await
            .expect_err("503 must surface as an error");
        let msg = err.to_string();
        assert!(msg.contains("loading F2LLM weights"), "got: {msg}");
        assert!(msg.contains("spelunk server status"), "got: {msg}");
    }

    /// End-to-end: a 503 `unavailable` from `/index/embed` against an
    /// explicit team `server_url` must name that server, never `spelunk
    /// server logs` (which would read a healthy local daemon's log instead).
    #[tokio::test]
    async fn embed_text_remote_names_that_server_never_local_logs() {
        let inference = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/index/embed"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "error": "embedder unavailable",
                "state": "unavailable",
                "detail": "OOM loading GGUF",
            })))
            .mount(&inference)
            .await;

        let client =
            ServerInferenceClient::for_test(&inference.uri(), "proj", Some("sk".into()), None)
                .with_explicit_remote();
        let err = client
            .embed_text("hello")
            .await
            .expect_err("503 must surface as an error");
        let msg = err.to_string();
        assert!(msg.contains("OOM loading GGUF"), "got: {msg}");
        assert!(msg.contains(&inference.uri()), "got: {msg}");
        assert!(
            !msg.contains("spelunk server logs"),
            "must not point a remote failure at local logs: {msg}"
        );
    }

    /// `/v1/health`-style probes aside, inference requests built via
    /// `send_authed`/`authed` still attach the bearer — this is expected (those
    /// routes ARE authenticated); this test just documents/pins that behaviour
    /// so a future edit doesn't accidentally strip auth from real inference
    /// calls while fixing the health probe.
    #[tokio::test]
    async fn inference_requests_still_carry_bearer_when_present() {
        let inference = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/search"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_vector": [0.1_f32],
                "mode": "semantic",
            })))
            .expect(1)
            .mount(&inference)
            .await;

        let client = ServerInferenceClient::for_test(
            &inference.uri(),
            "proj",
            Some("sk-test".to_string()),
            None,
        );
        let vec = client.search_query("q", "semantic", 1).await.unwrap();
        assert_eq!(vec, Some(vec![0.1_f32]));
    }
}
