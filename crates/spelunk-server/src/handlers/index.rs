use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{AppError, AppState, ErrorBody};

use super::{MAX_EMBED_BATCH, require_embedder, validate_project_slug};

// ── Index / embed ─────────────────────────────────────────────────────────────

/// A single chunk to embed.
#[derive(Deserialize, ToSchema)]
pub struct EmbedChunkIn {
    /// Opaque CLI-assigned identifier (e.g. blake3 hash of file + offset). Echoed back verbatim.
    pub chunk_id: String,
    /// Raw text content to embed.
    pub content: String,
}

/// Embedding result for a single chunk.
#[derive(Serialize, ToSchema)]
pub struct EmbedChunkOut {
    /// The same `chunk_id` that was sent in the request.
    pub chunk_id: String,
    /// Embedding vector produced by the server.
    pub vector: Vec<f32>,
}

/// Request body for `POST /v1/projects/{project_id}/index/embed`.
#[derive(Deserialize, ToSchema)]
pub struct EmbedRequest {
    /// Chunks to embed. Maximum 256 per request.
    pub chunks: Vec<EmbedChunkIn>,
}

/// Response body for `POST /v1/projects/{project_id}/index/embed`.
#[derive(Serialize, ToSchema)]
pub struct EmbedResponse {
    pub chunks: Vec<EmbedChunkOut>,
}

/// Observability guard for an in-flight `/index/embed` call (GH#631 /
/// GH#631). Created armed right before the `embed_with_cancel` await and
/// disarmed right after it returns. If the surrounding handler future is
/// dropped while still armed  -  client disconnect or the router's
/// `TimeoutLayer` firing a 408, both of which drop the handler future rather
/// than running it to completion  -  `Drop` fires instead: it flips the shared
/// cancellation flag (which `embed_with_cancel` polls from inside its detached
/// `spawn_blocking` task, the only way to reach in there) and logs the
/// abandonment, since today the server otherwise cannot distinguish a slow
/// client from a gone one.
pub(crate) struct EmbedAbandonGuard {
    pub(crate) cancel: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) armed: bool,
    pub(crate) project_id: String,
    pub(crate) batch_size: usize,
    pub(crate) started: std::time::Instant,
}

impl Drop for EmbedAbandonGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::info!(
                "embed request abandoned: project={} batch_size={} elapsed={:?} \
                 (client disconnected or server-side timeout fired before the embed \
                 call returned)",
                self.project_id,
                self.batch_size,
                self.started.elapsed(),
            );
        }
    }
}

/// Generate embeddings for code chunks. The server encodes each chunk and returns the
/// vectors. **The server does not store the vectors**: the CLI is the only persistent
/// store for index data.
///
/// Returns 400 if no embedder is configured.
/// Returns 413 if the batch exceeds 256 chunks.
/// Returns 429 (with `Retry-After`) if the embed admission queue is full.
#[utoipa::path(
    post,
    path = "/v1/projects/{project_id}/index/embed",
    params(
        ("project_id" = String, Path, description = "Project slug"),
    ),
    request_body = EmbedRequest,
    responses(
        (status = 200, description = "Embedding vectors as raw little-endian f32 bytes, row-major [n_chunks x dim] in request order (not stored server-side)", content_type = "application/octet-stream"),
        (status = 400, description = "No embedder configured", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 429, description = "Embed admission queue full; retry after the given delay", body = ErrorBody),
        (status = 413, description = "Batch exceeds 256 chunks", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "index"
)]
pub async fn index_embed(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(body): Json<EmbedRequest>,
) -> Result<Response, AppError> {
    validate_project_slug(&project_id)?;

    // Check batch size first so clients get a 413 even when no embedder is configured.
    if body.chunks.len() > MAX_EMBED_BATCH {
        return Ok((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ErrorBody::new(
                "bad_request",
                &format!(
                    "Batch size {} exceeds maximum of {MAX_EMBED_BATCH} chunks per request.",
                    body.chunks.len()
                ),
            )),
        )
            .into_response());
    }

    let embedder = require_embedder(
        &state,
        "index.embed requires an embedder, but this server was built without the \
         native embedder (embed-native feature).",
    )?;

    if body.chunks.is_empty() {
        return Ok(octet_stream(Vec::new()));
    }

    // Admission control: the embedder is mutex-serialized and processes
    // one request at a time, so a saturated index run must not
    // let this request join an unbounded wait behind it. Shed with `429`
    // immediately if the bounded queue is already full, rather than parking
    // as another blocking-pool thread on the mutex. Held for the whole embed
    // call so the permit only frees up once this request's turn is done.
    let _admission = state.embed_admission.try_acquire()?;

    // Collect texts, preserving order for reassembly.
    let texts: Vec<&str> = body.chunks.iter().map(|c| c.content.as_str()).collect();

    // Cancellation seam (GH#631): if this handler's future is
    // dropped mid-embed  -  client disconnect or the router's `TimeoutLayer`
    // firing a 408  -  `cancel_guard` drops while still armed and flips
    // `cancel_flag`, which `embed_with_cancel` polls from inside its detached
    // `spawn_blocking` task (a plain `.await` drop does not otherwise reach in
    // there). Disarmed once the embed call returns on its own, so an ordinary
    // completed request (success or a real embed error) never logs abandonment.
    let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut cancel_guard = EmbedAbandonGuard {
        cancel: Arc::clone(&cancel_flag),
        armed: true,
        project_id: project_id.clone(),
        batch_size: body.chunks.len(),
        started: std::time::Instant::now(),
    };
    let embed_result = embedder.embed_with_cancel(&texts, cancel_flag).await;
    cancel_guard.armed = false;
    let vectors = embed_result.map_err(AppError::Internal)?;

    if vectors.len() != body.chunks.len() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "Embedder returned {} vectors for {} chunks",
            vectors.len(),
            body.chunks.len()
        )));
    }

    // Serialise as raw little-endian f32 bytes, one vector after another in
    // request order. Avoids the per-element JSON float cost on both ends; the
    // client maps response[i] → request chunk[i] by position, so no chunk_id
    // framing is needed.
    let dim = vectors.first().map_or(0, Vec::len);
    let mut body_bytes = Vec::with_capacity(vectors.len() * dim * 4);
    for v in &vectors {
        for f in v {
            body_bytes.extend_from_slice(&f.to_le_bytes());
        }
    }
    // Data promise: vectors are NOT stored on the server. We return them directly.
    Ok(octet_stream(body_bytes))
}

/// Build a `200 OK` response carrying raw bytes as `application/octet-stream`.
fn octet_stream(bytes: Vec<u8>) -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        bytes,
    )
        .into_response()
}
