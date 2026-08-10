use axum::{Json, extract::State, response::IntoResponse};
use serde::Serialize;
use utoipa::ToSchema;

use crate::{AppState, EmbedderState};

use super::MAX_EMBED_BATCH;

// ── Health ────────────────────────────────────────────────────────────────────

/// Embedder readiness reported inside the health body.
///
/// Liveness (`status: "ok"`) is independent of this: `/v1/health` returns `200`
/// the instant the listener binds, and this sub-object reports whether the
/// embedder is `loading`, `ready`, `unavailable` (load failed), or `disabled`.
#[derive(Serialize, ToSchema)]
pub struct EmbedderStatus {
    /// Readiness of the server-side embedder.
    pub state: EmbedderState,
    /// Optional human-readable detail (e.g. the load-failure summary while
    /// `unavailable`). `null` when not useful.
    pub detail: Option<String>,
}

/// Server-enforced limits relevant to sizing an `/index/embed` request, so a
/// client can clamp its batching to what this server build supports.
///
/// Absent `limits` (an older server pre-dating this field) means "assume the
/// legacy 30s / no-embed-exemption profile", not "unlimited".
#[derive(Serialize, ToSchema)]
pub struct ServerLimits {
    /// Wall-clock budget (seconds) this server allows a single `/index/embed`
    /// request before returning `408`. A client should keep its per-request
    /// batch's *expected* duration comfortably under this, not just under its
    /// own client-side timeout: the server will cut it off regardless of
    /// what the client is willing to wait for.
    pub embed_request_timeout_secs: u64,
    /// Max chunks accepted in a single `/index/embed` request (`413` above
    /// this). Mirrors `MAX_EMBED_BATCH`.
    pub max_batch_chunks: usize,
    /// Per-chunk token truncation cap the embedder enforces, if known (native
    /// backend only, see `EmbeddingBackend::token_cap`). `null` when the
    /// embedder isn't ready or exposes none (e.g. a test-only mock backend).
    /// Informational: the binding constraint in practice is wall-clock time,
    /// not per-batch memory.
    pub embedder_token_cap: Option<usize>,
}

/// Server capabilities reported in the health response.
#[derive(Serialize, ToSchema)]
pub struct HealthResponse {
    /// Always `"ok"`: liveness marker, independent of embedder readiness.
    pub status: &'static str,
    /// Server version string.
    pub version: &'static str,
    /// List of feature capabilities supported by this server instance.
    /// `index.embed` / `search.semantic` are advertised **only** when the
    /// embedder is `ready`, so pre-readiness clients that key off `capabilities`
    /// keep working (they see "no semantic yet").
    pub capabilities: Vec<String>,
    /// Persistent UUID v7 identifying this server instance across restarts.
    pub instance_id: String,
    /// Effective UID of the process that started the server (Unix); `null` on Windows.
    pub started_by: Option<u32>,
    /// Embedding dimension produced by this server's embedder.
    /// `0` until the embedder is `ready` (capability `index.embed` absent).
    pub embedding_dim: usize,
    /// Embedder readiness. Newer clients read `embedder.state` for the finer
    /// `loading` vs `unavailable` distinction that `capabilities` cannot express.
    pub embedder: EmbedderStatus,
    /// Server-enforced operative limits for `/index/embed` batch sizing. See
    /// [`ServerLimits`]. Absent on servers that pre-date this field; treat
    /// that as the legacy 30s/no-exemption profile, not "no limit".
    pub limits: ServerLimits,
}

/// Server liveness check. No authentication required.
///
/// Returns `200` the instant the listener is bound, regardless of embedder
/// state: it never blocks on, and never fails because of, model loading.
/// Returns server version, capabilities, identity fields, and the embedder
/// readiness sub-object.
#[utoipa::path(
    get,
    path = "/v1/health",
    responses(
        (status = 200, description = "Server is up", body = HealthResponse)
    ),
    tag = "health"
)]
pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let embedder_state = state.embedder.state();
    // A ready backend (native model loaded) is the only one that can serve
    // embeddings, so advertise the semantic caps and a non-zero dim only then.
    let ready_backend = state.embedder.backend();

    let mut capabilities = vec!["memory".to_string()];
    if let Some(backend) = &ready_backend {
        capabilities.push("index.embed".to_string());
        capabilities.push("search.semantic".to_string());
        let _ = backend; // dim read below
    }
    if state.llm.is_some() {
        capabilities.push("explore".to_string());
        capabilities.push("llm.complete".to_string());
    }
    let embedding_dim = ready_backend.as_ref().map_or(0, |e| e.dimension());
    let embedder_token_cap = ready_backend.as_ref().and_then(|e| e.token_cap());
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        capabilities,
        instance_id: state.instance_id.clone(),
        started_by: state.started_by,
        embedding_dim,
        embedder: EmbedderStatus {
            state: embedder_state,
            detail: state.embedder.detail(),
        },
        limits: ServerLimits {
            embed_request_timeout_secs: crate::EMBED_REQUEST_TIMEOUT.as_secs(),
            max_batch_chunks: MAX_EMBED_BATCH,
            embedder_token_cap,
        },
    })
}
