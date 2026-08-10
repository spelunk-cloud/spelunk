//! Wire types for the cloud API's memory routes.
//!
//! Deliberately separate from the team server's types in
//! [`wire_types`](super::super::wire_types): the two peers disagree about the
//! shape of an entry, not merely the routes that carry it. The cloud API keys
//! entries by UUID, timestamps them as RFC 3339 strings, names the harvest
//! field `source_commit`, and expresses "archived" as a tombstone timestamp
//! rather than a status word.

use serde::{Deserialize, Serialize};

use super::super::super::memory::{Note, NoteId};

#[derive(Serialize)]
pub(super) struct CreateEntryBody {
    pub(super) kind: String,
    pub(super) title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) body: Option<String>,
    /// The server's idempotency key and the only id the batch edge route
    /// accepts. Minted client-side on every add.
    pub(super) external_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) source_commit: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct EntryResponse {
    pub(super) id: NoteId,
    pub(super) kind: String,
    pub(super) title: String,
    #[serde(default)]
    pub(super) body: Option<String>,
    #[serde(default)]
    pub(super) external_id: Option<String>,
    #[serde(default)]
    pub(super) source_commit: Option<String>,
    /// Tombstone timestamp; `Some` means the entry is archived.
    #[serde(default)]
    pub(super) archived_at: Option<String>,
    #[serde(default)]
    pub(super) created_at: Option<String>,
    #[serde(default)]
    pub(super) distance: Option<f64>,
}

/// RFC 3339 to Unix seconds, `None` when absent or unparseable.
///
/// A timestamp this client cannot read must not sink the entry that carries
/// it: the caller loses ordering fidelity on that one row, not the read.
fn epoch_secs(ts: Option<&String>) -> Option<i64> {
    let raw = ts?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.timestamp())
}

impl EntryResponse {
    pub(super) fn into_note(self) -> Note {
        let created_at = epoch_secs(self.created_at.as_ref()).unwrap_or_default();
        let invalid_at = epoch_secs(self.archived_at.as_ref());
        let status = if self.archived_at.is_some() {
            "archived"
        } else {
            "active"
        };
        // The cloud id IS the cross-machine identity, so it is both the entry's
        // id on this backend and its `remote_id`.
        let remote_id = Some(self.id.to_string());
        Note {
            id: self.id,
            kind: self.kind,
            title: self.title,
            body: self.body.unwrap_or_default(),
            // The cloud API carries neither on its entry shape.
            tags: vec![],
            linked_files: vec![],
            created_at,
            status: status.to_string(),
            superseded_by: None,
            source_ref: self.source_commit,
            valid_at: None,
            invalid_at,
            distance: self.distance,
            score: None,
            source_project: None,
            source_project_path: None,
            remote_id,
        }
    }
}

#[derive(Deserialize)]
pub(super) struct EntryListResponse {
    #[serde(default)]
    pub(super) entries: Vec<EntryResponse>,
    /// Count of matching entries, computed server-side in the same round trip.
    #[serde(default)]
    pub(super) total: i64,
}

#[derive(Serialize)]
pub(super) struct BatchEdge {
    pub(super) from_external_id: String,
    pub(super) to_external_id: String,
    pub(super) kind: &'static str,
}

/// An edge-only batch: `entries` is required by the route but stays empty.
#[derive(Serialize)]
pub(super) struct BatchEdgeBody {
    pub(super) entries: [(); 0],
    pub(super) edges: Vec<BatchEdge>,
}

#[derive(Deserialize)]
pub(super) struct BatchEdgeOutcome {
    pub(super) status: String,
}

#[derive(Deserialize)]
pub(super) struct BatchEdgeResult {
    #[serde(default)]
    pub(super) edges: Vec<BatchEdgeOutcome>,
}

impl BatchEdgeResult {
    /// Whether the supersede edge actually landed.
    ///
    /// An edge naming an already-archived predecessor comes back
    /// `"unresolved"`; reporting that as success would tell the user a link
    /// exists when none does.
    pub(super) fn edge_applied(&self) -> bool {
        self.edges
            .iter()
            .any(|e| matches!(e.status.as_str(), "created" | "applied" | "updated"))
    }
}
