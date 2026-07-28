use std::convert::Infallible;
use std::net::SocketAddr;

use async_stream::stream;
use axum::{
    Extension, Json,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use serde::Deserialize;
use tokio::sync::mpsc;
use utoipa::ToSchema;

use crate::auth::AuthContext;
use crate::{AppError, AppState, ErrorBody};

use super::{llm_generate_with_timeout, rate_limit_key, validate_project_slug};

// ── Explore (SSE) ─────────────────────────────────────────────────────────────

/// A single context chunk supplied by the CLI for `/explore`.
#[derive(Deserialize, ToSchema)]
pub struct ExploreContextChunk {
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub content: String,
}

/// Request body for `POST /v1/projects/{project_id}/explore`.
#[derive(Deserialize, ToSchema)]
pub struct ExploreRequest {
    pub question: String,
    #[serde(default)]
    pub context_chunks: Vec<ExploreContextChunk>,
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
}
fn default_max_turns() -> usize {
    5
}

/// Run an LLM reasoning loop over caller-supplied context chunks.
/// The CLI retrieves relevant chunks from its local index and sends them alongside
/// the question. **The server does not store context chunks.**
///
/// Returns an SSE stream with events: `thought`, `answer`, `done`, `error`.
/// Returns 503 if no LLM is configured.
#[utoipa::path(
    post,
    path = "/v1/projects/{project_id}/explore",
    params(
        ("project_id" = String, Path, description = "Project slug"),
    ),
    request_body = ExploreRequest,
    responses(
        (status = 200, description = "SSE stream: thought/answer/done/error events"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 429, description = "Rate limit exceeded", body = ErrorBody),
        (status = 503, description = "No LLM configured", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "inference"
)]
pub async fn explore(
    State(state): State<AppState>,
    Extension(auth_ctx): Extension<AuthContext>,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Path(project_id): Path<String>,
    Json(body): Json<ExploreRequest>,
) -> Result<Response, AppError> {
    validate_project_slug(&project_id)?;

    // ── Rate limit ────────────────────────────────────────────────────────────
    // Same token-burn exposure as `/llm/complete` (up to `2048 * max_turns`
    // generated tokens per call): key on client IP (not just principal) so a
    // shared team key can't be used to bypass the limit from many clients.
    let rate_key = rate_limit_key(
        &auth_ctx,
        &headers,
        connect_info.map(|Extension(ConnectInfo(addr))| addr),
    );
    if state.rate_limiter.check(&rate_key).is_err() {
        return Ok((
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorBody::new(
                "rate_limited",
                "Rate limit exceeded. Slow down and retry.",
            )),
        )
            .into_response());
    }

    let llm = state.llm.clone().ok_or_else(|| {
        AppError::ServiceUnavailable(
            "This server has no LLM configured. Set SPELUNK_LLM_URL and SPELUNK_LLM_MODEL."
                .to_string(),
        )
    })?;

    // Build context block from provided chunks.
    let context_text = if body.context_chunks.is_empty() {
        "(no context provided)".to_string()
    } else {
        body.context_chunks
            .iter()
            .map(|c| {
                format!(
                    "// {}:{}-{}\n{}",
                    c.file, c.start_line, c.end_line, c.content
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n")
    };

    let system_prompt = "You are a code-exploration assistant. \
        Analyse the supplied code context and answer the user's question step by step. \
        Emit your intermediate reasoning as thoughts, then a final answer. \
        Format: emit lines like JSON objects with 'kind' and 'content' fields.";

    let user_prompt = format!(
        "<code_context>\n{context_text}\n</code_context>\n\n\
         <question>\n{question}\n</question>\n\n\
         Respond with a series of JSON objects (one per line), each with \
         {{\"kind\": \"thought\", \"content\": \"...\"}} or \
         {{\"kind\": \"answer\", \"content\": \"...\"}}. \
         End with {{\"kind\": \"done\"}}.",
        question = body.question
    );

    let messages = vec![
        spelunk_core::llm::Message::system(system_prompt),
        spelunk_core::llm::Message::user(user_prompt),
    ];

    let (tx, mut rx) = mpsc::channel::<String>(64);
    let max_tokens = 2048 * body.max_turns.min(10);

    // Spawn LLM generation into a background task, bounded by the same
    // budget as `REQUEST_TIMEOUT`: see `llm_generate_with_timeout` for why
    // the router's `TimeoutLayer` alone doesn't cover this.
    tokio::spawn(llm_generate_with_timeout(
        llm, messages, max_tokens, tx, None, "explore",
    ));

    // Stream tokens as SSE events. We buffer tokens into lines and emit each
    // complete JSON object as a separate SSE event.
    let s = stream! {
        let mut buffer = String::new();
        while let Some(token) = rx.recv().await {
            buffer.push_str(&token);
            // Emit complete lines as SSE events.
            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim().to_string();
                buffer.drain(..newline_pos + 1);
                if line.is_empty() {
                    continue;
                }
                yield Ok::<Event, Infallible>(Event::default().data(line));
            }
        }
        // Flush remaining buffer content.
        let remaining = buffer.trim().to_string();
        if !remaining.is_empty() {
            yield Ok::<Event, Infallible>(Event::default().data(remaining));
        }
        // Terminal event.
        yield Ok::<Event, Infallible>(
            Event::default().data(r#"{"kind":"done"}"#)
        );
    };

    Ok(Sse::new(s).keep_alive(KeepAlive::default()).into_response())
}
