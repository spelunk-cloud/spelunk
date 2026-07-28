use axum::{
    Json,
    extract::{Path, State},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{AppError, AppState, ErrorBody};

use super::{require_embedder, validate_project_slug};

// ── Code search (query embedding proxy) ───────────────────────────────────────

/// Request body for `POST /v1/projects/{project_id}/search`.
#[derive(Deserialize, ToSchema)]
pub struct CodeSearchRequest {
    /// Natural-language search query.
    pub query: String,
    /// Maximum number of results the caller intends to fetch.
    /// Passed back in the response for informational purposes only: the server
    /// does not perform the KNN step; the CLI does that against its local index.
    #[serde(default = "default_search_limit")]
    pub limit: usize,
    /// Search mode: `"hybrid"` (default), `"semantic"`, or `"text"`.
    ///
    /// `"hybrid"` and `"semantic"` require an embedder; the server will return
    /// `query_vector` so the CLI can run KNN against its local SQLite index.
    /// `"text"` skips embedding and signals the CLI to use FTS.
    #[serde(default = "default_search_mode")]
    pub mode: String,
}
fn default_search_limit() -> usize {
    10
}
fn default_search_mode() -> String {
    "hybrid".to_string()
}

/// Response body for `POST /v1/projects/{project_id}/search`.
#[derive(Serialize, ToSchema)]
pub struct CodeSearchResponse {
    /// Mode actually used (`"semantic"`, `"hybrid"`, or `"text"`).
    /// May differ from the requested mode if the embedder is unavailable.
    pub mode: String,
    /// Query embedding vector: present for semantic/hybrid modes.
    /// The CLI uses this to run KNN against its local index.
    /// `null` when mode is `"text"` (no embedding needed).
    pub query_vector: Option<Vec<f32>>,
}

/// Embed a search query server-side and return the vector for the CLI to use
/// in its local KNN search.
///
/// The server applies the F2LLM code-retrieval query prefix
/// (`Instruct: Given a code search query…\nQuery: {q}`) so the CLI does not
/// need to know the embedding format. The server does **not** perform the KNN
/// step: the local SQLite index lives on the CLI side.
///
/// - `"semantic"` / `"hybrid"`: embeds the query and returns `query_vector`.
///   Returns **400** if no embedder is configured on this server.
/// - `"text"`: returns `query_vector: null`; the CLI falls back to FTS.
#[utoipa::path(
    post,
    path = "/v1/projects/{project_id}/search",
    params(
        ("project_id" = String, Path, description = "Project slug"),
    ),
    request_body = CodeSearchRequest,
    responses(
        (status = 200, description = "Query vector (CLI runs KNN locally)", body = CodeSearchResponse),
        (status = 400, description = "No embedder configured or invalid mode", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 429, description = "Embed admission queue full; retry after the given delay", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "search"
)]
pub async fn project_search(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(body): Json<CodeSearchRequest>,
) -> Result<impl IntoResponse, AppError> {
    validate_project_slug(&project_id)?;
    let mode = body.mode.as_str();

    // Validate mode.
    if !matches!(mode, "hybrid" | "semantic" | "text") {
        return Err(AppError::BadRequest(format!(
            "invalid mode '{mode}'; must be one of: hybrid, semantic, text"
        )));
    }

    // Text mode: no embedding needed.
    if mode == "text" {
        return Ok(Json(CodeSearchResponse {
            mode: "text".to_string(),
            query_vector: None,
        }));
    }

    // Semantic / hybrid: require an embedder.
    let embedder = require_embedder(
        &state,
        "semantic/hybrid search requires an embedder, but this server was built \
         without the native embedder (embed-native feature); use mode=text.",
    )?;

    // Admission control: a query embed sharing the mutex-serialized
    // embedder with a running `/index/embed` batch must not queue
    // silently behind it until the client's own timeout fires (the observed
    // symptom: `search` reporting "no results" against a live-but-busy
    // server). Shed with 429 instead once the bounded queue is full.
    let _admission = state.embed_admission.try_acquire()?;

    // F2LLM-v2-330M query prefix: instruction + query. Documents are embedded
    // without a prefix; queries must use this format for correct retrieval.
    let query_text = format!(
        "Instruct: Given a code search query, retrieve the relevant code snippets\nQuery: {}",
        body.query
    );
    let vecs = embedder
        .embed(&[query_text.as_str()])
        .await
        .map_err(AppError::Internal)?;
    let query_vector = vecs
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("embedder returned no vectors")))?;

    Ok(Json(CodeSearchResponse {
        mode: mode.to_string(),
        query_vector: Some(query_vector),
    }))
}
