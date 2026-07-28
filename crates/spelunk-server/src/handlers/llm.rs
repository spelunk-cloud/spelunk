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

// ── LLM complete (generic primitive) ─────────────────────────────────────────

/// A single chat message for `/llm/complete`.
#[derive(Deserialize, ToSchema)]
pub struct LlmCompleteMessage {
    /// Role: `system`, `user`, or `assistant`.
    pub role: String,
    pub content: String,
}

/// Request body for `POST /v1/projects/{project_id}/llm/complete`.
#[derive(Deserialize, ToSchema)]
pub struct LlmCompleteRequest {
    /// Non-empty list of chat messages.
    pub messages: Vec<LlmCompleteMessage>,
    /// Desired max completion tokens. The server clamps this to its configured ceiling.
    pub max_tokens: usize,
    /// Optional OpenAI-style `response_format.json_schema` for structured output.
    pub json_schema: Option<serde_json::Value>,
}

/// Run a single LLM completion over caller-supplied messages. Streaming SSE.
///
/// The server performs no orchestration, adds no system prompt, and stores nothing.
/// Client-supplied `max_tokens` is clamped server-side to the configured ceiling.
///
/// **Auth:** `Authorization: Bearer` required (Tier 1).
///
/// **SSE event shapes:**
/// - `{"kind":"token","content":"..."}`: one streamed fragment
/// - `{"kind":"done"}`: terminal success
/// - `{"kind":"error","code":"...","message":"..."}`: terminal failure mid-stream
///
/// Returns 503 if no LLM is configured on this server.
#[utoipa::path(
    post,
    path = "/v1/projects/{project_id}/llm/complete",
    params(
        ("project_id" = String, Path, description = "Project slug")
    ),
    request_body = LlmCompleteRequest,
    responses(
        (status = 200, description = "SSE stream: token/done/error events"),
        (status = 400, description = "messages empty or max_tokens ≤ 0", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 413, description = "Request body too large", body = ErrorBody),
        (status = 429, description = "Rate limit exceeded", body = ErrorBody),
        (status = 503, description = "No LLM configured", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "inference"
)]
pub async fn llm_complete(
    State(state): State<AppState>,
    Extension(auth_ctx): Extension<AuthContext>,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Path(project_id): Path<String>,
    Json(body): Json<LlmCompleteRequest>,
) -> Result<Response, AppError> {
    validate_project_slug(&project_id)?;

    // ── Validate request ──────────────────────────────────────────────────────
    if body.messages.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(ErrorBody::new("bad_request", "messages must not be empty")),
        )
            .into_response());
    }
    if body.max_tokens == 0 {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(ErrorBody::new("bad_request", "max_tokens must be > 0")),
        )
            .into_response());
    }

    // ── Rate limit ────────────────────────────────────────────────────────────
    // Keyed on principal + client IP (not principal alone) so a shared team
    // key doesn't collapse every distinct caller onto one bucket.
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

    // ── Clamp max_tokens server-side (never trust client upward) ─────────────
    let max_tokens = body.max_tokens.min(state.max_tokens_ceiling);

    // ── LLM availability ──────────────────────────────────────────────────────
    let llm = state.llm.clone().ok_or_else(|| {
        AppError::ServiceUnavailable(
            "llm.complete requires an LLM backend. \
             Configure the chat model on the server (--llm-url / SPELUNK_LLM_URL)."
                .to_string(),
        )
    })?;

    // ── Convert messages ──────────────────────────────────────────────────────
    let messages: Vec<spelunk_core::llm::Message> = body
        .messages
        .iter()
        .map(|m| spelunk_core::llm::Message {
            role: m.role.clone(),
            content: m.content.clone(),
        })
        .collect();

    let json_schema = body.json_schema;

    // ── Spawn LLM generation ──────────────────────────────────────────────────
    // Bounded by the same budget as `REQUEST_TIMEOUT`: see
    // `llm_generate_with_timeout` for why the router's `TimeoutLayer` alone
    // doesn't cover this endpoint.
    let (tx, mut rx) = mpsc::channel::<String>(64);
    tokio::spawn(llm_generate_with_timeout(
        llm,
        messages,
        max_tokens,
        tx,
        json_schema,
        "llm_complete",
    ));

    // ── Stream tokens as SSE ─────────────────────────────────────────────────
    let s = stream! {
        while let Some(token) = rx.recv().await {
            let data = serde_json::json!({"kind": "token", "content": token}).to_string();
            yield Ok::<Event, Infallible>(Event::default().data(data));
        }
        yield Ok::<Event, Infallible>(
            Event::default().data(r#"{"kind":"done"}"#)
        );
    };

    Ok(Sse::new(s).keep_alive(KeepAlive::default()).into_response())
}
