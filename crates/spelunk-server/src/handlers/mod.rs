use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::http::HeaderMap;
use tokio::sync::mpsc;

use crate::auth::AuthContext;
use crate::{AppError, AppState, EmbedderState};

mod batch;
mod explore;
mod health;
mod index;
mod llm;
mod notes;
mod projects;
mod search;
mod sync;

pub use batch::*;
pub use explore::*;
pub use health::*;
pub use index::*;
pub use llm::*;
pub use notes::*;
pub use projects::*;
pub use search::*;
pub use sync::*;

#[cfg(test)]
mod tests;

// ── Input validation caps ─────────────────────────────────────────────────────

/// Max length (chars) for a memory entry's `title`.
pub const MAX_TITLE_LEN: usize = 500;
/// Max length (chars) for a memory entry's `body`.
pub const MAX_BODY_LEN: usize = 50_000;
/// Max length (bytes) for a `project_id` path slug (e.g. `usercise/spelunk`).
pub const MAX_SLUG_LEN: usize = 200;
/// Max number of chunks accepted in a single `/index/embed` request. Also
/// advertised in `/v1/health`'s `limits.max_batch_chunks` so a client can size
/// its calibrated batch without guessing (see `HealthResponse`).
pub const MAX_EMBED_BATCH: usize = 256;
/// Max number of entries accepted in a single `POST /memory/batch` request.
/// Matches cloud-api's cap and comfortably exceeds the CLI's own push chunk
/// size (`PUSH_BATCH_CHUNK_SIZE` in `sync.rs`), so a legitimate CLI push never
/// trips it.
pub const MAX_BATCH_ENTRIES: usize = 200;

/// Reject a title/body pair that exceeds the configured caps. Shared by every
/// handler that accepts free-text memory content (`add_note`, `supersede`'s
/// linked note content is validated at insert time, etc.).
fn validate_title_body(title: &str, body: &str) -> Result<(), AppError> {
    if title.chars().count() > MAX_TITLE_LEN {
        return Err(AppError::BadRequest(format!(
            "title exceeds maximum length of {MAX_TITLE_LEN} characters (got {})",
            title.chars().count()
        )));
    }
    if body.chars().count() > MAX_BODY_LEN {
        return Err(AppError::BadRequest(format!(
            "body exceeds maximum length of {MAX_BODY_LEN} characters (got {})",
            body.chars().count()
        )));
    }
    Ok(())
}

/// Reject an embedding vector whose length doesn't match the server's
/// configured embedding dimension. `None` (no vector supplied) always passes:
/// embedding is optional on write.
fn validate_embedding_dim(
    embedding: Option<&[f32]>,
    configured_dim: usize,
) -> Result<(), AppError> {
    if let Some(v) = embedding
        && configured_dim != 0
        && v.len() != configured_dim
    {
        return Err(AppError::BadRequest(format!(
            "embedding vector length {} does not match server's configured dimension {configured_dim}",
            v.len()
        )));
    }
    Ok(())
}

/// Reject a `project_id` path parameter that is empty or unreasonably long.
/// Project ids are human slugs (e.g. `usercise/spelunk`), not UUIDs, so this
/// is a length/sanity cap rather than a UUID-format check.
fn validate_project_slug(slug: &str) -> Result<(), AppError> {
    if slug.is_empty() {
        return Err(AppError::BadRequest("project_id must not be empty".into()));
    }
    if slug.len() > MAX_SLUG_LEN {
        return Err(AppError::BadRequest(format!(
            "project_id exceeds maximum length of {MAX_SLUG_LEN} bytes (got {})",
            slug.len()
        )));
    }
    Ok(())
}

/// Resolve the client's IP for rate-limiting: prefer the leftmost
/// `X-Forwarded-For` entry (the server sits behind a trusted proxy in team
/// deployments; see ADR-056), else the TCP peer. Falls back to a constant so
/// keyless requests share one bucket rather than bypassing the limit.
fn client_ip_key(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        let first = xff.split(',').next().unwrap_or("").trim();
        if !first.is_empty() {
            return first.to_string();
        }
    }
    match peer {
        Some(addr) => addr.ip().to_string(),
        None => "unknown".to_string(),
    }
}

/// Test-only override for the generation budget `llm_generate_with_timeout`
/// enforces (production uses `crate::REQUEST_TIMEOUT`). Lets tests inject a
/// millisecond-scale budget. `#[cfg(test)]`-gated, inert in the release binary.
#[cfg(test)]
static GENERATION_TIMEOUT_OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<Duration>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn set_generation_timeout_override(d: Duration) {
    let cell = GENERATION_TIMEOUT_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    *cell.lock().expect("override mutex poisoned") = Some(d);
}

#[cfg(test)]
fn clear_generation_timeout_override() {
    if let Some(cell) = GENERATION_TIMEOUT_OVERRIDE.get() {
        *cell.lock().expect("override mutex poisoned") = None;
    }
}

#[cfg(test)]
fn generation_timeout() -> Duration {
    GENERATION_TIMEOUT_OVERRIDE
        .get()
        .and_then(|cell| *cell.lock().expect("override mutex poisoned"))
        .unwrap_or(crate::REQUEST_TIMEOUT)
}

#[cfg(not(test))]
#[inline]
fn generation_timeout() -> Duration {
    crate::REQUEST_TIMEOUT
}

/// Run an LLM backend's `generate` call with a wall-clock budget, so a hung/slow
/// backend can't hold the spawned generation task (and the SSE connection it
/// feeds) open forever.
///
/// `/explore` and `/llm/complete` return their SSE `Response` as soon as the
/// stream is built and hand generation to a detached `tokio::spawn`, so the
/// router-level `TimeoutLayer` never sees this work. This wraps the generation
/// call with the same budget to close that gap without changing the SSE framing.
async fn llm_generate_with_timeout(
    llm: Arc<dyn spelunk_core::llm::LlmBackend>,
    messages: Vec<spelunk_core::llm::Message>,
    max_tokens: usize,
    tx: mpsc::Sender<String>,
    json_schema: Option<serde_json::Value>,
    label: &'static str,
) {
    let budget = generation_timeout();
    match tokio::time::timeout(budget, llm.generate(&messages, max_tokens, tx, json_schema)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("{label} LLM generate error: {e}"),
        Err(_elapsed) => {
            tracing::warn!(
                "{label} LLM generate exceeded the {budget:?} generation budget; aborting",
            );
            // Dropping `tx`-holding future here closes the channel; the SSE
            // stream's `rx.recv()` loop sees `None` and ends the connection
            // (with whatever partial output was already sent).
        }
    }
}

/// Build the rate-limiter bucket key for an authenticated inference request:
/// `"<principal>|<client-ip>"`. Keying on IP as well as principal means a
/// shared team API key (a single `Principal::ApiKey` string, or the empty
/// string when no key is configured at all) doesn't collapse every distinct
/// client onto one shared bucket: each caller gets its own budget.
fn rate_limit_key(auth_ctx: &AuthContext, headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    let principal = match &auth_ctx.principal {
        crate::auth::Principal::ApiKey(k) => k.clone(),
        crate::auth::Principal::User { id } => id.clone(),
    };
    let ip = client_ip_key(headers, peer);
    format!("{principal}|{ip}")
}

/// Resolve the embedder for an embed-consuming handler, translating the slot's
/// readiness into the correct HTTP error when it is not `ready`:
/// - `loading`     → `503` + `Retry-After: 5` (transient: CLI keeps polling)
/// - `unavailable` → `503` (terminal: CLI stops polling, surfaces the error)
/// - `disabled`    → `400` (permanent misconfiguration for this request)
fn require_embedder(
    state: &AppState,
    disabled_msg: &str,
) -> Result<Arc<dyn spelunk_core::embeddings::EmbeddingBackend>, AppError> {
    if let Some(backend) = state.embedder.backend() {
        return Ok(backend);
    }
    match state.embedder.state() {
        EmbedderState::Loading => {
            let detail = state
                .embedder
                .detail()
                .unwrap_or_else(|| "embedder warming up, retry shortly".to_string());
            // Log the real cause: a 503 here is the model still loading, not a
            // generic outage. Keeps the transient case out of error logs.
            tracing::debug!(%detail, "embed request rejected: embedder still loading");
            Err(AppError::EmbedderWarmingUp {
                terminal: false,
                detail,
            })
        }
        EmbedderState::Unavailable => {
            let detail = state
                .embedder
                .detail()
                .unwrap_or_else(|| "embedder failed to load".to_string());
            tracing::warn!(%detail, "embed request rejected: embedder unavailable (load failed)");
            Err(AppError::EmbedderWarmingUp {
                terminal: true,
                detail,
            })
        }
        // Disabled (or the improbable ready-but-no-backend race) → permanent 400.
        EmbedderState::Disabled | EmbedderState::Ready => {
            Err(AppError::BadRequest(disabled_msg.to_string()))
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn require_project(db: &crate::db::ServerDb, slug: &str) -> Result<crate::db::Project, AppError> {
    validate_project_slug(slug)?;
    db.get_project(slug)?.ok_or(AppError::NotFound)
}
