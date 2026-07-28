use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{AppError, AppState, ErrorBody};

use super::{
    require_embedder, require_project, validate_embedding_dim, validate_project_slug,
    validate_title_body,
};

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
    /// Pre-computed embedding vector from the client. Optional: if omitted and the
    /// server's embedder is ready, the server embeds the entry.
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
    /// Text query: the server encodes this using its configured embedder.
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

// ── Memory CRUD ───────────────────────────────────────────────────────────────

/// Add a memory entry to a project. The project is auto-created on first write.
///
/// The `embedding` field is optional. If omitted and the server's embedder is ready,
/// the server embeds the entry before storage. If neither is available, the
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
        (status = 422, description = "Entry rejected: prompt injection detected"),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn add_note(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(body): Json<AddNoteRequest>,
) -> Result<Response, AppError> {
    validate_project_slug(&project_id)?;
    validate_title_body(&body.title, &body.body)?;
    {
        let configured_dim = state.db.lock().await.embedding_dim;
        validate_embedding_dim(body.embedding.as_deref(), configured_dim)?;
    }

    // Reject entries that contain prompt-injection patterns.
    if let Some(m) = crate::security::scan_for_injection(&body.title, &body.body) {
        // Audit only non-sensitive locators; never echo the matched text.
        tracing::warn!(
            "note rejected: injection pattern matched (project={project_id}, field={}, category={}, title_len={}, body_len={})",
            m.field,
            m.category,
            body.title.len(),
            body.body.len()
        );
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
    // Only the *ready* backend can embed; while the embedder is loading/unavailable
    // we store the entry text-only (graceful: a memory write must not block on
    // model warm-up), matching the existing "no embedder" degradation.
    let server_embedding: Option<Vec<f32>> = if body.embedding.is_none() {
        if let Some(embedder) = state.embedder.backend() {
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
    let model = db.embedding_model.clone();
    let project = db.upsert_project(&project_id, dim, &model)?;

    // The single-note response reports the local row id (unchanged wire
    // shape); `sync_id` is irrelevant here since this path never round-trips
    // through `/memory/since` cursoring the way a batch push ack does.
    let (id, _sync_id) = db.add_note(
        project.id,
        &body.kind,
        &body.title,
        &body.body,
        &body.tags,
        &body.linked_files,
        embedding,
        None,
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
        (status = 200, description = "List of notes", body = Vec<crate::db::ServerNote>),
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
        (status = 200, description = "Note found", body = crate::db::ServerNote),
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
        (status = 200, description = "Nearest neighbours", body = Vec<crate::db::ServerNote>),
        (status = 400, description = "No embedder configured", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Project not found", body = ErrorBody),
        (status = 429, description = "Embed admission queue full; retry after the given delay", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn search_notes(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(body): Json<SearchRequest>,
) -> Result<impl IntoResponse, AppError> {
    let embedder = require_embedder(
        &state,
        "This server has no embedder configured. Semantic memory search is unavailable.",
    )?;

    // Admission control: same shared mutex-serialized embedder as
    // `/index/embed` and `project_search`; shed with 429 once the bounded
    // queue is full instead of queuing silently.
    let _admission = state.embed_admission.try_acquire()?;

    // F2LLM QA query prefix: matches the instruction format used for memory documents.
    let query_text = format!(
        "Instruct: Given a question, retrieve passages that answer the question\nQuery: {}",
        body.query
    );
    let query_vecs = embedder
        .embed(&[query_text.as_str()])
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
