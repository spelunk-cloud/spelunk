use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::collections::HashSet;
use std::path::PathBuf;

use git_meta_lib::{Session, Target};

use super::backend::{MemoryBackend, NoteInput};
use super::memory::{MemoryEdge, Note};
use super::note_record::{NoteRecord, now_millis, now_secs, record_to_note};

const SPELUNK_KEY: &str = "spelunk:entries";

pub struct GitMetaBackend {
    /// If `Some`, pin session to this directory (for tests).
    /// If `None`, use `Session::discover()` from the current directory.
    root: Option<PathBuf>,
}

impl Default for GitMetaBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl GitMetaBackend {
    pub fn new() -> Self {
        Self { root: None }
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self { root: Some(root) }
    }
}

fn open_session(root: &Option<PathBuf>) -> Result<Session> {
    match root {
        Some(path) => Session::open(path).map_err(|e| anyhow!("git-meta: open session: {e}")),
        None => Session::discover().map_err(|e| anyhow!("git-meta: discover session: {e}")),
    }
}

/// Deserialize all list entries and deduplicate by id, keeping the entry with
/// the highest `ListEntry.timestamp` for each id (handles append-only archive updates).
fn read_records(session: &Session) -> Result<Vec<NoteRecord>> {
    let handle = session.target(&Target::project());
    let entries = handle
        .list_entries(SPELUNK_KEY)
        .map_err(|e| anyhow!("git-meta: list_entries: {e}"))?;

    let mut by_id: std::collections::HashMap<i64, (i64, NoteRecord)> =
        std::collections::HashMap::new();

    for entry in entries {
        let record: NoteRecord = serde_json::from_str(&entry.value)
            .map_err(|e| anyhow!("git-meta: deserialize record: {e}"))?;

        if record.schema_version > 1 {
            return Err(anyhow::Error::new(
                crate::error::SpelunkError::SchemaMismatch {
                    found: record.schema_version,
                    max_known: 1,
                },
            ));
        }

        let ts = entry.timestamp;
        match by_id.entry(record.id) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((ts, record));
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                if ts > o.get().0 {
                    o.insert((ts, record));
                }
            }
        }
    }

    // Return newest-first by id (id = now_millis() at insert time, so higher = newer)
    let mut records: Vec<NoteRecord> = by_id.into_values().map(|(_, r)| r).collect();
    records.sort_by_key(|r| std::cmp::Reverse(r.id));
    Ok(records)
}

#[async_trait]
impl MemoryBackend for GitMetaBackend {
    async fn add(&self, input: NoteInput) -> Result<i64> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            let session = open_session(&root)?;
            let id = now_millis();
            let record = NoteRecord {
                schema_version: 1,
                id,
                kind: input.kind,
                title: input.title,
                body: input.body,
                tags: input.tags,
                linked_files: input.linked_files,
                created_at: now_secs(),
                status: "active".to_string(),
                source_ref: input.source_ref,
                valid_at: input.valid_at,
                invalid_at: None,
                superseded_by: None,
            };
            let json = serde_json::to_string(&record)?;
            let handle = session.target(&Target::project());
            handle
                .list_push(SPELUNK_KEY, &json)
                .map_err(|e| anyhow!("git-meta: list_push: {e}"))?;
            Ok(id)
        })
        .await?
    }

    async fn list(
        &self,
        kind_filter: Option<&str>,
        limit: usize,
        include_archived: bool,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        let root = self.root.clone();
        let kind_filter = kind_filter.map(str::to_owned);
        tokio::task::spawn_blocking(move || {
            let session = open_session(&root)?;
            let records = read_records(&session)?;
            let notes = records
                .into_iter()
                .filter(|r| kind_filter.as_deref().is_none_or(|k| r.kind == k))
                .filter(|r| include_archived || r.status != "archived")
                .filter(|r| {
                    if let Some(ts) = as_of {
                        let effective = r.valid_at.unwrap_or(r.created_at);
                        if effective > ts {
                            return false;
                        }
                        if r.invalid_at.is_some_and(|ia| ia <= ts) {
                            return false;
                        }
                    }
                    true
                })
                .take(limit)
                .map(record_to_note)
                .collect();
            Ok(notes)
        })
        .await?
    }

    async fn get(&self, id: i64) -> Result<Option<Note>> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            let session = open_session(&root)?;
            let records = read_records(&session)?;
            Ok(records.into_iter().find(|r| r.id == id).map(record_to_note))
        })
        .await?
    }

    async fn count(&self) -> Result<i64> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            let session = open_session(&root)?;
            Ok(read_records(&session)?.len() as i64)
        })
        .await?
    }

    async fn archive(&self, id: i64) -> Result<bool> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            let session = open_session(&root)?;
            let records = read_records(&session)?;
            let Some(mut record) = records.into_iter().find(|r| r.id == id) else {
                return Ok(false);
            };
            record.status = "archived".to_string();
            let json = serde_json::to_string(&record)?;
            let handle = session.target(&Target::project());
            handle
                .list_push(SPELUNK_KEY, &json)
                .map_err(|e| anyhow!("git-meta: list_push (archive): {e}"))?;
            Ok(true)
        })
        .await?
    }

    // ── Unsupported ──────────────────────────────────────────────────────────

    async fn search_timeline(&self, _query_blob: &[u8], _limit: usize) -> Result<Vec<Note>> {
        Err(crate::error::SpelunkError::BackendUnsupported("search_timeline".into()).into())
    }

    async fn search(
        &self,
        _query_blob: &[u8],
        _limit: usize,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        Err(crate::error::SpelunkError::BackendUnsupported("search".into()).into())
    }

    async fn search_text(
        &self,
        _query: &str,
        _limit: usize,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        Err(crate::error::SpelunkError::BackendUnsupported("search_text".into()).into())
    }

    async fn search_hybrid(
        &self,
        _query_blob: &[u8],
        _query: &str,
        _limit: usize,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        Err(crate::error::SpelunkError::BackendUnsupported("search_hybrid".into()).into())
    }

    async fn list_by_source_ref(
        &self,
        _source_ref_prefix: &str,
        _limit: usize,
        _include_archived: bool,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        Err(crate::error::SpelunkError::BackendUnsupported("list_by_source_ref".into()).into())
    }

    async fn supersede(&self, _old_id: i64, _new_id: i64) -> Result<bool> {
        Err(crate::error::SpelunkError::BackendUnsupported("supersede".into()).into())
    }

    async fn harvested_shas(&self) -> Result<HashSet<String>> {
        Err(crate::error::SpelunkError::BackendUnsupported("harvested_shas".into()).into())
    }

    async fn has_source_ref(&self, _sha: &str) -> Result<bool> {
        Err(crate::error::SpelunkError::BackendUnsupported("has_source_ref".into()).into())
    }

    async fn add_edge(&self, _from_id: i64, _to_id: i64, _kind: &str) -> Result<()> {
        Err(crate::error::SpelunkError::BackendUnsupported("add_edge".into()).into())
    }

    async fn get_edges(&self, _id: i64) -> Result<(Vec<MemoryEdge>, Vec<MemoryEdge>)> {
        Err(crate::error::SpelunkError::BackendUnsupported("get_edges".into()).into())
    }

    fn backend_kind(&self) -> &'static str {
        "git-meta"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::storage::backend::NoteInput;

    fn make_temp_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    fn input(kind: &str, title: &str) -> NoteInput {
        NoteInput {
            kind: kind.to_string(),
            title: title.to_string(),
            body: "body".to_string(),
            tags: vec![],
            linked_files: vec![],
            embedding: None,
            source_ref: None,
            valid_at: None,
            supersedes: None,
        }
    }

    #[tokio::test]
    async fn test_add_and_list() {
        let (_dir, path) = make_temp_repo();
        let backend = GitMetaBackend::with_root(path);
        backend.add(input("decision", "alpha")).await.unwrap();
        backend.add(input("decision", "beta")).await.unwrap();
        backend.add(input("decision", "gamma")).await.unwrap();

        let notes = backend.list(None, 100, false, None).await.unwrap();
        assert_eq!(notes.len(), 3);
        // newest first
        assert_eq!(notes[0].title, "gamma");
        assert_eq!(notes[2].title, "alpha");
    }

    #[tokio::test]
    async fn test_list_kind_filter() {
        let (_dir, path) = make_temp_repo();
        let backend = GitMetaBackend::with_root(path);
        backend.add(input("decision", "a decision")).await.unwrap();
        backend.add(input("note", "a note")).await.unwrap();

        let notes = backend.list(Some("note"), 100, false, None).await.unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, "a note");
    }

    #[tokio::test]
    async fn test_archive_hides_entry() {
        let (_dir, path) = make_temp_repo();
        let backend = GitMetaBackend::with_root(path);
        let id = backend.add(input("decision", "to archive")).await.unwrap();
        assert!(backend.archive(id).await.unwrap());

        let notes = backend.list(None, 100, false, None).await.unwrap();
        assert!(notes.is_empty());

        let all = backend.list(None, 100, true, None).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, "archived");
    }
}
