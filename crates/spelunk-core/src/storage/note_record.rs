use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use super::entity_id::entity_id;
use super::memory::{Note, NoteId};

/// Serialised form stored as JSON in a memory backend (git-notes blob or SQLite).
///
/// `schema_version` 0 = legacy (field absent in old blobs), 1 = current.
#[derive(Debug, Serialize, Deserialize)]
pub struct NoteRecord {
    /// Absent in legacy blobs — treated as version 0 via `#[serde(default)]`.
    #[serde(default)]
    pub schema_version: u8,
    /// Machine-local SQLite rowid. NOT an identity: it renumbers on re-`init`
    /// and is assigned independently per machine. Kept for backward
    /// compatibility only — use `resolve_entity_id()` to identify an entry.
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
    /// Machine-local rowid of the successor. Not portable — see `id`. Prefer
    /// `superseded_by_entity_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<i64>,
    /// Canonical cross-machine id (uuid), set on sync to a remote server.
    /// Optional and additive: absent on the wire means `None`; an old blob
    /// without this key reads as `None`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub remote_id: Option<String>,
    /// Content-addressed canonical identity. Optional only because legacy blobs
    /// predate it; a reader recovers it with `resolve_entity_id()`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub entity_id: Option<String>,
    /// Portable supersede edge: the successor's `entity_id`. Survives a rowid
    /// renumber and resolves on any machine.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub superseded_by_entity_id: Option<String>,
}

impl NoteRecord {
    /// The record's canonical identity: the stored `entity_id`, or recomputed
    /// from `{kind, title, body}` when absent (legacy blob).
    pub fn resolve_entity_id(&self) -> String {
        self.entity_id
            .clone()
            .unwrap_or_else(|| entity_id(&self.kind, &self.title, &self.body))
    }
}

pub fn record_to_note(r: NoteRecord) -> Note {
    Note {
        id: NoteId::from_i64(r.id),
        kind: r.kind,
        title: r.title,
        body: r.body,
        tags: r.tags,
        linked_files: r.linked_files,
        created_at: r.created_at,
        status: r.status,
        superseded_by: r.superseded_by.map(NoteId::from_i64),
        source_ref: r.source_ref,
        valid_at: r.valid_at,
        invalid_at: r.invalid_at,
        distance: None,
        score: None,
        source_project: None,
        source_project_path: None,
        remote_id: r.remote_id,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn base_record() -> NoteRecord {
        NoteRecord {
            schema_version: 1,
            id: 42,
            kind: "decision".to_string(),
            title: "t".to_string(),
            body: "b".to_string(),
            tags: vec![],
            linked_files: vec![],
            created_at: 100,
            status: "active".to_string(),
            source_ref: None,
            valid_at: None,
            invalid_at: None,
            superseded_by: None,
            remote_id: None,
            entity_id: None,
            superseded_by_entity_id: None,
        }
    }

    /// (d) A record with a `remote_id` serializes the key and round-trips.
    #[test]
    fn note_record_round_trips_with_remote_id() {
        let mut rec = base_record();
        rec.remote_id = Some("11111111-1111-7111-8111-111111111111".to_string());

        let json = serde_json::to_string(&rec).expect("serialize");
        assert!(json.contains("\"remote_id\""), "key present when Some");

        let back: NoteRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.remote_id, rec.remote_id);
    }

    /// (d) A record without a `remote_id` omits the key, and an old blob that
    /// never had the key still deserializes (reads as `None`).
    #[test]
    fn note_record_round_trips_without_remote_id() {
        let rec = base_record();
        let json = serde_json::to_string(&rec).expect("serialize");
        assert!(!json.contains("remote_id"), "key omitted when None: {json}");

        // Old blob shape: no remote_id key at all.
        let old = r#"{"schema_version":1,"id":7,"kind":"note","title":"t","body":"b","tags":[],"linked_files":[],"created_at":1,"status":"active"}"#;
        let back: NoteRecord = serde_json::from_str(old).expect("deserialize old blob");
        assert_eq!(back.remote_id, None, "absent key reads as None");
        assert_eq!(back.id, 7);
    }

    /// A record carrying both identity fields round-trips.
    #[test]
    fn note_record_round_trips_with_entity_id() {
        let mut rec = base_record();
        rec.entity_id = Some(entity_id(&rec.kind, &rec.title, &rec.body));
        rec.superseded_by_entity_id = Some(entity_id("decision", "newer", "b2"));

        let json = serde_json::to_string(&rec).expect("serialize");
        assert!(json.contains("\"entity_id\""));
        assert!(json.contains("\"superseded_by_entity_id\""));

        let back: NoteRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.entity_id, rec.entity_id);
        assert_eq!(back.superseded_by_entity_id, rec.superseded_by_entity_id);
    }

    /// A legacy blob with no `entity_id` key recomputes the same id a fresh
    /// writer would have stored — absence is fully recoverable.
    #[test]
    fn legacy_blob_recomputes_entity_id() {
        let legacy = r#"{"schema_version":1,"id":1,"kind":"decision","title":"HTTP layer","body":"use axum","tags":["x"],"linked_files":["a.rs"],"created_at":123,"status":"active"}"#;
        let back: NoteRecord = serde_json::from_str(legacy).expect("deserialize legacy blob");

        assert_eq!(back.entity_id, None, "key absent in legacy blob");
        assert_eq!(
            back.resolve_entity_id(),
            "cc308a1ca5d849191e1710cc9def561377a9ef37e4fcb895e5aa3b1896e43603"
        );

        // A record that stores the field resolves to the identical value.
        let mut fresh = base_record();
        fresh.kind = "decision".to_string();
        fresh.title = "HTTP layer".to_string();
        fresh.body = "use axum".to_string();
        fresh.entity_id = Some(entity_id(&fresh.kind, &fresh.title, &fresh.body));
        assert_eq!(fresh.resolve_entity_id(), back.resolve_entity_id());
    }

    /// The bug this fixes: a re-`init` renumbers the rowid, so two different
    /// entries can carry the same `id` in one notes ref. Their `entity_id`s
    /// must still distinguish them.
    #[test]
    fn colliding_rowids_have_distinct_entity_ids() {
        let mut first = base_record();
        first.id = 1;
        first.title = "first decision".to_string();
        first.body = "body one".to_string();

        let mut second = base_record();
        second.id = 1; // re-init reset the counter
        second.title = "second decision".to_string();
        second.body = "body two".to_string();

        assert_eq!(first.id, second.id, "rowids collide, as observed live");
        assert_ne!(
            first.resolve_entity_id(),
            second.resolve_entity_id(),
            "distinct content must yield distinct identity despite the rowid collision"
        );
    }
}
