use serde::{Deserialize, Serialize};

use super::super::memory::Note;

// ── Wire types (match server JSON schema) ─────────────────────────────────────

#[derive(Serialize)]
pub(super) struct AddNoteRequest {
    pub(super) kind: String,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) tags: Vec<String>,
    pub(super) linked_files: Vec<String>,
    pub(super) embedding: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) source_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) valid_at: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct AddNoteResponse {
    pub(super) id: i64,
    #[serde(default)]
    pub(super) conflicts: Vec<ConflictInfo>,
}

/// Conflict information returned by the server when a new note is semantically
/// close to an existing active entry (HTTP 409).
#[derive(Debug, Deserialize, Clone)]
pub struct ConflictInfo {
    pub id: i64,
    pub title: String,
    pub similarity: f32,
}

#[derive(Deserialize)]
pub(super) struct NoteResponse {
    pub(super) id: i64,
    pub(super) kind: String,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) tags: Vec<String>,
    pub(super) linked_files: Vec<String>,
    pub(super) created_at: i64,
    pub(super) status: String,
    pub(super) superseded_by: Option<i64>,
    #[serde(default)]
    pub(super) source_ref: Option<String>,
    #[serde(default)]
    pub(super) valid_at: Option<i64>,
    #[serde(default)]
    pub(super) invalid_at: Option<i64>,
    #[serde(default)]
    pub(super) distance: Option<f64>,
}

impl From<NoteResponse> for Note {
    fn from(r: NoteResponse) -> Self {
        Note {
            id: r.id,
            kind: r.kind,
            title: r.title,
            body: r.body,
            tags: r.tags,
            linked_files: r.linked_files,
            created_at: r.created_at,
            status: r.status,
            superseded_by: r.superseded_by,
            source_ref: r.source_ref,
            valid_at: r.valid_at,
            invalid_at: r.invalid_at,
            distance: r.distance,
            score: None,
            source_project: None,
            source_project_path: None,
        }
    }
}

#[derive(Serialize)]
pub(super) struct SearchRequest {
    pub(super) query: String,
    pub(super) limit: usize,
}

#[derive(Serialize)]
pub(super) struct SupersedeRequest {
    pub(super) new_id: i64,
}

#[derive(Deserialize)]
pub(super) struct BoolResponse {
    pub(super) changed: bool,
}

#[derive(Deserialize)]
pub(super) struct CountResponse {
    pub(super) count: i64,
}

// ── Cloud project listing (ADR-005 slug→UUID resolution) ──────────────────────

/// One entry from cloud-api `GET /v1/projects` (`listProjects`).
///
/// Only `id` and `slug` are needed to resolve a human slug to its UUID; the
/// other fields the endpoint returns (name, visibility, …) are ignored.
#[derive(Deserialize)]
pub(super) struct CloudProjectItem {
    pub(super) id: uuid::Uuid,
    #[serde(default)]
    pub(super) slug: Option<String>,
}

/// Response body of cloud-api `GET /v1/projects`.
#[derive(Deserialize)]
pub(super) struct CloudProjectListResponse {
    pub(super) projects: Vec<CloudProjectItem>,
}
