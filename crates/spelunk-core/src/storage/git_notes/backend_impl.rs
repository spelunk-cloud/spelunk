use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashSet;

use super::super::backend::{MemoryBackend, NoteInput};
use super::super::memory::{MemoryEdge, Note};
use super::super::note_record::{NoteRecord, now_millis, now_secs, record_to_note};
use super::GitNotesBackend;

#[async_trait]
impl MemoryBackend for GitNotesBackend {
    async fn add(&self, input: NoteInput) -> Result<(i64, bool)> {
        let id = now_millis();
        let entity_id =
            crate::storage::entity_id::entity_id(&input.kind, &input.title, &input.body);
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
            remote_id: None,
            entity_id: Some(entity_id),
            superseded_by_entity_id: None,
        };

        let head = self.head_sha().await?;
        self.append_record(&head, &record).await?;

        // Git notes are append-only: this backend never detects or collapses
        // a collision, so every add is reported as a fresh insert.
        Ok((id, true))
    }

    async fn list(
        &self,
        kind_filter: Option<&str>,
        limit: usize,
        include_archived: bool,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        let effective_limit = limit.min(super::GIT_NOTES_MAX_LIST);
        if limit > super::GIT_NOTES_MAX_LIST {
            tracing::warn!(
                "GitNotesBackend::list: caller requested {} entries; capped at {}. \
                 Use --backend sqlite for unbounded listing.",
                limit,
                super::GIT_NOTES_MAX_LIST
            );
        }
        self.collect(kind_filter, include_archived, as_of, effective_limit)
            .await
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

    /// Folds every commit's records first (`folded_records`), then looks up
    /// `id` in the *folded* result. A raw unfolded scan would return an
    /// entity's original record verbatim even after a later state-update
    /// (e.g. from `append_state_update`) archived it — the folded record
    /// keeps the original `id` (the earliest-created copy is always
    /// `fold_group`'s base) but reflects the entity's current `status` and
    /// `superseded_by_entity_id`, which callers checking "is OLD still
    /// active" (ADR-068 E4) depend on.
    async fn get(&self, id: i64) -> Result<Option<Note>> {
        Ok(self
            .folded_records()
            .await?
            .into_iter()
            .find(|record| record.id == id)
            .map(record_to_note))
    }

    async fn count(&self) -> Result<i64> {
        Ok(self.noted_commits().await?.len() as i64)
    }

    async fn archive(&self, id: i64) -> Result<bool> {
        for noted in self.noted_commits().await? {
            if self.archive_record(&noted.commit, id).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ── Unsupported ──────────────────────────────────────────────────────────

    async fn search_timeline(
        &self,
        _query_blob: &[u8],
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<Note>> {
        Err(crate::error::SpelunkError::BackendUnsupported("search_timeline".into()).into())
    }

    async fn search(
        &self,
        _query_blob: &[u8],
        _query: &str,
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
        "git-notes"
    }
}
