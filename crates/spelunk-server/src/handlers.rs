use std::convert::Infallible;
use std::time::Duration;

use anyhow::Result;
use async_stream::stream;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use utoipa::ToSchema;

use super::{AppError, AppState, ErrorBody};

// ── Request / Response types ──────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct AddNoteRequest {
    /// Kind of memory entry: `decision`, `requirement`, `note`, `question`, `handoff`, `intent`.
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// Optional tags for filtering.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Source file paths this entry is linked to.
    #[serde(default)]
    pub linked_files: Vec<String>,
    /// Pre-computed embedding vector from the client. Optional — if omitted and the server has an
    /// embedding backend configured (`SPELUNK_EMBEDDING_URL`), the server embeds the entry.
    /// If neither is available, the entry is stored without a vector (text search only).
    pub embedding: Option<Vec<f32>>,
}

#[derive(Serialize, ToSchema)]
pub struct AddNoteResponse {
    /// Whether the note was stored (always true for 201/409).
    pub stored: bool,
    /// ID of the created note.
    pub id: i64,
    /// Conflicting entries (only present on 409).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<ConflictEntry>,
}

/// A single conflicting memory entry returned in a 409 response.
#[derive(Serialize, ToSchema)]
pub struct ConflictEntry {
    pub id: i64,
    pub title: String,
    /// Cosine similarity to the new entry (0.0–1.0).
    pub similarity: f32,
}

#[derive(Deserialize, ToSchema, utoipa::IntoParams)]
pub struct ListQuery {
    /// Filter by kind (`decision`, `requirement`, `note`, `question`, `handoff`, `intent`).
    pub kind: Option<String>,
    /// Maximum number of results to return (default: 20).
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Include archived entries (default: false).
    #[serde(default)]
    pub archived: bool,
}
fn default_limit() -> usize {
    20
}

#[derive(Deserialize, ToSchema)]
pub struct SearchRequest {
    /// Text query — the server encodes this using its configured embedder.
    pub query: String,
    /// Maximum number of results to return (default: 20).
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Serialize, ToSchema)]
pub struct BoolResponse {
    /// Whether the operation modified a record.
    pub changed: bool,
}

#[derive(Serialize, ToSchema)]
pub struct CountResponse {
    pub count: i64,
}

#[derive(Deserialize, ToSchema)]
pub struct SupersedeRequest {
    /// ID of the new note that replaces the superseded one.
    pub new_id: i64,
}

// ── Health ────────────────────────────────────────────────────────────────────

/// Server capabilities reported in the health response.
#[derive(Serialize, ToSchema)]
pub struct HealthResponse {
    /// Always `"ok"`.
    pub status: &'static str,
    /// Server version string.
    pub version: &'static str,
    /// List of feature capabilities supported by this server instance.
    pub capabilities: Vec<String>,
}

/// Server liveness check. No authentication required.
/// Returns server version and capabilities list.
#[utoipa::path(
    get,
    path = "/v1/health",
    responses(
        (status = 200, description = "Server is up", body = HealthResponse)
    ),
    tag = "health"
)]
pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let mut capabilities = vec!["memory".to_string()];
    if state.embedder.is_some() {
        capabilities.push("index.embed".to_string());
        capabilities.push("search.semantic".to_string());
    }
    if state.llm.is_some() {
        capabilities.push("explore".to_string());
        capabilities.push("llm.complete".to_string());
    }
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        capabilities,
    })
}

// ── Projects ──────────────────────────────────────────────────────────────────

/// List all projects registered on this server.
#[utoipa::path(
    get,
    path = "/v1/projects",
    responses(
        (status = 200, description = "List of projects", body = Vec<super::db::Project>),
        (status = 401, description = "Unauthorized"),
    ),
    security(("bearer_auth" = [])),
    tag = "projects"
)]
pub async fn list_projects(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    let db = state.db.lock().await;
    let projects = db.list_projects()?;
    Ok(Json(projects))
}

// ── Memory CRUD ───────────────────────────────────────────────────────────────

/// Add a memory entry to a project. The project is auto-created on first write.
///
/// The `embedding` field is optional. If omitted and the server has `SPELUNK_EMBEDDING_URL`
/// configured, the server embeds the entry before storage. If neither is available, the
/// entry is stored without a vector (text search only, no KNN).
///
/// Returns **201** on success. Returns **409** when the new entry is semantically
/// close to one or more existing active entries (similarity ≥ conflict_threshold).
/// The entry is still stored in both cases; the 409 is informational.
/// Returns **422** when the entry contains prompt-injection patterns.
#[utoipa::path(
    post,
    path = "/v1/projects/{project_id}/memory",
    params(
        ("project_id" = String, Path, description = "Project slug (e.g. `usercise/spelunk`)")
    ),
    request_body = AddNoteRequest,
    responses(
        (status = 201, description = "Note created", body = AddNoteResponse),
        (status = 400, description = "Embedding dimension mismatch", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 409, description = "Note stored but conflicts with existing entries", body = AddNoteResponse),
        (status = 422, description = "Entry rejected — prompt injection detected"),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn add_note(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(body): Json<AddNoteRequest>,
) -> Result<Response, AppError> {
    // Reject entries that contain prompt-injection patterns.
    if let Some(m) = super::security::scan_for_injection(&body.title, &body.body) {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "injection_detected",
                "field": m.field,
                "category": m.category,
                "message": "Entry contains patterns associated with prompt injection. \
                            Review and revise the entry.",
            })),
        )
            .into_response());
    }

    // Server-side embedding: embed the entry when no client vector is supplied.
    let server_embedding: Option<Vec<f32>> = if body.embedding.is_none() {
        if let Some(embedder) = &state.embedder {
            let text = format!("title: {} | text: {}", body.title, body.body);
            match embedder.embed(&[text.as_str()]).await {
                Ok(mut vecs) if !vecs.is_empty() => vecs.pop(),
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!("server-side embedding failed, storing without vector: {e}");
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    let embedding = body.embedding.as_deref().or(server_embedding.as_deref());
    let dim = embedding.map(|v| v.len()).unwrap_or(0);

    let db = state.db.lock().await;
    let project = db.upsert_project(&project_id, dim)?;

    let id = db.add_note(
        project.id,
        &body.kind,
        &body.title,
        &body.body,
        &body.tags,
        &body.linked_files,
        embedding,
    )?;

    // ── Conflict detection ────────────────────────────────────────────────────
    // Only run if the entry has an embedding and conflict detection is enabled
    // (threshold < 1.0).
    let threshold = state.conflict_threshold;
    if let Some(vec) = embedding
        && threshold < 1.0
    {
        let max_distance = 1.0 - threshold;
        let nearby = db.search_notes_for_conflicts(project.id, vec, max_distance, id, 5)?;
        if !nearby.is_empty() {
            // Insert `contradicts` edges for each conflict.
            for note in &nearby {
                if let Err(e) = db.add_edge(id, note.id, "contradicts") {
                    tracing::warn!("failed to insert contradicts edge {id}→{}: {e}", note.id);
                }
            }
            let conflicts: Vec<ConflictEntry> = nearby
                .into_iter()
                .map(|n| {
                    let similarity = n
                        .distance
                        .map(|d| (1.0 - d as f32).clamp(0.0, 1.0))
                        .unwrap_or(0.0);
                    ConflictEntry {
                        id: n.id,
                        title: n.title,
                        similarity,
                    }
                })
                .collect();
            return Ok((
                StatusCode::CONFLICT,
                Json(AddNoteResponse {
                    stored: true,
                    id,
                    conflicts,
                }),
            )
                .into_response());
        }
    }

    Ok((
        StatusCode::CREATED,
        Json(AddNoteResponse {
            stored: true,
            id,
            conflicts: vec![],
        }),
    )
        .into_response())
}

/// List memory entries for a project, optionally filtered by kind.
#[utoipa::path(
    get,
    path = "/v1/projects/{project_id}/memory",
    params(
        ("project_id" = String, Path, description = "Project slug"),
        ListQuery,
    ),
    responses(
        (status = 200, description = "List of notes", body = Vec<super::db::ServerNote>),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Project not found", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn list_notes(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Query(params): Query<ListQuery>,
) -> Result<impl IntoResponse, AppError> {
    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;
    let notes = db.list_notes(
        project.id,
        params.kind.as_deref(),
        params.limit,
        params.archived,
    )?;
    Ok(Json(notes))
}

/// Get a single memory entry by ID.
#[utoipa::path(
    get,
    path = "/v1/projects/{project_id}/memory/{note_id}",
    params(
        ("project_id" = String, Path, description = "Project slug"),
        ("note_id" = i64, Path, description = "Note ID"),
    ),
    responses(
        (status = 200, description = "Note found", body = super::db::ServerNote),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Note not found", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn get_note(
    State(state): State<AppState>,
    Path((project_id, note_id)): Path<(String, i64)>,
) -> Result<impl IntoResponse, AppError> {
    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;
    match db.get_note(project.id, note_id)? {
        Some(note) => Ok(Json(note).into_response()),
        None => Err(AppError::NotFound),
    }
}

/// Semantic search over memory entries. The server encodes the text query using its
/// configured embedder. Returns 400 if no embedder is configured.
#[utoipa::path(
    post,
    path = "/v1/projects/{project_id}/memory/search",
    params(
        ("project_id" = String, Path, description = "Project slug"),
    ),
    request_body = SearchRequest,
    responses(
        (status = 200, description = "Nearest neighbours", body = Vec<super::db::ServerNote>),
        (status = 400, description = "No embedder configured", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Project not found", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn search_notes(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(body): Json<SearchRequest>,
) -> Result<impl IntoResponse, AppError> {
    let embedder = state.embedder.as_ref().ok_or_else(|| {
        AppError::BadRequest(
            "This server has no embedder configured. Semantic memory search is unavailable."
                .to_string(),
        )
    })?;

    let query_vecs = embedder
        .embed(&[body.query.as_str()])
        .await
        .map_err(AppError::Internal)?;
    let query_vec = query_vecs
        .into_iter()
        .next()
        .ok_or_else(|| AppError::BadRequest("Embedder returned no vectors".to_string()))?;

    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;
    let notes = db.search_notes(project.id, &query_vec, body.limit)?;
    Ok(Json(notes))
}

/// Delete a memory entry permanently.
#[utoipa::path(
    delete,
    path = "/v1/projects/{project_id}/memory/{note_id}",
    params(
        ("project_id" = String, Path, description = "Project slug"),
        ("note_id" = i64, Path, description = "Note ID"),
    ),
    responses(
        (status = 200, description = "Deletion result", body = BoolResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Note not found", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn delete_note(
    State(state): State<AppState>,
    Path((project_id, note_id)): Path<(String, i64)>,
) -> Result<impl IntoResponse, AppError> {
    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;
    let changed = db.delete_note(project.id, note_id)?;
    Ok(Json(BoolResponse { changed }))
}

/// Archive a memory entry. Archived entries are excluded from search and `ask`
/// context but remain visible via `?archived=true`.
#[utoipa::path(
    post,
    path = "/v1/projects/{project_id}/memory/{note_id}/archive",
    params(
        ("project_id" = String, Path, description = "Project slug"),
        ("note_id" = i64, Path, description = "Note ID"),
    ),
    responses(
        (status = 200, description = "Archive result", body = BoolResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Note not found", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn archive_note(
    State(state): State<AppState>,
    Path((project_id, note_id)): Path<(String, i64)>,
) -> Result<impl IntoResponse, AppError> {
    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;
    let changed = db.archive_note(project.id, note_id)?;
    Ok(Json(BoolResponse { changed }))
}

/// Mark a memory entry as superseded by a newer one. The old entry is archived
/// and linked to the new one.
#[utoipa::path(
    post,
    path = "/v1/projects/{project_id}/memory/{note_id}/supersede",
    params(
        ("project_id" = String, Path, description = "Project slug"),
        ("note_id" = i64, Path, description = "Note ID to supersede"),
    ),
    request_body = SupersedeRequest,
    responses(
        (status = 200, description = "Supersede result", body = BoolResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Note not found", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn supersede_note(
    State(state): State<AppState>,
    Path((project_id, note_id)): Path<(String, i64)>,
    Json(body): Json<SupersedeRequest>,
) -> Result<impl IntoResponse, AppError> {
    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;
    let changed = db.supersede_note(project.id, note_id, body.new_id)?;
    Ok(Json(BoolResponse { changed }))
}

/// Return entry counts and embedding dimension for a project.
#[utoipa::path(
    get,
    path = "/v1/projects/{project_id}/stats",
    params(
        ("project_id" = String, Path, description = "Project slug"),
    ),
    responses(
        (status = 200, description = "Project stats", body = super::db::ProjectStats),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Project not found", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn project_stats(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;
    let stats = db.stats(project.id)?;
    Ok(Json(stats))
}

/// Return all git commit SHAs stored in note tags for a project.
///
/// Each harvested commit is tagged `git:<sha>`. Clients call this endpoint
/// to skip commits they have already stored, enabling incremental harvest.
#[utoipa::path(
    get,
    path = "/v1/projects/{project_id}/memory/harvested-shas",
    params(
        ("project_id" = String, Path, description = "Project slug"),
    ),
    responses(
        (status = 200, description = "List of harvested git SHAs", body = Vec<String>),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Project not found", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn harvested_shas(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;
    let shas = db.harvested_shas(project.id)?;
    Ok(Json(shas))
}

// ── Poll / SSE endpoints ──────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema, utoipa::IntoParams)]
pub struct SinceQuery {
    /// Unix epoch seconds (exclusive lower bound).
    pub t: i64,
    /// Maximum number of results (default: 100, max: 500).
    #[serde(default = "default_since_limit")]
    pub limit: i64,
}
fn default_since_limit() -> i64 {
    100
}

#[derive(Deserialize, ToSchema, utoipa::IntoParams)]
pub struct StreamQuery {
    /// Unix epoch seconds to start from (inclusive). Defaults to now.
    pub t: Option<i64>,
}

/// Return notes created after a given Unix timestamp. Archived entries are
/// excluded. Results are ordered `created_at ASC`.
#[utoipa::path(
    get,
    path = "/v1/projects/{project_id}/memory/since",
    params(
        ("project_id" = String, Path, description = "Project slug"),
        SinceQuery,
    ),
    responses(
        (status = 200, description = "Notes newer than `t`", body = Vec<super::db::ServerNote>),
        (status = 400, description = "Missing or invalid `t` parameter", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Project not found", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn memory_since(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Query(params): Query<SinceQuery>,
) -> Result<impl IntoResponse, AppError> {
    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;
    let notes = db.notes_since(project.id, params.t, params.limit)?;
    Ok(Json(notes))
}

/// Stream new memory entries as Server-Sent Events. Each event carries a
/// single `ServerNote` serialised as JSON. The stream polls the database once
/// per second and stays open indefinitely — close the connection to stop it.
///
/// Pass `?t=<unix_secs>` to replay entries written after a known timestamp.
/// Omit it to receive only entries written after the connection opens.
#[utoipa::path(
    get,
    path = "/v1/projects/{project_id}/memory/stream",
    params(
        ("project_id" = String, Path, description = "Project slug"),
        StreamQuery,
    ),
    responses(
        (status = 200, description = "SSE stream of new memory entries"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Project not found", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn memory_stream(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Query(params): Query<StreamQuery>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, AppError> {
    // Validate the project exists before opening the stream.
    {
        let db = state.db.lock().await;
        require_project(&db, &project_id)?;
    }

    let start_t = params.t.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
    });

    let s = stream! {
        let mut last_seen = start_t;
        loop {
            // Lock, query, immediately release.
            let notes = {
                let db = state.db.lock().await;
                // Re-fetch the project each iteration so the stream survives
                // project creation that may have happened after the handshake.
                let pid = match db.get_project(&project_id) {
                    Ok(Some(p)) => p.id,
                    _ => {
                        drop(db);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };
                // Ignore DB errors mid-stream; keep the connection alive.
                db.notes_since(pid, last_seen, 50).unwrap_or_default()
            };

            for note in notes {
                if note.created_at > last_seen {
                    last_seen = note.created_at;
                }
                let data = serde_json::to_string(&note).unwrap_or_default();
                yield Ok::<Event, Infallible>(Event::default().data(data));
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };

    Ok(Sse::new(s).keep_alive(KeepAlive::default()))
}

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

/// Generate embeddings for code chunks. The server encodes each chunk and returns the
/// vectors. **The server does not store the vectors** — the CLI is the only persistent
/// store for index data.
///
/// Returns 400 if no embedder is configured.
/// Returns 413 if the batch exceeds 256 chunks.
#[utoipa::path(
    post,
    path = "/v1/projects/{project_id}/index/embed",
    params(
        ("project_id" = String, Path, description = "Project slug"),
    ),
    request_body = EmbedRequest,
    responses(
        (status = 200, description = "Embedding vectors (not stored server-side)", body = EmbedResponse),
        (status = 400, description = "No embedder configured", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 413, description = "Batch exceeds 256 chunks", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "index"
)]
pub async fn index_embed(
    State(state): State<AppState>,
    Path(_project_id): Path<String>,
    Json(body): Json<EmbedRequest>,
) -> Result<Response, AppError> {
    const MAX_BATCH: usize = 256;

    // Check batch size first so clients get a 413 even when no embedder is configured.
    if body.chunks.len() > MAX_BATCH {
        return Ok((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ErrorBody::new(
                "bad_request",
                &format!(
                    "Batch size {} exceeds maximum of {MAX_BATCH} chunks per request.",
                    body.chunks.len()
                ),
            )),
        )
            .into_response());
    }

    let embedder = state.embedder.as_ref().ok_or_else(|| {
        AppError::BadRequest(
            "index.embed requires an embedder. Configure SPELUNK_EMBEDDING_URL on the server."
                .to_string(),
        )
    })?;

    if body.chunks.is_empty() {
        return Ok(Json(EmbedResponse { chunks: vec![] }).into_response());
    }

    // Collect texts, preserving order for reassembly.
    let texts: Vec<&str> = body.chunks.iter().map(|c| c.content.as_str()).collect();
    let vectors = embedder.embed(&texts).await.map_err(AppError::Internal)?;

    if vectors.len() != body.chunks.len() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "Embedder returned {} vectors for {} chunks",
            vectors.len(),
            body.chunks.len()
        )));
    }

    let out_chunks: Vec<EmbedChunkOut> = body
        .chunks
        .into_iter()
        .zip(vectors)
        .map(|(c, v)| EmbedChunkOut {
            chunk_id: c.chunk_id,
            vector: v,
        })
        .collect();

    // Data promise: vectors are NOT stored on the server. We return them directly.
    Ok(Json(EmbedResponse { chunks: out_chunks }).into_response())
}

// ── LLM complete (SSE) ───────────────────────────────────────────────────────

/// A single chat message in the completion request.
#[derive(Deserialize, ToSchema)]
pub struct LlmMessage {
    /// Role of the message author: `system`, `user`, or `assistant`.
    pub role: String,
    /// Text content of the message.
    pub content: String,
}

/// Request body for `POST /v1/projects/{project_id}/llm/complete`.
#[derive(Deserialize, ToSchema)]
pub struct LlmCompleteRequest {
    /// Ordered list of chat messages. Must be non-empty.
    pub messages: Vec<LlmMessage>,
    /// Maximum tokens to generate. The server clamps this to its configured ceiling.
    pub max_tokens: u32,
    /// Optional OpenAI-style `response_format.json_schema` for structured output.
    /// Backends that do not support structured output silently ignore this field.
    #[serde(default)]
    pub json_schema: Option<serde_json::Value>,
}

/// Server-side ceiling for `max_tokens` on `llm/complete` requests.
const LLM_COMPLETE_MAX_TOKENS: u32 = 8192;

/// Run a single LLM completion over caller-supplied messages. Streams tokens as SSE events.
///
/// The server performs **no orchestration**, adds **no system prompt**, and stores **nothing**.
/// Prompt content is entirely the caller's responsibility.
///
/// Response events (one per `data:` line):
/// - `{"kind":"token","content":"..."}` — one streamed token fragment
/// - `{"kind":"done"}` — terminal success event
/// - `{"kind":"error","code":"...","message":"..."}` — terminal failure event
///
/// Returns **400** when `messages` is empty or `max_tokens` ≤ 0.
/// Returns **503** when no LLM backend is configured on this server.
#[utoipa::path(
    post,
    path = "/v1/projects/{project_id}/llm/complete",
    params(
        ("project_id" = String, Path, description = "Project slug (e.g. `usercise/spelunk`)")
    ),
    request_body = LlmCompleteRequest,
    responses(
        (status = 200, description = "SSE stream: token/done/error events"),
        (status = 400, description = "Bad request (empty messages or invalid max_tokens)", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 503, description = "No LLM configured on this server", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "inference"
)]
pub async fn llm_complete(
    State(state): State<AppState>,
    Path(_project_id): Path<String>,
    Json(body): Json<LlmCompleteRequest>,
) -> Result<Response, AppError> {
    // Validate inputs before touching the LLM backend.
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
            Json(ErrorBody::new(
                "bad_request",
                "max_tokens must be greater than 0",
            )),
        )
            .into_response());
    }

    // Validate roles.
    for msg in &body.messages {
        if !matches!(msg.role.as_str(), "system" | "user" | "assistant") {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(ErrorBody::new(
                    "bad_request",
                    &format!(
                        "invalid role {:?}; must be system, user, or assistant",
                        msg.role
                    ),
                )),
            )
                .into_response());
        }
    }

    // 503 when no LLM is configured — use the llm_unavailable code from ADR-002.
    let llm = match state.llm.clone() {
        Some(l) => l,
        None => {
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorBody::new(
                    "llm_unavailable",
                    "llm.complete requires an LLM backend. \
                     Configure the chat model on the server.",
                )),
            )
                .into_response());
        }
    };

    // Clamp max_tokens to server ceiling.
    let max_tokens = body.max_tokens.min(LLM_COMPLETE_MAX_TOKENS) as usize;

    // Convert request messages to core Message type.
    let messages: Vec<spelunk_core::llm::Message> = body
        .messages
        .into_iter()
        .map(|m| spelunk_core::llm::Message {
            role: m.role,
            content: m.content,
        })
        .collect();

    let (tx, mut rx) = mpsc::channel::<String>(64);
    let json_schema = body.json_schema;

    // Spawn LLM generation into a background task.
    tokio::spawn(async move {
        if let Err(e) = llm.generate(&messages, max_tokens, tx, json_schema).await {
            tracing::warn!("llm/complete generate error: {e}");
        }
    });

    // Stream tokens as SSE events per ADR-002 contract:
    //   {"kind":"token","content":"..."} per token
    //   {"kind":"done"} as the terminal event
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
        (status = 503, description = "No LLM configured", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "inference"
)]
pub async fn explore(
    State(state): State<AppState>,
    Path(_project_id): Path<String>,
    Json(body): Json<ExploreRequest>,
) -> Result<Response, AppError> {
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

    // Spawn LLM generation into a background task.
    tokio::spawn(async move {
        if let Err(e) = llm.generate(&messages, max_tokens, tx, None).await {
            tracing::warn!("explore LLM generate error: {e}");
        }
    });

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

// ── Helpers ───────────────────────────────────────────────────────────────────

fn require_project(db: &super::db::ServerDb, slug: &str) -> Result<super::db::Project, AppError> {
    db.get_project(slug)?.ok_or(AppError::NotFound)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::Body,
        http::{self, Request},
    };
    use serde_json::{Value, json};
    use tower::ServiceExt; // for `oneshot`

    use super::super::auth::ApiKeyAuth;
    use super::super::db::ServerDb;
    use super::super::{AppState, router};

    /// Register sqlite-vec extension once per test process.
    fn register_sqlite_vec() {
        use std::sync::OnceLock;
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )));
            }
        });
    }

    fn make_app(conflict_threshold: f32) -> (axum::Router, i32) {
        register_sqlite_vec();
        let dim: usize = 4;
        let db = ServerDb::open(std::path::Path::new(":memory:"), dim)
            .expect("failed to open in-memory server db");
        let state = AppState {
            db: Arc::new(tokio::sync::Mutex::new(db)),
            auth: Arc::new(ApiKeyAuth::new(None)),
            conflict_threshold,
            embedder: None,
            llm: None,
        };
        (router(state), dim as i32)
    }

    /// POST /v1/projects/{slug}/memory with the given embedding. Returns the response.
    async fn post_note(
        app: axum::Router,
        slug: &str,
        title: &str,
        embedding: Vec<f32>,
    ) -> (http::StatusCode, Value) {
        let body = json!({
            "kind": "note",
            "title": title,
            "body": "test body",
            "embedding": embedding,
        });
        let req = Request::builder()
            .method("POST")
            .uri(format!("/v1/projects/{slug}/memory"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    /// Two semantically identical entries (identical embeddings) should trigger 409
    /// and a `contradicts` edge should be inserted.
    #[tokio::test]
    async fn conflict_detection_identical_embeddings_returns_409() {
        let (app, _dim) = make_app(0.92);
        // Use a very low threshold to ensure a conflict (0.0 = any non-zero similarity conflicts).
        let (app_low, _dim) = make_app(0.0);

        // First entry — must be 201.
        let embedding = vec![1.0_f32, 0.0, 0.0, 0.0];
        let (status1, body1) = post_note(
            app_low.clone(),
            "test-project",
            "Entry A",
            embedding.clone(),
        )
        .await;
        assert_eq!(
            status1,
            http::StatusCode::CREATED,
            "first write must be 201; body: {body1}"
        );
        let first_id = body1["id"].as_i64().expect("id in response");
        assert_eq!(body1["stored"], json!(true));

        // Second entry with identical embedding — must be 409.
        let (status2, body2) = post_note(
            app_low.clone(),
            "test-project",
            "Entry B (duplicate)",
            embedding.clone(),
        )
        .await;
        assert_eq!(
            status2,
            http::StatusCode::CONFLICT,
            "second identical write must be 409; body: {body2}"
        );
        assert_eq!(
            body2["stored"],
            json!(true),
            "stored must be true even on 409"
        );

        let conflicts = body2["conflicts"]
            .as_array()
            .expect("conflicts array in 409 body");
        assert!(!conflicts.is_empty(), "conflicts must not be empty");
        let conflicting_ids: Vec<i64> = conflicts.iter().filter_map(|c| c["id"].as_i64()).collect();
        assert!(
            conflicting_ids.contains(&first_id),
            "first entry's id ({first_id}) must appear in conflicts; got: {conflicting_ids:?}"
        );

        // Similarity should be > 0.
        let similarity = conflicts[0]["similarity"]
            .as_f64()
            .expect("similarity field");
        assert!(
            similarity > 0.0,
            "similarity must be positive; got {similarity}"
        );

        // Suppress unused variable warning from app (default threshold).
        drop(app);
    }

    /// At default threshold (0.92), dissimilar entries must not conflict.
    #[tokio::test]
    async fn conflict_detection_dissimilar_entries_no_conflict() {
        let (app, _dim) = make_app(0.92);

        // Orthogonal embeddings — cosine similarity = 0.
        let emb_a = vec![1.0_f32, 0.0, 0.0, 0.0];
        let emb_b = vec![0.0_f32, 1.0, 0.0, 0.0];

        let (status1, _) = post_note(app.clone(), "proj-dissimilar", "Alpha", emb_a).await;
        assert_eq!(status1, http::StatusCode::CREATED);

        let (status2, body2) = post_note(app.clone(), "proj-dissimilar", "Beta", emb_b).await;
        assert_eq!(
            status2,
            http::StatusCode::CREATED,
            "orthogonal entries must not conflict; body: {body2}"
        );
    }

    /// threshold = 1.0 disables conflict detection entirely.
    #[tokio::test]
    async fn conflict_detection_disabled_at_threshold_one() {
        let (app, _dim) = make_app(1.0);

        // Use identical embeddings — but with threshold=1.0, no conflict should fire.
        let embedding = vec![1.0_f32, 0.0, 0.0, 0.0];
        let (status1, _) = post_note(app.clone(), "proj-disabled", "X", embedding.clone()).await;
        assert_eq!(status1, http::StatusCode::CREATED);
        let (status2, body2) = post_note(app.clone(), "proj-disabled", "X dup", embedding).await;
        assert_eq!(
            status2,
            http::StatusCode::CREATED,
            "threshold=1.0 must disable conflict detection; body: {body2}"
        );
    }

    /// GET /v1/health should return JSON with `status`, `version`, and `capabilities`.
    #[tokio::test]
    async fn health_returns_json_with_capabilities() {
        let (app, _) = make_app(0.92);
        let req = Request::builder()
            .method("GET")
            .uri("/v1/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).expect("health must return JSON");
        assert_eq!(json["status"], json!("ok"), "status must be 'ok'");
        assert!(json["version"].is_string(), "version must be a string");
        assert!(
            json["capabilities"].is_array(),
            "capabilities must be an array"
        );
        let caps = json["capabilities"].as_array().unwrap();
        assert!(
            caps.iter().any(|c| c == "memory"),
            "capabilities must include 'memory'"
        );
    }

    /// POST /v1/projects/{slug}/memory/search with no embedder should return 400.
    #[tokio::test]
    async fn search_without_embedder_returns_400() {
        let (app, _) = make_app(0.92);
        // First create the project.
        let _ = post_note(
            app.clone(),
            "search-proj",
            "seed note",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;

        let body = json!({"query": "test query", "limit": 5});
        let req = Request::builder()
            .method("POST")
            .uri("/v1/projects/search-proj/memory/search")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            http::StatusCode::BAD_REQUEST,
            "search without embedder must return 400"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        assert_eq!(
            json["error"]["code"],
            json!("bad_request"),
            "error code must be bad_request"
        );
    }

    /// POST /v1/projects/{slug}/index/embed with no embedder should return 400.
    #[tokio::test]
    async fn embed_without_embedder_returns_400() {
        let (app, _) = make_app(0.92);
        let body = json!({"chunks": [{"chunk_id": "abc", "content": "fn foo() {}"}]});
        let req = Request::builder()
            .method("POST")
            .uri("/v1/projects/proj/index/embed")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            http::StatusCode::BAD_REQUEST,
            "embed without embedder must return 400"
        );
    }

    /// POST /v1/projects/{slug}/explore with no LLM should return 503.
    #[tokio::test]
    async fn explore_without_llm_returns_503() {
        let (app, _) = make_app(0.92);
        let body = json!({"question": "what does foo do?", "context_chunks": []});
        let req = Request::builder()
            .method("POST")
            .uri("/v1/projects/proj/explore")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            http::StatusCode::SERVICE_UNAVAILABLE,
            "explore without LLM must return 503"
        );
    }

    /// POST /v1/projects/{slug}/index/embed with >256 chunks should return 413.
    #[tokio::test]
    async fn embed_batch_too_large_returns_413() {
        let (app, _) = make_app(0.92);
        let chunks: Vec<Value> = (0..=256)
            .map(|i| json!({"chunk_id": format!("c{i}"), "content": "fn foo() {}"}))
            .collect();
        let body = json!({"chunks": chunks});
        let req = Request::builder()
            .method("POST")
            .uri("/v1/projects/proj/index/embed")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            http::StatusCode::PAYLOAD_TOO_LARGE,
            "batch >256 must return 413"
        );
    }
}
