//! The server's OpenAI-compatible LLM backend, plus resolution and validation
//! of the credential it authenticates with.
//!
//! The credential never comes from a keychain: this process is commonly a
//! detached daemon with no user session, so the spawning CLI resolves the
//! value and passes it in via `SPELUNK_LLM_KEY` (or, for an operator running
//! the binary directly, `--llm-key-file`).

use anyhow::{Context, Result};

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
pub struct ServerLlm {
    pub client: reqwest::Client,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub reasoning_effort: Option<String>,
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
            messages: Vec<ChatMsg<'a>>,
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

        let response_format =
            json_schema.map(|s| serde_json::json!({ "type": "json_schema", "json_schema": s }));

        let req = ChatReq {
            model: &self.model,
            messages: chat_messages,
            stream: true,
            max_tokens,
            temperature: 0.7,
            reasoning_effort: self.reasoning_effort.as_deref(),
            response_format,
        };

        let mut builder = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url));
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }

        let mut stream = builder
            .json(&req)
            .send()
            .await
            .context("calling LLM server")?
            .error_for_status()
            .context("LLM server returned an error")?
            .bytes_stream();

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
}
