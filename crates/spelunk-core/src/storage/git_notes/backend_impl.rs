use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::collections::HashSet;

use super::super::backend::{MemoryBackend, NoteInput};
use super::super::memory::{MemoryEdge, Note};
use super::super::note_record::{NoteRecord, now_millis, now_secs, record_to_note};
use super::GitNotesBackend;

#[async_trait]
impl MemoryBackend for GitNotesBackend {
    async fn add(&self, input: NoteInput) -> Result<i64> {
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
        let head = self.head_sha().await?;

        let status = self
            .git()
            .args(["notes", "--ref=spelunk", "add", "-f", "-m", &json, &head])
            .status()
            .await?;

        if !status.success() {
            return Err(anyhow!("git notes add failed"));
        }

        Ok(id)
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
                "GitNotesBackend::list: caller requested {} entries; capped at {} to prevent \
                 O(n) subprocess hang. Use --backend sqlite for unbounded listing.",
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

    async fn get(&self, id: i64) -> Result<Option<Note>> {
        for (sha, _) in self.noted_commits().await? {
            if let Some(record) = self.read_record(&sha).await?
                && record.id == id
            {
                return Ok(Some(record_to_note(record)));
            }
        }
        Ok(None)
    }

    async fn count(&self) -> Result<i64> {
        Ok(self.noted_commits().await?.len() as i64)
    }

    async fn archive(&self, id: i64) -> Result<bool> {
        for (sha, _) in self.noted_commits().await? {
            if let Some(mut record) = self.read_record(&sha).await?
                && record.id == id
            {
                record.status = "archived".to_string();
                let json = serde_json::to_string(&record)?;
                let status = self
                    .git()
                    .args(["notes", "--ref=spelunk", "add", "-f", "-m", &json, &sha])
                    .status()
                    .await?;
                return Ok(status.success());
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
