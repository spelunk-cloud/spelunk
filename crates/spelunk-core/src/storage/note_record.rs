use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use super::memory::Note;

/// Serialised form stored as JSON in a memory backend (git-notes blob or SQLite).
///
/// `schema_version` 0 = legacy (field absent in old blobs), 1 = current.
#[derive(Debug, Serialize, Deserialize)]
pub struct NoteRecord {
    /// Absent in legacy blobs — treated as version 0 via `#[serde(default)]`.
    #[serde(default)]
    pub schema_version: u8,
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub linked_files: Vec<String>,
    pub created_at: i64,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<i64>,
}

pub fn record_to_note(r: NoteRecord) -> Note {
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
        distance: None,
        score: None,
        source_project: None,
        source_project_path: None,
    }
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
