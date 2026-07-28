use std::convert::Infallible;
use std::time::Duration;

use async_stream::stream;
use axum::{
    Json,
    extract::{Path, Query, State},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{AppError, AppState, ErrorBody};

use super::require_project;

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
    /// Unix epoch seconds (exclusive lower bound). Required unless `since_id`
    /// is given; ignored when `since_id` is present.
    pub t: Option<i64>,
    /// UUID cursor (exclusive lower bound, arrival-ordered; see
    /// `ServerDb::notes_since_id`). Takes precedence over `t` when both are
    /// supplied. Selects the delta-pull response shape (`{entries, count}`)
    /// instead of the bare array `t` returns.
    #[serde(default)]
    pub since_id: Option<String>,
    /// Maximum number of results (default: 100, max: 500).
    #[serde(default = "default_since_limit")]
    pub limit: i64,
}
fn default_since_limit() -> i64 {
    100
}

/// One entry in the `since_id`-cursor response of `/memory/since`. `id` is
/// the note's server-minted `sync_id` (arrival-ordered), never its integer
/// `id`, which has no meaning to a puller on a different machine.
#[derive(Serialize, ToSchema)]
pub struct SinceIdEntry {
    pub id: String,
    pub kind: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<String>,
    /// RFC 3339 timestamp.
    pub created_at: String,
}

/// Response body for the `since_id`-cursor mode of `/memory/since`.
#[derive(Serialize, ToSchema)]
pub struct SinceIdResponse {
    pub entries: Vec<SinceIdEntry>,
    pub count: usize,
}

#[derive(Deserialize, ToSchema, utoipa::IntoParams)]
pub struct StreamQuery {
    /// Unix epoch seconds to start from (inclusive). Defaults to now.
    pub t: Option<i64>,
}

/// Return notes newer than a cursor, in one of two modes:
///
/// - `?since_id=<uuid>`: delta-pull mode (wire parity with cloud-api;
///   `CloudSyncClient::pull_since`/`spelunk sync` targets this). Returns
///   `{entries, count}`, entries ordered by arrival at this server.
/// - `?t=<unix_secs>`: legacy timestamp mode (`spelunk memory since`
///   targets this). Returns a bare array, ordered `created_at ASC`.
///
/// `since_id` takes precedence when both are supplied. Archived entries are
/// excluded in both modes.
#[utoipa::path(
    get,
    path = "/v1/projects/{project_id}/memory/since",
    params(
        ("project_id" = String, Path, description = "Project slug"),
        SinceQuery,
    ),
    responses(
        (status = 200, description = "Notes newer than `t` (bare array) or `since_id` (`{entries, count}`)", body = Vec<crate::db::ServerNote>),
        (status = 400, description = "Neither `t` nor `since_id` given", body = ErrorBody),
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
) -> Result<Response, AppError> {
    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;

    if let Some(cursor) = params.since_id.as_deref() {
        let rows = db.notes_since_id(project.id, cursor, params.limit)?;
        let entries: Vec<SinceIdEntry> = rows
            .into_iter()
            .map(|r| SinceIdEntry {
                id: r.sync_id,
                kind: r.kind,
                title: r.title,
                body: (!r.body.is_empty()).then_some(r.body),
                source_commit: r.source_commit,
                created_at: chrono::DateTime::from_timestamp(r.created_at, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default(),
            })
            .collect();
        let count = entries.len();
        return Ok(Json(SinceIdResponse { entries, count }).into_response());
    }

    let t = params
        .t
        .ok_or_else(|| AppError::BadRequest("missing `t` or `since_id` query parameter".into()))?;
    let notes = db.notes_since(project.id, t, params.limit)?;
    Ok(Json(notes).into_response())
}

/// Stream new memory entries as Server-Sent Events. Each event carries a
/// single `ServerNote` serialised as JSON. The stream polls the database once
/// per second and stays open indefinitely; close the connection to stop it.
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
