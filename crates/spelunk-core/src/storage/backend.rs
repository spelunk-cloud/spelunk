use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashSet;

use super::memory::{MemoryEdge, MemoryStore, Note, NoteId};

/// Input for adding a note. Owned to avoid lifetime issues across async boundaries.
pub struct NoteInput {
    pub kind: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub linked_files: Vec<String>,
    /// Raw embedding blob (little-endian f32 bytes). `None` = no embedding stored.
    pub embedding: Option<Vec<u8>>,
    /// Full 40-char git commit SHA for harvested entries; `None` for manual entries.
    pub source_ref: Option<String>,
    /// Unix epoch timestamp for when this entry became valid.
    /// None = use created_at (i.e. not explicitly set).
    pub valid_at: Option<i64>,
    /// ID of an existing entry that this entry supersedes.
    /// When set, the old entry's invalid_at is set to now() atomically.
    pub supersedes: Option<NoteId>,
}

/// Abstraction over local SQLite and remote HTTP memory stores.
#[async_trait]
pub trait MemoryBackend: Send {
    /// Returns `(id, created)`. `created` is `true` for a genuinely new entry,
    /// `false` when the write collided with an existing entry's `entity_id`
    /// and that entry was reused instead (only possible on a local SQLite
    /// backend whose `idx_notes_entity_id` has been promoted to UNIQUE, see
    /// `MemoryStore::add_note`). Backends that cannot detect this (git notes,
    /// remote) always return `true`.
    async fn add(&self, input: NoteInput) -> Result<(NoteId, bool)>;
    /// Topic-filtered search over ALL notes (incl. archived), ordered by
    /// valid_at/created_at ASC — the `memory timeline` retrieval.
    ///
    /// `query`: the raw query text. The local backend filters by full-text
    /// (FTS5) relevance to `query` and ignores `query_blob`, so `timeline`
    /// needs no inference server; the remote backend has no local embedder and
    /// sends `query` to the server, which embeds it server-side.
    async fn search_timeline(
        &self,
        query_blob: &[u8],
        query: &str,
        limit: usize,
    ) -> Result<Vec<Note>>;
    /// Semantic (vector KNN) search.
    ///
    /// `query`: the raw query text, see `search_timeline` for why both
    /// `query_blob` and `query` are passed to every backend.
    /// `as_of`: if set, only entries valid at that Unix timestamp are returned.
    async fn search(
        &self,
        query_blob: &[u8],
        query: &str,
        limit: usize,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>>;
    /// BM25 full-text search (no embedding required).
    /// `as_of`: if set, only entries valid at that Unix timestamp are returned.
    async fn search_text(&self, query: &str, limit: usize, as_of: Option<i64>)
    -> Result<Vec<Note>>;
    /// Hybrid search: semantic + BM25 fused via Reciprocal Rank Fusion.
    /// `as_of`: if set, only entries valid at that Unix timestamp are returned.
    async fn search_hybrid(
        &self,
        query_blob: &[u8],
        query: &str,
        limit: usize,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>>;
    /// `as_of`: if set, only entries valid at that Unix timestamp are returned.
    async fn list(
        &self,
        kind_filter: Option<&str>,
        limit: usize,
        include_archived: bool,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>>;
    /// List entries filtered by source_ref prefix (exact or prefix match).
    /// `as_of`: if set, only entries valid at that Unix timestamp are returned.
    async fn list_by_source_ref(
        &self,
        source_ref_prefix: &str,
        limit: usize,
        include_archived: bool,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>>;
    async fn get(&self, id: NoteId) -> Result<Option<Note>>;
    async fn count(&self) -> Result<i64>;
    async fn archive(&self, id: NoteId) -> Result<bool>;
    async fn supersede(&self, old_id: NoteId, new_id: NoteId) -> Result<bool>;
    async fn harvested_shas(&self) -> Result<HashSet<String>>;
    /// Check whether any entry already has the given full SHA in source_ref.
    async fn has_source_ref(&self, sha: &str) -> Result<bool>;
    /// Insert a directed edge between two notes.
    /// `kind` must be one of: supersedes, relates_to, contradicts.
    async fn add_edge(&self, from_id: i64, to_id: i64, kind: &str) -> Result<()>;
    /// Return `(outgoing, incoming)` edges for a note.
    async fn get_edges(&self, id: i64) -> Result<(Vec<MemoryEdge>, Vec<MemoryEdge>)>;

    /// Stable identifier for the concrete backend implementation, used for
    /// diagnostics (`spelunk status`/`check --format json`). One of:
    /// `"sqlite"`, `"git-notes"`, `"remote"`.
    fn backend_kind(&self) -> &'static str;
}

// ── Local SQLite backend ──────────────────────────────────────────────────────

/// Narrow an opaque [`NoteId`] to the integer a locally-keyed store uses.
///
/// The SQLite store keys on a rowid and the git-notes carrier keys on a
/// creation-time integer; neither can resolve a token minted elsewhere. A
/// non-numeric id here therefore means the caller aimed a cloud-minted id at a
/// local store, so the message says that rather than reporting the entry as
/// missing.
pub(crate) fn numeric_note_id(id: &NoteId) -> Result<i64> {
    id.as_i64().ok_or_else(|| {
        anyhow::anyhow!(
            "'{id}' is not an id this project's memory store can resolve: it numbers \
             entries with integers, and this id was minted by a cloud-hosted project. \
             Run `spelunk memory list` to see the ids this project actually uses."
        )
    })
}

/// Wraps `MemoryStore` in a `tokio::sync::Mutex` so `LocalMemoryBackend: Send + Sync`,
/// satisfying the `async-trait` Send constraint without needing spawn_blocking.
pub struct LocalMemoryBackend {
    store: tokio::sync::Mutex<MemoryStore>,
}

impl LocalMemoryBackend {
    pub fn new(store: MemoryStore) -> Self {
        Self {
            store: tokio::sync::Mutex::new(store),
        }
    }
}

#[async_trait]
impl MemoryBackend for LocalMemoryBackend {
    async fn add(&self, input: NoteInput) -> Result<(NoteId, bool)> {
        let store = self.store.lock().await;
        let tags: Vec<&str> = input.tags.iter().map(String::as_str).collect();
        let files: Vec<&str> = input.linked_files.iter().map(String::as_str).collect();
        let (id, created) = if let Some(supersedes_id) = input.supersedes {
            let supersedes_id = numeric_note_id(&supersedes_id)?;
            store.add_note_superseding(
                &input.kind,
                &input.title,
                &input.body,
                &tags,
                &files,
                input.valid_at,
                supersedes_id,
            )?
        } else {
            store.add_note(
                &input.kind,
                &input.title,
                &input.body,
                &tags,
                &files,
                input.source_ref.as_deref(),
                input.valid_at,
            )?
        };
        if let Some(blob) = &input.embedding {
            store.insert_embedding(id, blob)?;
        }
        Ok((NoteId::from_i64(id), created))
    }

    async fn search_timeline(
        &self,
        _query_blob: &[u8],
        query: &str,
        limit: usize,
    ) -> Result<Vec<Note>> {
        self.store.lock().await.search_timeline(query, limit)
    }

    async fn search(
        &self,
        query_blob: &[u8],
        _query: &str,
        limit: usize,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        self.store.lock().await.search(query_blob, limit, as_of)
    }

    async fn search_text(
        &self,
        query: &str,
        limit: usize,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        self.store.lock().await.search_text(query, limit, as_of)
    }

    async fn search_hybrid(
        &self,
        query_blob: &[u8],
        query: &str,
        limit: usize,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        self.store
            .lock()
            .await
            .search_hybrid(query_blob, query, limit, as_of)
    }

    async fn list(
        &self,
        kind_filter: Option<&str>,
        limit: usize,
        include_archived: bool,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        self.store
            .lock()
            .await
            .list_filtered(kind_filter, None, limit, include_archived, as_of)
    }

    async fn list_by_source_ref(
        &self,
        source_ref_prefix: &str,
        limit: usize,
        include_archived: bool,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        self.store.lock().await.list_filtered(
            None,
            Some(source_ref_prefix),
            limit,
            include_archived,
            as_of,
        )
    }

    async fn get(&self, id: NoteId) -> Result<Option<Note>> {
        self.store.lock().await.get(numeric_note_id(&id)?)
    }

    async fn count(&self) -> Result<i64> {
        self.store.lock().await.count()
    }

    async fn archive(&self, id: NoteId) -> Result<bool> {
        self.store.lock().await.archive(numeric_note_id(&id)?)
    }

    async fn supersede(&self, old_id: NoteId, new_id: NoteId) -> Result<bool> {
        let (old_id, new_id) = (numeric_note_id(&old_id)?, numeric_note_id(&new_id)?);
        self.store.lock().await.supersede(old_id, new_id)
    }

    async fn harvested_shas(&self) -> Result<HashSet<String>> {
        self.store.lock().await.harvested_shas()
    }

    async fn has_source_ref(&self, sha: &str) -> Result<bool> {
        self.store.lock().await.has_source_ref(sha)
    }

    async fn add_edge(&self, from_id: i64, to_id: i64, kind: &str) -> Result<()> {
        self.store.lock().await.add_edge(from_id, to_id, kind)
    }

    async fn get_edges(&self, id: i64) -> Result<(Vec<MemoryEdge>, Vec<MemoryEdge>)> {
        self.store.lock().await.get_edges(id)
    }

    fn backend_kind(&self) -> &'static str {
        "sqlite"
    }
}
