use serde::{Deserialize, Serialize};

use super::super::memory::{Note, NoteId};

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
    pub(super) id: NoteId,
    #[serde(default)]
    pub(super) conflicts: Vec<ConflictInfo>,
    /// Server-assigned cross-machine id, if the server minted one. Absent on
    /// older servers → `None`.
    #[serde(default)]
    pub(super) remote_id: Option<String>,
}

/// Conflict information returned by the server when a new note is semantically
/// close to an existing active entry (HTTP 409).
#[derive(Debug, Deserialize, Clone)]
pub struct ConflictInfo {
    pub id: NoteId,
    pub title: String,
    pub similarity: f32,
}

#[derive(Deserialize)]
pub(super) struct NoteResponse {
    pub(super) id: NoteId,
    pub(super) kind: String,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) tags: Vec<String>,
    pub(super) linked_files: Vec<String>,
    pub(super) created_at: i64,
    pub(super) status: String,
    pub(super) superseded_by: Option<NoteId>,
    #[serde(default)]
    pub(super) source_ref: Option<String>,
    #[serde(default)]
    pub(super) valid_at: Option<i64>,
    #[serde(default)]
    pub(super) invalid_at: Option<i64>,
    /// Canonical cross-machine id, if the server has one. Absent on older
    /// servers → `None`. Surfaced into the domain `Note` (ADR-059 D2).
    #[serde(default)]
    pub(super) remote_id: Option<String>,
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
            remote_id: r.remote_id,
        }
    }
}

/// Tolerant reader for the `list` and `search` read endpoints.
///
/// A team `spelunk-server` at or after the wire-contract fix wraps notes in an
/// `{ "entries": [...], "total": N }` object (ADR-076: a JSON response root
/// must be an object, never a bare array). Older servers still in the
/// version-skew support window emit a bare `[...]` array. Accepting both is
/// what keeps a newer CLI working against an older team server: the common
/// real-world skew, since a CLI is upgraded ahead of a team server running on
/// someone else's schedule. See `docs/version-skew.md`.
#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum NoteListPayload {
    Enveloped {
        #[serde(default)]
        entries: Vec<NoteResponse>,
    },
    Bare(Vec<NoteResponse>),
}

impl NoteListPayload {
    pub(super) fn into_notes(self) -> Vec<NoteResponse> {
        match self {
            NoteListPayload::Enveloped { entries } => entries,
            NoteListPayload::Bare(notes) => notes,
        }
    }
}

/// Tolerant reader for the `harvested-shas` endpoint. Same rationale as
/// [`NoteListPayload`]: newer servers send `{ "shas": [...] }`, older ones a
/// bare `["sha", ...]` array of primitives.
#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum HarvestedShasPayload {
    Enveloped {
        #[serde(default)]
        shas: Vec<String>,
    },
    Bare(Vec<String>),
}

impl HarvestedShasPayload {
    pub(super) fn into_shas(self) -> Vec<String> {
        match self {
            HarvestedShasPayload::Enveloped { shas } => shas,
            HarvestedShasPayload::Bare(shas) => shas,
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
    pub(super) new_id: NoteId,
}

#[derive(Deserialize)]
pub(super) struct BoolResponse {
    pub(super) changed: bool,
}

#[derive(Deserialize)]
pub(super) struct CountResponse {
    pub(super) count: i64,
}
