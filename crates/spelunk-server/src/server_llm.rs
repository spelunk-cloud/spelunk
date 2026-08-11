//! The server's OpenAI-compatible LLM backend, plus resolution and validation
//! of the credential it authenticates with.
//!
//! The credential never comes from a keychain: this process is commonly a
//! detached daemon with no user session, so the spawning CLI resolves the
//! value and passes it in via `SPELUNK_LLM_KEY` (or, for an operator running
//! the binary directly, `--llm-key-file`).

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicU8, Ordering};

/// Trim `raw` and treat a blank result as "no key", so a set-but-empty
/// `SPELUNK_LLM_KEY` reads as unauthenticated rather than as an empty-string
/// credential that every upstream request would then send.
fn normalize(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Resolve the LLM credential: `--llm-key`, then `--llm-key-file`, then
/// `SPELUNK_LLM_KEY`.
///
/// An unreadable `--llm-key-file` is fatal rather than a fall-through: an
/// operator who named a file meant that file, and silently authenticating
/// with a different credential (or none) is worse than failing to start.
pub fn resolve_llm_key(
    key: Option<&str>,
    key_file: Option<&std::path::Path>,
    env_key: Option<&str>,
) -> Result<Option<String>> {
    if let Some(k) = normalize(key) {
        return Ok(Some(k));
    }
    if let Some(path) = key_file {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading --llm-key-file {}", path.display()))?;
        if let Some(k) = normalize(Some(&raw)) {
            return Ok(Some(k));
        }
    }
    Ok(normalize(env_key))
}

/// Refuse to send an LLM credential over plaintext HTTP to a non-loopback host.
///
/// Scoped to the credential's presence: a keyless LAN endpoint (LM Studio or
/// Ollama on `http://192.168.x.x:1234`) is an established, supported setup and
/// keeps working untouched.
pub fn check_llm_transport(llm_url: &str, has_key: bool) -> Result<()> {
    if !has_key {
        return Ok(());
    }
    spelunk_core::config::validate_transport_url(llm_url).map_err(|e| {
        anyhow::anyhow!(
            "{e}. An LLM key is configured, so {llm_url:?} would send it in the clear: \
             use an https:// endpoint, or unset the key"
        )
    })
}

/// OpenAI-compatible chat-completions backend.
///
/// `api_key` is `Some` only when a credential resolved; when it is `None` the
/// request carries no `Authorization` header, so keyless local endpoints that
/// reject unexpected headers keep working.
///
/// `reasoning_effort` is sent on every request when `Some` (default `"none"`)
/// to suppress chain-of-thought on reasoning models: our uses (memory harvest,
/// explore) want the JSON answer, not the model's thinking, and an unbounded
/// reasoning pass burns the whole `max_tokens` budget before any `content`
/// arrives. `None` omits the field for endpoints that reject it.
///
/// `structured_mode` caches how this endpoint accepts a requested JSON schema.
/// The first schema-bearing request tries strict `response_format.json_schema`;
/// an endpoint that rejects it (DeepSeek answers HTTP 400 "This response_format
/// type is unavailable now") is retried at the next-weaker mode, and the first
/// mode that succeeds is remembered so later requests skip the doomed attempt.
/// See [`StructuredMode`].
pub struct ServerLlm {
    pub client: reqwest::Client,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub reasoning_effort: Option<String>,
    pub structured_mode: AtomicU8,
}

/// How the endpoint is asked to constrain output to a requested JSON schema,
/// strongest first. Weaker modes are the fallback for endpoints that reject a
/// stronger one; every mode still gets the schema described in the prompt and
/// is parsed by the same defensive JSON parser downstream.
///
/// * `JsonSchema`: OpenAI strict structured output, `response_format` =
///   `{"type":"json_schema","json_schema":{…}}`. The exact shape is enforced
///   server-side.
/// * `JsonObject`: `response_format` = `{"type":"json_object"}`. The endpoint
///   only guarantees valid JSON, not the schema; DeepSeek and older OpenAI-
///   compatible servers accept this where they reject `json_schema`. Relies on
///   the prompt (which names the schema and contains the word "json") for shape.
/// * `Prompt`: no `response_format` at all. The last resort for endpoints that
///   reject every `response_format`; JSON is requested by the prompt alone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StructuredMode {
    JsonSchema,
    JsonObject,
    Prompt,
}

impl StructuredMode {
    /// Strongest first, so a cached ordinal is monotonic: a downgrade only ever
    /// moves toward `Prompt`, never back.
    const ORDERED: [StructuredMode; 3] = [
        StructuredMode::JsonSchema,
        StructuredMode::JsonObject,
        StructuredMode::Prompt,
    ];

    fn from_ordinal(v: u8) -> StructuredMode {
        Self::ORDERED
            .get(usize::from(v))
            .copied()
            .unwrap_or(StructuredMode::Prompt)
    }

    fn ordinal(self) -> u8 {
        match self {
            StructuredMode::JsonSchema => 0,
            StructuredMode::JsonObject => 1,
            StructuredMode::Prompt => 2,
        }
    }

    /// The `response_format` value for this mode given the requested schema, or
    /// `None` when the field must be omitted.
    fn response_format(self, schema: &serde_json::Value) -> Option<serde_json::Value> {
        match self {
            StructuredMode::JsonSchema => {
                Some(serde_json::json!({ "type": "json_schema", "json_schema": schema }))
            }
            StructuredMode::JsonObject => Some(serde_json::json!({ "type": "json_object" })),
            StructuredMode::Prompt => None,
        }
    }
}

/// Upstream statuses worth retrying the same request unchanged: rate limiting
/// and transient server-side failures. Deliberately excludes 400/422, which
/// mean the request itself is unacceptable and are handled by stepping down the
/// structured-output mode instead. Harvest fires many batches back to back at a
/// shared endpoint, so the occasional 429/503 that a brief backoff clears must
/// not fail the batch outright.
fn is_transient_status(code: u16) -> bool {
    code == 408 || code == 425 || code == 429 || (500..=599).contains(&code)
}

#[async_trait::async_trait]
impl spelunk_core::llm::LlmBackend for ServerLlm {
    async fn generate(
        &self,
        messages: &[spelunk_core::llm::Message],
        max_tokens: usize,
        tx: tokio::sync::mpsc::Sender<spelunk_core::llm::Token>,
        json_schema: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        use futures_util::StreamExt;

        #[derive(serde::Serialize)]
        struct ChatReq<'a> {
            model: &'a str,
            messages: &'a [ChatMsg<'a>],
            stream: bool,
            max_tokens: usize,
            temperature: f32,
            #[serde(skip_serializing_if = "Option::is_none")]
            reasoning_effort: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            response_format: Option<serde_json::Value>,
        }
        #[derive(serde::Serialize)]
        struct ChatMsg<'a> {
            role: &'a str,
            content: &'a str,
        }
        #[derive(serde::Deserialize)]
        struct StreamChunk {
            choices: Vec<StreamChoice>,
        }
        #[derive(serde::Deserialize)]
        struct StreamChoice {
            delta: Delta,
        }
        #[derive(serde::Deserialize)]
        struct Delta {
            content: Option<String>,
            // Reasoning models (DeepSeek, etc.) stream chain-of-thought here,
            // as a sibling of `content`. It is intermediate output, never the
            // answer, so it is parsed only to be dropped: forwarding it would
            // corrupt a JSON-schema-constrained completion.
            #[serde(default)]
            reasoning_content: Option<String>,
        }

        let chat_messages: Vec<ChatMsg> = messages
            .iter()
            .map(|m| ChatMsg {
                role: &m.role,
                content: &m.content,
            })
            .collect();

        // A schema-bearing request tries the cached structured-output mode first
        // and, on an HTTP 400/422 that rejects `response_format`, steps down to
        // the next-weaker mode (json_schema -> json_object -> prompt-only),
        // caching the first mode this endpoint accepts so later requests skip the
        // rejected attempt. Without a schema there is nothing to constrain: one
        // unconstrained request is made and any error is surfaced as-is.
        let attempts: Vec<(Option<StructuredMode>, Option<serde_json::Value>)> = match &json_schema
        {
            Some(schema) => {
                let start =
                    StructuredMode::from_ordinal(self.structured_mode.load(Ordering::Relaxed));
                StructuredMode::ORDERED
                    .into_iter()
                    .skip(usize::from(start.ordinal()))
                    .map(|m| (Some(m), m.response_format(schema)))
                    .collect()
            }
            None => vec![(None, None)],
        };

        let endpoint = format!("{}/v1/chat/completions", self.base_url);
        let last = attempts.len() - 1;
        // A transient upstream failure (429, 5xx, a dropped connection) is
        // retried in place with capped exponential backoff before the batch is
        // failed: harvest issues many batches at a shared endpoint and the
        // occasional 429/503 clears on a brief retry. This is a separate axis
        // from the structured-output downgrade below, which is for an endpoint
        // that refuses the request's shape rather than one momentarily unable to
        // serve it.
        const MAX_TRANSIENT_RETRIES: u32 = 3;
        let mut response = None;
        'modes: for (i, (mode, response_format)) in attempts.into_iter().enumerate() {
            let mut backoff = std::time::Duration::from_millis(300);
            for attempt in 0..=MAX_TRANSIENT_RETRIES {
                let has_retries_left = attempt < MAX_TRANSIENT_RETRIES;
                let req = ChatReq {
                    model: &self.model,
                    messages: &chat_messages,
                    stream: true,
                    max_tokens,
                    temperature: 0.7,
                    reasoning_effort: self.reasoning_effort.as_deref(),
                    response_format: response_format.clone(),
                };

                let mut builder = self.client.post(endpoint.as_str());
                if let Some(key) = &self.api_key {
                    builder = builder.bearer_auth(key);
                }

                let resp = match builder.json(&req).send().await {
                    Ok(resp) => resp,
                    Err(e) if has_retries_left => {
                        tracing::warn!(error = %e, "LLM request send failed; retrying in {backoff:?}");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(std::time::Duration::from_secs(8));
                        continue;
                    }
                    Err(e) => return Err(e).context("calling LLM server"),
                };

                let status = resp.status();
                if status.is_success() {
                    if let Some(mode) = mode {
                        self.structured_mode
                            .store(mode.ordinal(), Ordering::Relaxed);
                    }
                    response = Some(resp);
                    break 'modes;
                }

                let code = status.as_u16();

                if is_transient_status(code) && has_retries_left {
                    let body = resp.text().await.unwrap_or_default();
                    tracing::warn!(
                        status = code,
                        "LLM upstream transient error; retrying in {backoff:?}: {}",
                        body.trim()
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(std::time::Duration::from_secs(8));
                    continue;
                }

                // A 400/422 that is not the last mode is the endpoint refusing
                // this `response_format`; step down. If it is wrong for another
                // reason, the final prompt-only attempt carries no
                // `response_format` and surfaces that true error instead.
                if i < last && (code == 400 || code == 422) {
                    let body = resp.text().await.unwrap_or_default();
                    tracing::warn!(
                        status = code,
                        "LLM rejected the structured-output request; retrying at a weaker mode: {}",
                        body.trim()
                    );
                    continue 'modes;
                }

                // Include the upstream status and body: "LLM server returned an
                // error" alone is undiagnosable, and this path is exactly where a
                // still-unexplained failure surfaces.
                let body = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!(
                    "LLM server returned an error (HTTP {code}): {}",
                    body.trim()
                ));
            }
        }

        let mut stream = match response {
            Some(resp) => resp.bytes_stream(),
            None => anyhow::bail!("LLM request exhausted all attempts without a response"),
        };

        let mut buffer = String::new();
        let mut saw_content = false;
        let mut saw_reasoning = false;
        'stream: while let Some(chunk) = stream.next().await {
            let bytes = chunk.context("reading SSE byte chunk")?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(pos) = buffer.find("\n\n") {
                let event = buffer[..pos].to_string();
                buffer.drain(..pos + 2);

                for line in event.lines() {
                    let data = match line.strip_prefix("data: ") {
                        Some(d) => d,
                        None => continue,
                    };
                    if data == "[DONE]" {
                        break 'stream;
                    }
                    if data.is_empty() {
                        continue;
                    }
                    if let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) {
                        for choice in chunk.choices {
                            let Delta {
                                content,
                                reasoning_content,
                            } = choice.delta;
                            if reasoning_content.is_some_and(|r| !r.is_empty()) {
                                saw_reasoning = true;
                            }
                            if let Some(content) = content
                                && !content.is_empty()
                            {
                                saw_content = true;
                                if tx.send(content).await.is_err() {
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
            }
        }

        // A reasoning model that ignored `reasoning_effort` can spend the whole
        // `max_tokens` budget thinking and emit no `content` at all. Downstream
        // that surfaces as an opaque parse failure on an empty string; name the
        // real cause here instead.
        if saw_reasoning && !saw_content {
            tracing::warn!(
                max_tokens,
                "LLM streamed only reasoning_content and no content: the reasoning \
                 model likely exhausted the token budget before answering. Raise \
                 max_tokens, or disable reasoning (--llm-reasoning-effort=none)."
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelunk_core::llm::LlmBackend;
    use std::io::Write as _;

    fn write_key_file(dir: &tempfile::TempDir, name: &str, contents: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    // ── resolve_llm_key ──────────────────────────────────────────────────────

    #[test]
    fn inline_key_alone_resolves() {
        assert_eq!(
            resolve_llm_key(Some("sk-inline"), None, None)
                .unwrap()
                .as_deref(),
            Some("sk-inline")
        );
    }

    #[test]
    fn key_file_alone_resolves_trimmed_contents() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_key_file(&dir, "llm.key", "  sk-from-file \n");

        assert_eq!(
            resolve_llm_key(None, Some(&path), None).unwrap().as_deref(),
            Some("sk-from-file")
        );
    }

    #[test]
    fn env_alone_resolves() {
        assert_eq!(
            resolve_llm_key(None, None, Some("sk-from-env"))
                .unwrap()
                .as_deref(),
            Some("sk-from-env")
        );
    }

    #[test]
    fn inline_key_wins_over_file_and_env() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_key_file(&dir, "llm.key", "sk-from-file");

        assert_eq!(
            resolve_llm_key(Some("sk-inline"), Some(&path), Some("sk-from-env"))
                .unwrap()
                .as_deref(),
            Some("sk-inline")
        );
    }

    #[test]
    fn key_file_wins_over_env() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_key_file(&dir, "llm.key", "sk-from-file");

        assert_eq!(
            resolve_llm_key(None, Some(&path), Some("sk-from-env"))
                .unwrap()
                .as_deref(),
            Some("sk-from-file")
        );
    }

    // Naming a file that cannot be read is an operator mistake worth failing
    // on, not a reason to authenticate with some other credential.
    #[test]
    fn missing_key_file_is_fatal_and_does_not_fall_back_to_env() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("absent.key");

        let err = resolve_llm_key(None, Some(&missing), Some("sk-from-env")).unwrap_err();

        assert!(
            format!("{err:#}").contains("absent.key"),
            "the error must name the unreadable file: {err:#}"
        );
    }

    #[test]
    fn blank_from_any_single_source_resolves_to_unset() {
        let dir = tempfile::TempDir::new().unwrap();
        let blank_file = write_key_file(&dir, "blank.key", "   \n");

        assert_eq!(resolve_llm_key(Some("   "), None, None).unwrap(), None);
        assert_eq!(
            resolve_llm_key(None, Some(&blank_file), None).unwrap(),
            None
        );
        assert_eq!(resolve_llm_key(None, None, Some("")).unwrap(), None);
    }

    #[test]
    fn no_source_resolves_to_unset() {
        assert_eq!(resolve_llm_key(None, None, None).unwrap(), None);
    }

    // ── transport guard ──────────────────────────────────────────────────────

    #[test]
    fn a_key_over_plaintext_to_a_non_loopback_host_is_refused() {
        let err = check_llm_transport("http://192.168.1.10:1234", true).unwrap_err();

        assert!(
            format!("{err:#}").contains("192.168.1.10"),
            "the error must name the offending URL: {err:#}"
        );
    }

    #[test]
    fn a_key_over_https_is_allowed() {
        assert!(check_llm_transport("https://gateway.example", true).is_ok());
    }

    #[test]
    fn a_key_over_plaintext_loopback_is_allowed() {
        assert!(check_llm_transport("http://127.0.0.1:1234", true).is_ok());
        assert!(check_llm_transport("http://localhost:1234", true).is_ok());
    }

    // Today's keyless LAN endpoints must keep working untouched: the guard
    // exists because a credential does, so it applies exactly where one is.
    #[test]
    fn a_keyless_plaintext_non_loopback_endpoint_is_allowed() {
        assert!(check_llm_transport("http://192.168.1.10:1234", false).is_ok());
    }

    // ── upstream Authorization header ────────────────────────────────────────

    async fn mount_chat_completions(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_raw("data: [DONE]\n\n", "text/event-stream"),
            )
            .mount(server)
            .await;
    }

    async fn generate_once(base_url: String, api_key: Option<String>) {
        let llm = ServerLlm {
            client: reqwest::Client::new(),
            base_url,
            model: "test-model".to_string(),
            api_key,
            reasoning_effort: Some("none".to_string()),
            structured_mode: AtomicU8::new(0),
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        llm.generate(
            &[spelunk_core::llm::Message {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            16,
            tx,
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_resolved_key_is_sent_as_a_bearer() {
        let server = wiremock::MockServer::start().await;
        mount_chat_completions(&server).await;

        generate_once(server.uri(), Some("sk-llm-secret".to_string())).await;

        let requests = server.received_requests().await.unwrap();
        let auth = requests[0]
            .headers
            .get("authorization")
            .expect("an Authorization header must be sent when a key is configured");
        assert_eq!(auth, "Bearer sk-llm-secret");
    }

    #[tokio::test]
    async fn no_key_means_no_authorization_header_at_all() {
        let server = wiremock::MockServer::start().await;
        mount_chat_completions(&server).await;

        generate_once(server.uri(), None).await;

        let requests = server.received_requests().await.unwrap();
        assert!(
            requests[0].headers.get("authorization").is_none(),
            "a keyless endpoint must keep receiving the request it gets today"
        );
    }

    // ── reasoning control ────────────────────────────────────────────────────

    async fn mount_sse(server: &wiremock::MockServer, body: &str) {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"),
            )
            .mount(server)
            .await;
    }

    async fn generate_collect(base_url: String, reasoning_effort: Option<String>) -> String {
        let llm = ServerLlm {
            client: reqwest::Client::new(),
            base_url,
            model: "test-model".to_string(),
            api_key: None,
            reasoning_effort,
            structured_mode: AtomicU8::new(0),
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
        let collector = tokio::spawn(async move {
            let mut out = String::new();
            while let Some(t) = rx.recv().await {
                out.push_str(&t);
            }
            out
        });
        llm.generate(
            &[spelunk_core::llm::Message {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            64,
            tx,
            None,
        )
        .await
        .unwrap();
        collector.await.unwrap()
    }

    #[tokio::test]
    async fn reasoning_effort_is_sent_when_configured() {
        let server = wiremock::MockServer::start().await;
        mount_sse(&server, "data: [DONE]\n\n").await;

        let _ = generate_collect(server.uri(), Some("none".to_string())).await;

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            body["reasoning_effort"], "none",
            "reasoning_effort must be sent so reasoning models skip chain-of-thought"
        );
    }

    #[tokio::test]
    async fn reasoning_effort_is_omitted_when_unset() {
        let server = wiremock::MockServer::start().await;
        mount_sse(&server, "data: [DONE]\n\n").await;

        let _ = generate_collect(server.uri(), None).await;

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(
            body.get("reasoning_effort").is_none(),
            "with reasoning control disabled the field must not appear at all"
        );
    }

    // A reasoning model streams chain-of-thought (reasoning_content) before the
    // real answer (content). Only the answer may reach the caller: reasoning
    // deltas prepended to a JSON-schema completion would break parsing.
    #[tokio::test]
    async fn reasoning_content_is_dropped_and_content_is_forwarded() {
        let server = wiremock::MockServer::start().await;
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"let me think\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\" harder\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"ok\\\":\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"true}\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        mount_sse(&server, body).await;

        let out = generate_collect(server.uri(), Some("none".to_string())).await;

        assert_eq!(
            out, "{\"ok\":true}",
            "only content deltas may be forwarded; reasoning_content must be dropped"
        );
    }

    // ── structured-output fallback ───────────────────────────────────────────

    const CONTENT_SSE: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"ok\\\":true}\"}}]}\n\n",
        "data: [DONE]\n\n",
    );

    // A single MockServer response can't vary on the request, so drive it from
    // the request body: reject whatever `response_format` `reject` matches (as
    // DeepSeek does with `json_schema`), otherwise stream real content.
    struct RejectResponseFormat {
        reject: &'static str,
    }

    impl wiremock::Respond for RejectResponseFormat {
        fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
            let body = String::from_utf8_lossy(&request.body);
            if body.contains(self.reject) {
                wiremock::ResponseTemplate::new(400).set_body_string(
                    "{\"error\":{\"message\":\"This response_format type is unavailable now\",\
                     \"code\":\"invalid_request_error\"}}",
                )
            } else {
                wiremock::ResponseTemplate::new(200).set_body_raw(CONTENT_SSE, "text/event-stream")
            }
        }
    }

    async fn mount_rejecting(server: &wiremock::MockServer, reject: &'static str) {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(RejectResponseFormat { reject })
            .mount(server)
            .await;
    }

    fn test_llm(base_url: String) -> ServerLlm {
        ServerLlm {
            client: reqwest::Client::new(),
            base_url,
            model: "test-model".to_string(),
            api_key: None,
            reasoning_effort: Some("none".to_string()),
            structured_mode: AtomicU8::new(0),
        }
    }

    fn test_schema() -> serde_json::Value {
        serde_json::json!({ "name": "t", "strict": true, "schema": { "type": "object" } })
    }

    async fn drain(llm: &ServerLlm, schema: Option<serde_json::Value>) -> anyhow::Result<String> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
        let collector = tokio::spawn(async move {
            let mut out = String::new();
            while let Some(t) = rx.recv().await {
                out.push_str(&t);
            }
            out
        });
        llm.generate(
            &[spelunk_core::llm::Message {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            64,
            tx,
            schema,
        )
        .await?;
        Ok(collector.await.unwrap())
    }

    #[tokio::test]
    async fn a_json_schema_rejection_falls_back_to_json_object_and_streams_content() {
        let server = wiremock::MockServer::start().await;
        mount_rejecting(&server, "json_schema").await;

        let llm = test_llm(server.uri());
        let out = drain(&llm, Some(test_schema())).await.unwrap();

        assert_eq!(
            out, "{\"ok\":true}",
            "the fallback attempt's content must reach the caller"
        );

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            2,
            "json_schema is rejected, then json_object is accepted"
        );
        let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(first["response_format"]["type"], "json_schema");
        let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(second["response_format"]["type"], "json_object");
    }

    #[tokio::test]
    async fn the_accepted_mode_is_cached_so_later_requests_skip_the_rejected_one() {
        let server = wiremock::MockServer::start().await;
        mount_rejecting(&server, "json_schema").await;

        let llm = test_llm(server.uri());
        drain(&llm, Some(test_schema())).await.unwrap();
        drain(&llm, Some(test_schema())).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            3,
            "first call probes then downgrades (2 requests); the second call starts at the cached mode (1 request)"
        );
        let third = String::from_utf8_lossy(&requests[2].body);
        assert!(
            !third.contains("json_schema"),
            "a cached downgrade must not re-send the rejected json_schema mode: {third}"
        );
    }

    // An endpoint that rejects every `response_format` must still complete via
    // the prompt-only mode, which sends no `response_format` at all.
    #[tokio::test]
    async fn a_full_downgrade_ends_at_a_prompt_only_request_with_no_response_format() {
        let server = wiremock::MockServer::start().await;
        mount_rejecting(&server, "response_format").await;

        let llm = test_llm(server.uri());
        let out = drain(&llm, Some(test_schema())).await.unwrap();

        assert_eq!(out, "{\"ok\":true}");
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            3,
            "json_schema and json_object are both rejected before the prompt-only attempt"
        );
        let last: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
        assert!(
            last.get("response_format").is_none(),
            "the terminal attempt must omit response_format entirely"
        );
    }

    // Without a schema there is no `response_format` to step down from, so a 400
    // is a real error: it must surface after exactly one request, not trigger
    // pointless identical retries.
    #[tokio::test]
    async fn a_schemaless_request_is_sent_once_and_its_error_is_surfaced() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("bad request"))
            .mount(&server)
            .await;

        let llm = test_llm(server.uri());
        let err = drain(&llm, None).await.unwrap_err();

        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("LLM server returned an error")
                && rendered.contains("400")
                && rendered.contains("bad request"),
            "the surfaced error must carry the upstream status and body: {rendered}"
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            1,
            "a non-transient 400 with no schema is not retried"
        );
    }

    // Returns `fail_status` for the first `fail_times` calls, then a 200 stream:
    // a rate-limited or briefly-overloaded endpoint that recovers.
    struct FlakyThenOk {
        fail_times: usize,
        fail_status: u16,
        seen: std::sync::atomic::AtomicUsize,
    }

    impl wiremock::Respond for FlakyThenOk {
        fn respond(&self, _request: &wiremock::Request) -> wiremock::ResponseTemplate {
            let n = self.seen.fetch_add(1, Ordering::Relaxed);
            if n < self.fail_times {
                wiremock::ResponseTemplate::new(self.fail_status).set_body_string("upstream busy")
            } else {
                wiremock::ResponseTemplate::new(200).set_body_raw(CONTENT_SSE, "text/event-stream")
            }
        }
    }

    #[tokio::test]
    async fn a_transient_upstream_error_is_retried_then_succeeds() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(FlakyThenOk {
                fail_times: 2,
                fail_status: 503,
                seen: std::sync::atomic::AtomicUsize::new(0),
            })
            .mount(&server)
            .await;

        let llm = test_llm(server.uri());
        let out = drain(&llm, None).await.unwrap();

        assert_eq!(
            out, "{\"ok\":true}",
            "content from the recovered attempt must reach the caller"
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            3,
            "two 503s are retried, the third attempt succeeds"
        );
    }

    #[tokio::test]
    async fn persistent_transient_errors_are_surfaced_with_status_after_retries() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&server)
            .await;

        let llm = test_llm(server.uri());
        let err = drain(&llm, None).await.unwrap_err();

        assert!(
            format!("{err:#}").contains("429"),
            "an exhausted transient retry must surface the upstream status: {err:#}"
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            4,
            "one initial attempt plus MAX_TRANSIENT_RETRIES (3) before giving up"
        );
    }
}
