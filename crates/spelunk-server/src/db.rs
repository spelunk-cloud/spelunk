use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use spelunk_core::embeddings::blob_to_vec;

/// Typed error for an embedding-dimension mismatch on a project. Kept distinct
/// from `anyhow::Error` so callers (the HTTP layer) can map it to a safe,
/// specific 400 response without sniffing the error message for substrings —
/// see `AppError::BadRequest` in `lib.rs`.
#[derive(Debug)]
pub struct DimensionMismatch {
    pub slug: String,
    pub expected: usize,
    pub got: usize,
}

impl std::fmt::Display for DimensionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "embedding dimension mismatch for project '{}': server expects {}, got {}. \
             All clients on the same project must use the same embedding model.",
            self.slug, self.expected, self.got
        )
    }
}

impl std::error::Error for DimensionMismatch {}

/// Typed error for an embedding-model mismatch on a project — a same-dim
/// successor model that would silently corrupt the KNN space. Distinct from
/// `anyhow::Error` so the HTTP layer maps it to a 400 without message sniffing,
/// exactly as [`DimensionMismatch`] does (`AppError::Internal` in `lib.rs`).
#[derive(Debug)]
pub struct ModelMismatch {
    pub slug: String,
    pub expected: String,
    pub got: String,
}

impl std::fmt::Display for ModelMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "embedding model mismatch for project '{}': server expects '{}', got '{}'. \
             All clients on the same project must use the same embedding model; \
             a model change requires a deliberate re-index.",
            self.slug, self.expected, self.got
        )
    }
}

impl std::error::Error for ModelMismatch {}

/// Shared state for all DB operations on the server.
pub struct ServerDb {
    pub conn: Connection,
    pub embedding_dim: usize,
    /// Provenance id of the model this server embeds with; stamped onto and
    /// validated against each project alongside `embedding_dim`.
    pub embedding_model: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct Project {
    pub id: i64,
    pub slug: String,
    pub embedding_dim: usize,
    /// Embedding model provenance id; None on a legacy project not yet stamped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    /// Unix timestamp of project creation.
    pub created_at: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ServerNote {
    pub id: i64,
    /// Kind: `decision`, `requirement`, `note`, `question`, `handoff`, or `intent`.
    pub kind: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub linked_files: Vec<String>,
    /// Unix timestamp of creation.
    pub created_at: i64,
    /// `active` or `archived`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<i64>,
    /// Canonical cross-machine id (uuid). Optional and additive; `None` for
    /// rows never assigned one. Absent on the wire when `None`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub remote_id: Option<String>,
    /// Cosine distance from query (only present in search results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f64>,
}

/// A row from [`ServerDb::notes_since_id`] — the `since_id` cursor-mode
/// delta-pull query. Not directly serialized: `handlers::memory_since` shapes
/// this into the wire-level `SinceIdEntry`, formatting `created_at` as
/// RFC3339 and using `sync_id` as the entry's public `id` (never the note's
/// integer `id`, which has no meaning to a puller on a different machine).
#[derive(Debug)]
pub struct SinceIdRow {
    pub sync_id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    /// Extracted from a `git:<sha>` tag, the same convention `harvested_shas`
    /// reads (`source_commit` has no dedicated column on this schema).
    pub source_commit: Option<String>,
    pub created_at: i64,
}

impl ServerDb {
    pub fn open(
        path: &std::path::Path,
        embedding_dim: usize,
        embedding_model: &str,
    ) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening server db at {}", path.display()))?;
        // WAL mode for concurrent readers.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let db = Self {
            conn,
            embedding_dim,
            embedding_model: embedding_model.to_string(),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../migrations/server_001.sql"))
            .context("server migration 001")?;
        // Create the embeddings virtual table with the configured dimension.
        // IF NOT EXISTS means this is a no-op if the table already exists.
        self.conn
            .execute_batch(&format!(
                "CREATE VIRTUAL TABLE IF NOT EXISTS note_embeddings USING vec0(\
                    note_id INTEGER PRIMARY KEY, embedding FLOAT[{dim}]\
                )",
                dim = self.embedding_dim
            ))
            .context("creating note_embeddings virtual table")?;
        self.conn
            .execute_batch(include_str!("../migrations/server_002.sql"))
            .context("server migration 002")?;
        self.conn
            .execute_batch(include_str!("../migrations/server_003.sql"))
            .context("server migration 003")?;
        // 004 adds `notes.remote_id`. `ALTER TABLE ADD COLUMN` is not idempotent
        // in SQLite (errors if the column exists); swallow the error so re-open
        // is a no-op. The partial UNIQUE index in the same batch is created on
        // the first run and persists thereafter.
        let _ = self
            .conn
            .execute_batch(include_str!("../migrations/server_004.sql"));
        // Migration 005: embedding_model column. `ALTER TABLE` has no
        // `IF NOT EXISTS`; tolerate only the already-applied error.
        match self
            .conn
            .execute_batch(include_str!("../migrations/server_005.sql"))
        {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e).context("server migration 005"),
        }
        // Migration 006: re-scope the remote_id uniqueness to per-project.
        // `DROP INDEX IF EXISTS` + `CREATE UNIQUE INDEX IF NOT EXISTS` are both
        // naturally idempotent, so this can run unconditionally on every open.
        self.conn
            .execute_batch(include_str!("../migrations/server_006.sql"))
            .context("server migration 006")?;
        // Migration 007: sync_id (`ALTER TABLE ADD COLUMN`, not idempotent;
        // tolerate only the already-applied error, same as migration 005).
        match self
            .conn
            .execute_batch(include_str!("../migrations/server_007.sql"))
        {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e).context("server migration 007"),
        }
        self.backfill_missing_sync_ids()?;
        Ok(())
    }

    /// Assign a fresh `sync_id` to every row that predates migration 007.
    /// Idempotent and cheap once caught up (the `WHERE sync_id IS NULL` scan
    /// returns nothing). Runs unconditionally on every open so a legacy
    /// database is fully backfilled before any `since_id` cursor pull is
    /// served. Ordered by `id ASC` so the minted ids stay relatively ordered
    /// among themselves, matching original insertion order.
    fn backfill_missing_sync_ids(&self) -> Result<()> {
        let stale_ids: Vec<i64> = {
            let mut stmt = self
                .conn
                .prepare_cached("SELECT id FROM notes WHERE sync_id IS NULL ORDER BY id ASC")?;
            stmt.query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for id in stale_ids {
            self.conn.execute(
                "UPDATE notes SET sync_id = ?1 WHERE id = ?2",
                rusqlite::params![Uuid::now_v7().to_string(), id],
            )?;
        }
        Ok(())
    }

    /// Return the persistent instance UUID, creating one on first call.
    ///
    /// The id is a time-ordered UUID v7 minted by the `uuid` crate
    /// (`Uuid::now_v7`) and persisted verbatim, so it is stable for the life of
    /// the database. The high 48 bits carry the millisecond Unix timestamp of
    /// creation, which makes instance ids sort in creation order.
    pub fn get_or_create_instance_id(&self) -> Result<String> {
        // Mint a v7 UUID and persist it on first call; IGNORE keeps the
        // original id stable on every subsequent call.
        self.conn
            .execute(
                "INSERT OR IGNORE INTO server_meta(key, value) VALUES ('instance_id', ?1)",
                rusqlite::params![Uuid::now_v7().to_string()],
            )
            .context("seeding instance_id")?;
        let id: String = self
            .conn
            .query_row(
                "SELECT value FROM server_meta WHERE key = 'instance_id'",
                [],
                |row| row.get(0),
            )
            .context("reading instance_id")?;
        Ok(id)
    }

    // ── Projects ──────────────────────────────────────────────────────────────

    /// Get or auto-create a project by slug. On first write, records the
    /// embedding dimension and model for subsequent validation. `incoming_model`
    /// is the provenance id of the model producing this write's vectors.
    pub fn upsert_project(
        &self,
        slug: &str,
        incoming_dim: usize,
        incoming_model: &str,
    ) -> Result<Project> {
        // Check if project exists.
        let existing: Option<Project> = self
            .conn
            .query_row(
                "SELECT id, slug, embedding_dim, embedding_model, created_at FROM projects WHERE slug = ?1",
                rusqlite::params![slug],
                row_to_project,
            )
            .optional()
            .context("querying project")?;

        if let Some(mut p) = existing {
            // Validate dimension if already set.
            if p.embedding_dim != 0 && p.embedding_dim != incoming_dim {
                return Err(DimensionMismatch {
                    slug: slug.to_string(),
                    expected: p.embedding_dim,
                    got: incoming_dim,
                }
                .into());
            }
            // Validate model if already set; NULL = legacy, lazy-stamped below.
            if let Some(recorded) = &p.embedding_model
                && recorded != incoming_model
            {
                return Err(ModelMismatch {
                    slug: slug.to_string(),
                    expected: recorded.clone(),
                    got: incoming_model.to_string(),
                }
                .into());
            }
            // Set dimension on first note.
            if p.embedding_dim == 0 {
                self.conn.execute(
                    "UPDATE projects SET embedding_dim = ?1 WHERE id = ?2",
                    rusqlite::params![incoming_dim as i64, p.id],
                )?;
                p.embedding_dim = incoming_dim;
            }
            // Stamp model on first write (legacy/unstamped project).
            if p.embedding_model.is_none() {
                self.conn.execute(
                    "UPDATE projects SET embedding_model = ?1 WHERE id = ?2",
                    rusqlite::params![incoming_model, p.id],
                )?;
                p.embedding_model = Some(incoming_model.to_string());
            }
            Ok(p)
        } else {
            // Auto-create.
            self.conn.execute(
                "INSERT INTO projects (slug, embedding_dim, embedding_model) VALUES (?1, ?2, ?3)",
                rusqlite::params![slug, incoming_dim as i64, incoming_model],
            )?;
            let id = self.conn.last_insert_rowid();
            Ok(Project {
                id,
                slug: slug.to_string(),
                embedding_dim: incoming_dim,
                embedding_model: Some(incoming_model.to_string()),
                created_at: now_unix(),
            })
        }
    }

    pub fn get_project(&self, slug: &str) -> Result<Option<Project>> {
        self.conn
            .query_row(
                "SELECT id, slug, embedding_dim, embedding_model, created_at FROM projects WHERE slug = ?1",
                rusqlite::params![slug],
                row_to_project,
            )
            .optional()
            .context("querying project")
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, slug, embedding_dim, embedding_model, created_at FROM projects ORDER BY slug")?;
        let projects = stmt
            .query_map([], row_to_project)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(projects)
    }

    // ── Notes ─────────────────────────────────────────────────────────────────

    /// Returns `(note_id, sync_id)`: the local autoincrement row id, and the
    /// stable `sync_id` UUIDv7 minted for it. A caller that hands an id back
    /// across the wire (e.g. `push_memory_batch`'s ack) must use `sync_id`,
    /// not `note_id`: `/memory/since` cursors on `sync_id`, and a wire id that
    /// doesn't match what that endpoint returns breaks pull cursoring for
    /// whoever stores it.
    #[allow(clippy::too_many_arguments)]
    pub fn add_note(
        &self,
        project_id: i64,
        kind: &str,
        title: &str,
        body: &str,
        tags: &[String],
        linked_files: &[String],
        embedding: Option<&[f32]>,
        remote_id: Option<&str>,
    ) -> Result<(i64, String)> {
        let tags_csv = if tags.is_empty() {
            None
        } else {
            Some(tags.join(","))
        };
        let files_csv = if linked_files.is_empty() {
            None
        } else {
            Some(linked_files.join(","))
        };
        // `sync_id` is minted here unconditionally (server-side, arrival
        // order), independent of the caller-supplied `remote_id` (a pushing
        // client's own external_id, used only for push idempotency) — see
        // migration 007. Every insert path (single-note POST and batch push)
        // goes through this one function, so every new row gets one.
        let sync_id = Uuid::now_v7().to_string();
        self.conn.execute(
            "INSERT INTO notes (project_id, kind, title, body, tags, linked_files, remote_id, sync_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                project_id, kind, title, body, tags_csv, files_csv, remote_id, sync_id
            ],
        )?;
        let note_id = self.conn.last_insert_rowid();

        if let Some(vec) = embedding {
            let blob = spelunk_core::embeddings::vec_to_blob(vec);
            self.conn.execute(
                "INSERT INTO note_embeddings (note_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![note_id, blob],
            )?;
        }
        Ok((note_id, sync_id))
    }

    /// Bulk-lookup active notes by their cross-machine `remote_id` (the batch
    /// push idempotency key), returning each match's `sync_id` (not the row
    /// id: a caller acking a dedupe-hit back across the wire must hand out
    /// the same id `/memory/since` uses). Scoped to the project and to live
    /// rows only: an archived row with the same `remote_id` does not count as
    /// existing, so a re-push after archiving creates a fresh row rather than
    /// a no-op. Mirrors cloud-api's `find_by_external_ids`.
    pub fn find_by_remote_ids(
        &self,
        project_id: i64,
        remote_ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>> {
        if remote_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders = remote_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT remote_id, sync_id FROM notes
             WHERE project_id = ? AND status = 'active' AND remote_id IN ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut params: Vec<&dyn rusqlite::types::ToSql> = Vec::with_capacity(remote_ids.len() + 1);
        params.push(&project_id);
        for id in remote_ids {
            params.push(id);
        }
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (remote_id, sync_id) = row?;
            if let (Some(rid), Some(sid)) = (remote_id, sync_id) {
                map.insert(rid, sid);
            }
        }
        Ok(map)
    }

    pub fn get_note(&self, project_id: i64, note_id: i64) -> Result<Option<ServerNote>> {
        self.conn
            .query_row(
                "SELECT id, kind, title, body, tags, linked_files, created_at, status, superseded_by, remote_id
                 FROM notes WHERE id = ?1 AND project_id = ?2",
                rusqlite::params![note_id, project_id],
                row_to_note,
            )
            .optional()
            .context("querying note")
    }

    pub fn list_notes(
        &self,
        project_id: i64,
        kind_filter: Option<&str>,
        limit: usize,
        include_archived: bool,
    ) -> Result<Vec<ServerNote>> {
        let limit = limit.min(500);
        let status_clause = if include_archived {
            ""
        } else {
            "AND status = 'active'"
        };
        let (sql, params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = if let Some(kind) =
            kind_filter
        {
            (
                    format!(
                        "SELECT id, kind, title, body, tags, linked_files, created_at, status, superseded_by, remote_id
                         FROM notes WHERE project_id = ?1 AND kind = ?2 {status_clause}
                         ORDER BY created_at DESC LIMIT {limit}"
                    ),
                    vec![Box::new(project_id), Box::new(kind.to_string())],
                )
        } else {
            (
                    format!(
                        "SELECT id, kind, title, body, tags, linked_files, created_at, status, superseded_by, remote_id
                         FROM notes WHERE project_id = ?1 {status_clause}
                         ORDER BY created_at DESC LIMIT {limit}"
                    ),
                    vec![Box::new(project_id)],
                )
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let notes = stmt
            .query_map(refs.as_slice(), row_to_note)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(notes)
    }

    pub fn search_notes(
        &self,
        project_id: i64,
        query_vec: &[f32],
        limit: usize,
    ) -> Result<Vec<ServerNote>> {
        let limit = limit.min(100);
        let blob = spelunk_core::embeddings::vec_to_blob(query_vec);
        let sql = format!(
            "WITH knn AS (
                 SELECT note_id, distance
                 FROM   note_embeddings
                 WHERE  embedding MATCH ?1 AND k = {limit}
             )
             SELECT n.id, n.kind, n.title, n.body, n.tags, n.linked_files,
                    n.created_at, n.status, n.superseded_by, n.remote_id, CAST(k.distance AS REAL)
             FROM   knn k
             JOIN   notes n ON n.id = k.note_id
             WHERE  n.project_id = ?2 AND n.status = 'active'
             ORDER  BY k.distance"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let notes = stmt
            .query_map(
                rusqlite::params![blob, project_id],
                row_to_note_with_distance,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(notes)
    }

    /// Search for existing active notes that are semantically close to the given embedding.
    /// Returns notes with cosine distance ≤ `max_distance` (i.e. similarity ≥ `1 - max_distance`),
    /// excluding `exclude_id` (the note just written).
    pub fn search_notes_for_conflicts(
        &self,
        project_id: i64,
        query_vec: &[f32],
        max_distance: f32,
        exclude_id: i64,
        limit: usize,
    ) -> Result<Vec<ServerNote>> {
        let limit = limit.min(50);
        let blob = spelunk_core::embeddings::vec_to_blob(query_vec);
        // We search with a generous k (limit + 1 for the excluded entry) and filter in Rust.
        let search_limit = limit + 1;
        let sql = format!(
            "WITH knn AS (
                 SELECT note_id, distance
                 FROM   note_embeddings
                 WHERE  embedding MATCH ?1 AND k = {search_limit}
             )
             SELECT n.id, n.kind, n.title, n.body, n.tags, n.linked_files,
                    n.created_at, n.status, n.superseded_by, n.remote_id, CAST(k.distance AS REAL)
             FROM   knn k
             JOIN   notes n ON n.id = k.note_id
             WHERE  n.project_id = ?2
               AND  n.status = 'active'
               AND  n.id != ?3
               AND  k.distance <= ?4
             ORDER  BY k.distance
             LIMIT  {limit}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let notes = stmt
            .query_map(
                rusqlite::params![blob, project_id, exclude_id, max_distance as f64],
                row_to_note_with_distance,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(notes)
    }

    /// Insert a directed edge between two server notes.
    /// `kind` must be one of: supersedes, relates_to, contradicts.
    pub fn add_edge(&self, from_id: i64, to_id: i64, kind: &str) -> Result<()> {
        const VALID_KINDS: &[&str] = &["supersedes", "relates_to", "contradicts"];
        if !VALID_KINDS.contains(&kind) {
            anyhow::bail!(
                "invalid edge kind '{kind}'; must be one of: supersedes, relates_to, contradicts"
            );
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO note_edges (from_id, to_id, kind) VALUES (?1, ?2, ?3)",
            rusqlite::params![from_id, to_id, kind],
        )?;
        Ok(())
    }

    /// Return all git SHAs stored in tags for a project.
    ///
    /// Tags are stored as comma-separated strings; each SHA is stored as `git:<sha>`.
    /// Used by the client's `harvested_shas()` to avoid re-harvesting commits.
    pub fn harvested_shas(&self, project_id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT tags FROM notes WHERE project_id = ?1 AND tags LIKE '%git:%'",
        )?;
        let rows = stmt.query_map(rusqlite::params![project_id], |r| {
            r.get::<_, Option<String>>(0)
        })?;
        let mut shas = Vec::new();
        for row in rows {
            if let Some(tags) = row? {
                for tag in tags.split(',').map(str::trim) {
                    if let Some(sha) = tag.strip_prefix("git:") {
                        shas.push(sha.to_string());
                    }
                }
            }
        }
        Ok(shas)
    }

    pub fn archive_note(&self, project_id: i64, note_id: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE notes SET status = 'archived' WHERE id = ?1 AND project_id = ?2 AND status = 'active'",
            rusqlite::params![note_id, project_id],
        )?;
        Ok(changed > 0)
    }

    pub fn supersede_note(&self, project_id: i64, old_id: i64, new_id: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE notes SET status = 'archived', superseded_by = ?3
             WHERE id = ?1 AND project_id = ?2 AND status = 'active'",
            rusqlite::params![old_id, project_id, new_id],
        )?;
        Ok(changed > 0)
    }

    /// Return notes created after `since_secs` (exclusive), ordered ASC by `created_at`.
    /// Archived entries are excluded. `limit` is capped at 500.
    pub fn notes_since(
        &self,
        project_id: i64,
        since_secs: i64,
        limit: i64,
    ) -> Result<Vec<ServerNote>> {
        let limit = limit.clamp(1, 500);
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, kind, title, body, tags, linked_files, created_at, status, superseded_by, remote_id
             FROM notes
             WHERE project_id = ?1 AND created_at > ?2 AND status != 'archived'
             ORDER BY created_at ASC
             LIMIT ?3",
        )?;
        let notes = stmt
            .query_map(
                rusqlite::params![project_id, since_secs, limit],
                row_to_note,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(notes)
    }

    /// Return notes whose `sync_id` sorts strictly after `cursor` (a UUIDv7
    /// string), ordered ASC by `sync_id`. `sync_id` is server-minted at
    /// insert time (see `add_note`), so this ordering reflects arrival order
    /// at this server — unlike `remote_id`, which is a pushing client's own
    /// external_id and cannot be trusted to sort consistently across
    /// clients. Archived entries are excluded, matching `notes_since`'s
    /// behaviour: tombstone propagation over a team server is a known,
    /// separate gap, not covered by this cursor. `limit` is capped at 500.
    pub fn notes_since_id(
        &self,
        project_id: i64,
        cursor: &str,
        limit: i64,
    ) -> Result<Vec<SinceIdRow>> {
        let limit = limit.clamp(1, 500);
        let mut stmt = self.conn.prepare_cached(
            "SELECT sync_id, kind, title, body, tags, created_at
             FROM notes
             WHERE project_id = ?1 AND status != 'archived'
                   AND sync_id IS NOT NULL AND sync_id > ?2
             ORDER BY sync_id ASC
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![project_id, cursor, limit],
                row_to_since_id_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_note(&self, project_id: i64, note_id: i64) -> Result<bool> {
        self.conn.execute(
            "DELETE FROM note_embeddings WHERE note_id = ?1",
            rusqlite::params![note_id],
        )?;
        let changed = self.conn.execute(
            "DELETE FROM notes WHERE id = ?1 AND project_id = ?2",
            rusqlite::params![note_id, project_id],
        )?;
        Ok(changed > 0)
    }

    pub fn stats(&self, project_id: i64) -> Result<ProjectStats> {
        let total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM notes WHERE project_id = ?1",
            rusqlite::params![project_id],
            |r| r.get(0),
        )?;
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM notes WHERE project_id = ?1 AND status = 'active'",
            rusqlite::params![project_id],
            |r| r.get(0),
        )?;
        Ok(ProjectStats {
            count,
            total,
            embedding_dim: self.embedding_dim,
        })
    }
}

#[derive(Serialize, ToSchema)]
pub struct ProjectStats {
    /// Number of active memory entries.
    pub count: i64,
    /// Total entries including archived.
    pub total: i64,
    pub embedding_dim: usize,
}

// ── Row mappers ──────────────────────────────────────────────────────────────

fn row_to_project(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        slug: row.get(1)?,
        embedding_dim: row.get::<_, i64>(2)? as usize,
        embedding_model: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn split_csv(s: Option<&str>) -> Vec<String> {
    s.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn row_to_note(row: &rusqlite::Row<'_>) -> rusqlite::Result<ServerNote> {
    Ok(ServerNote {
        id: row.get(0)?,
        kind: row.get(1)?,
        title: row.get(2)?,
        body: row.get(3)?,
        tags: split_csv(row.get::<_, Option<String>>(4)?.as_deref()),
        linked_files: split_csv(row.get::<_, Option<String>>(5)?.as_deref()),
        created_at: row.get(6)?,
        status: row.get(7)?,
        superseded_by: row.get(8)?,
        remote_id: row.get(9)?,
        distance: None,
    })
}

fn row_to_note_with_distance(row: &rusqlite::Row<'_>) -> rusqlite::Result<ServerNote> {
    Ok(ServerNote {
        id: row.get(0)?,
        kind: row.get(1)?,
        title: row.get(2)?,
        body: row.get(3)?,
        tags: split_csv(row.get::<_, Option<String>>(4)?.as_deref()),
        linked_files: split_csv(row.get::<_, Option<String>>(5)?.as_deref()),
        created_at: row.get(6)?,
        status: row.get(7)?,
        superseded_by: row.get(8)?,
        remote_id: row.get(9)?,
        distance: Some(row.get(10)?),
    })
}

/// Extract a `git:<sha>` tag's sha, mirroring `harvested_shas`'s parsing of
/// the same tag convention. Returns the first match, if any.
fn source_commit_from_tags(tags: &[String]) -> Option<String> {
    tags.iter()
        .find_map(|t| t.strip_prefix("git:").map(str::to_string))
}

fn row_to_since_id_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SinceIdRow> {
    let tags = split_csv(row.get::<_, Option<String>>(4)?.as_deref());
    Ok(SinceIdRow {
        sync_id: row.get(0)?,
        kind: row.get(1)?,
        title: row.get(2)?,
        body: row.get(3)?,
        source_commit: source_commit_from_tags(&tags),
        created_at: row.get(5)?,
    })
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// Need blob_to_vec for search — it's in embeddings module so the import works.
// But we also need vec_to_blob for the vec0 match query. Both are in spelunk_core::embeddings.
impl ServerDb {
    /// Convenience: decode a raw embedding blob to f32 vec for use with search_notes.
    pub fn decode_embedding(blob: &[u8]) -> Vec<f32> {
        blob_to_vec(blob)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Register the sqlite-vec extension once per test process. `ServerDb::open`
    /// creates a `vec0` virtual table, so the extension must be auto-registered
    /// before any in-memory DB is opened. `sqlite3_auto_extension` is
    /// process-global, hence the `OnceLock` guard.
    fn register_sqlite_vec() {
        use std::sync::OnceLock;
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )));
            }
        });
    }

    // Assert that a UUID string is in canonical 8-4-4-4-12 form and every
    // non-dash character is a lowercase hex digit. This mirrors what the rest
    // of the codebase expects of instance_id (a 36-char UUID, see the health
    // handler in handlers.rs).
    fn assert_canonical_uuid(id: &str) {
        assert_eq!(id.len(), 36, "instance_id must be 36 chars: {id}");
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "must be 8-4-4-4-12 grouped: {id}"
        );
        for n in id.bytes().filter(|&c| c != b'-') {
            assert!(
                n.is_ascii_hexdigit() && !n.is_ascii_uppercase(),
                "all chars must be lowercase hex: {id}"
            );
        }
    }

    /// Re-opening a persistent DB re-runs `migrate()`; migration 004's
    /// `ALTER TABLE ADD COLUMN remote_id` is not idempotent in SQLite, so the
    /// second open must not fail. Also confirms `remote_id` is queryable and
    /// defaults to NULL on existing rows.
    #[test]
    fn reopen_is_idempotent_and_remote_id_defaults_null() {
        register_sqlite_vec();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("server.db");

        // First open creates the schema and inserts a note.
        {
            let db = ServerDb::open(&path, 768, "test-model").expect("first open");
            let project = db
                .upsert_project("acme/widget", 768, "test-model")
                .expect("project");
            db.add_note(project.id, "note", "t", "b", &[], &[], None, None)
                .expect("add note");
        }

        // Second open re-runs every migration, including the non-idempotent
        // ALTER; it must succeed and the row must read back with remote_id NULL.
        let db = ServerDb::open(&path, 768, "test-model").expect("reopen must not fail");
        let project = db.get_project("acme/widget").expect("get").expect("exists");
        let notes = db.list_notes(project.id, None, 10, true).expect("list");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].remote_id, None, "existing row defaults to NULL");
    }

    /// Migration 006 must re-scope `remote_id` uniqueness to per-project: two
    /// different projects reusing the same `remote_id` must both succeed at
    /// the DB layer. Migration 004 indexed `remote_id` alone (global), which
    /// collided here even though `find_by_remote_ids`'s idempotency lookup was
    /// always scoped to `project_id`.
    #[test]
    fn remote_id_uniqueness_is_scoped_per_project_not_global() {
        register_sqlite_vec();
        let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "test-model")
            .expect("open in-memory server db");
        let proj_a = db
            .upsert_project("team/a", 4, "test-model")
            .expect("proj a");
        let proj_b = db
            .upsert_project("team/b", 4, "test-model")
            .expect("proj b");

        db.add_note(
            proj_a.id,
            "note",
            "A",
            "body",
            &[],
            &[],
            None,
            Some("shared-ext-id"),
        )
        .expect("first project may claim the remote_id");
        db.add_note(
            proj_b.id,
            "note",
            "B",
            "body",
            &[],
            &[],
            None,
            Some("shared-ext-id"),
        )
        .expect(
            "a different project reusing the same remote_id must not collide \
                 with project A's row (global unique index regression)",
        );

        // Each project's own lookup only ever sees its own row.
        let found_a = db
            .find_by_remote_ids(proj_a.id, &["shared-ext-id".to_string()])
            .expect("lookup a");
        let found_b = db
            .find_by_remote_ids(proj_b.id, &["shared-ext-id".to_string()])
            .expect("lookup b");
        assert_eq!(found_a.len(), 1);
        assert_eq!(found_b.len(), 1);
        assert_ne!(
            found_a["shared-ext-id"], found_b["shared-ext-id"],
            "the two projects' rows must be distinct notes"
        );
    }

    /// Within the SAME project, `remote_id` uniqueness must still be enforced
    /// at the DB layer (the invariant migration 004 set out to establish is
    /// not lost by narrowing its scope in migration 006).
    #[test]
    fn remote_id_uniqueness_still_enforced_within_same_project() {
        register_sqlite_vec();
        let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "test-model")
            .expect("open in-memory server db");
        let project = db.upsert_project("team/a", 4, "test-model").expect("proj");

        db.add_note(
            project.id,
            "note",
            "A",
            "body",
            &[],
            &[],
            None,
            Some("dup-id"),
        )
        .expect("first insert succeeds");
        let err = db
            .add_note(
                project.id,
                "note",
                "A2",
                "body",
                &[],
                &[],
                None,
                Some("dup-id"),
            )
            .expect_err("same project, same remote_id must still violate the unique index");
        assert!(
            err.to_string().to_lowercase().contains("unique"),
            "must fail on the unique constraint, not some other error: {err}"
        );
    }

    /// `find_by_remote_ids` is active-only: an archived note's `remote_id`
    /// does not count as "existing", so a re-push after archiving creates a
    /// fresh live row (matches cloud-api's `archived_at IS NULL` filter in
    /// `find_by_external_ids`).
    #[test]
    fn find_by_remote_ids_ignores_archived_notes() {
        register_sqlite_vec();
        let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "test-model")
            .expect("open in-memory server db");
        let project = db.upsert_project("team/a", 4, "test-model").expect("proj");

        let (id, sync_id) = db
            .add_note(
                project.id,
                "note",
                "A",
                "body",
                &[],
                &[],
                None,
                Some("archived-id"),
            )
            .expect("insert");
        let found = db
            .find_by_remote_ids(project.id, &["archived-id".to_string()])
            .expect("lookup before archive");
        assert_eq!(
            found.get("archived-id"),
            Some(&sync_id),
            "live note must be found"
        );

        db.archive_note(project.id, id).expect("archive");
        let found_after = db
            .find_by_remote_ids(project.id, &["archived-id".to_string()])
            .expect("lookup after archive");
        assert!(
            found_after.is_empty(),
            "archived note's remote_id must not count as existing: {found_after:?}"
        );
    }

    /// `find_by_remote_ids` must scope to `project_id`: a note in a different
    /// project with the same `remote_id` string must never appear in another
    /// project's idempotency lookup.
    #[test]
    fn find_by_remote_ids_scopes_to_project() {
        register_sqlite_vec();
        let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "test-model")
            .expect("open in-memory server db");
        let proj_a = db
            .upsert_project("team/a", 4, "test-model")
            .expect("proj a");
        let proj_b = db
            .upsert_project("team/b", 4, "test-model")
            .expect("proj b");
        db.add_note(
            proj_a.id,
            "note",
            "A",
            "body",
            &[],
            &[],
            None,
            Some("cross-id"),
        )
        .expect("insert into project a");

        let found_in_b = db
            .find_by_remote_ids(proj_b.id, &["cross-id".to_string()])
            .expect("lookup scoped to project b");
        assert!(
            found_in_b.is_empty(),
            "project a's note must not leak into project b's idempotency lookup: {found_in_b:?}"
        );
    }

    #[test]
    fn get_or_create_instance_id_is_stable_and_canonical() {
        // End-to-end through the public method against a real (in-memory) DB:
        // the persisted v7 UUID is returned verbatim and is stable across calls.
        register_sqlite_vec();
        let db = ServerDb::open(std::path::Path::new(":memory:"), 768, "test-model")
            .expect("open in-memory server db");

        let id1 = db.get_or_create_instance_id().expect("first instance_id");
        assert_canonical_uuid(&id1);
        // Parses as a real version-7 (time-ordered random) UUID minted by the
        // `uuid` crate.
        let parsed = Uuid::parse_str(&id1).expect("instance_id must parse as a UUID");
        assert_eq!(parsed.get_version(), Some(uuid::Version::SortRand));

        // INSERT OR IGNORE keeps the original id, so the whole value is stable.
        let id2 = db.get_or_create_instance_id().expect("second instance_id");
        assert_eq!(id1, id2, "instance_id must be stable across calls");
    }

    /// A same-dim write with a different model id returns the typed
    /// `ModelMismatch`, which the HTTP layer maps to a 400 (see `lib.rs`).
    #[test]
    fn upsert_project_model_mismatch_is_typed_error() {
        register_sqlite_vec();
        let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "model-a")
            .expect("open in-memory server db");
        db.upsert_project("proj", 4, "model-a")
            .expect("first upsert stamps model");
        let err = db
            .upsert_project("proj", 4, "model-b")
            .expect_err("same dim, different model must error");
        let mismatch = err
            .downcast_ref::<ModelMismatch>()
            .expect("error must be the typed ModelMismatch");
        assert_eq!(mismatch.expected, "model-a");
        assert_eq!(mismatch.got, "model-b");
    }

    /// A legacy project row with NULL `embedding_model` is lazy-stamped on the
    /// next write rather than rejected.
    #[test]
    fn upsert_project_null_model_is_lazy_stamped() {
        register_sqlite_vec();
        let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "model-a")
            .expect("open in-memory server db");
        // Simulate a legacy row created before the embedding_model column.
        db.conn
            .execute(
                "INSERT INTO projects (slug, embedding_dim, embedding_model) VALUES ('legacy', 4, NULL)",
                [],
            )
            .expect("seed legacy project");
        let p = db
            .upsert_project("legacy", 4, "model-a")
            .expect("null model lazy-stamps without rejecting");
        assert_eq!(p.embedding_model.as_deref(), Some("model-a"));
    }

    // ── sync_id backfill + notes_since_id cursoring ─────────────────────────

    /// A note created via `add_note` (the single-note POST path, `remote_id =
    /// NULL`) still gets a `sync_id` — minted unconditionally at insert time,
    /// independent of `remote_id` — and is retrievable via the `since_id`
    /// cursor from the nil cursor.
    #[test]
    fn add_note_mints_sync_id_even_with_no_remote_id() {
        register_sqlite_vec();
        let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "test-model")
            .expect("open in-memory server db");
        let project = db
            .upsert_project("acme/widget", 4, "test-model")
            .expect("project");
        db.add_note(project.id, "note", "T", "B", &[], &[], None, None)
            .expect("add note with no remote_id");

        let rows = db
            .notes_since_id(project.id, "00000000-0000-0000-0000-000000000000", 100)
            .expect("cursor query from nil");
        assert_eq!(rows.len(), 1, "the remote_id=NULL note must still surface");
        assert_eq!(rows[0].title, "T");
        assert_canonical_uuid(&rows[0].sync_id);
    }

    /// A pre-existing row created before migration 007 (simulated by
    /// inserting directly with `sync_id` left NULL) is backfilled with a
    /// fresh `sync_id` on the next open, so legacy data is not stranded
    /// outside the `since_id` cursor.
    #[test]
    fn reopen_backfills_sync_id_for_legacy_rows() {
        register_sqlite_vec();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("server.db");

        {
            let db = ServerDb::open(&path, 4, "test-model").expect("first open");
            let project = db
                .upsert_project("acme/widget", 4, "test-model")
                .expect("project");
            // Simulate a genuinely legacy row: insert directly, bypassing
            // add_note (which now always mints a sync_id), to model a note
            // written before migration 007 existed.
            db.conn
                .execute(
                    "INSERT INTO notes (project_id, kind, title, body) VALUES (?1, 'note', 'legacy', 'b')",
                    rusqlite::params![project.id],
                )
                .expect("seed legacy row with no sync_id");
        }

        // Reopening re-runs migrate(), which must backfill the stray NULL.
        let db = ServerDb::open(&path, 4, "test-model").expect("reopen");
        let project = db.get_project("acme/widget").expect("get").expect("exists");
        let rows = db
            .notes_since_id(project.id, "00000000-0000-0000-0000-000000000000", 100)
            .expect("cursor query after backfill");
        assert_eq!(rows.len(), 1, "the backfilled legacy row must surface");
        assert_eq!(rows[0].title, "legacy");
        assert_canonical_uuid(&rows[0].sync_id);
    }

    /// `notes_since_id` orders by `sync_id` (arrival order at this server),
    /// and the cursor is exclusive: re-querying with the last-seen `sync_id`
    /// returns nothing further, and paging from an intermediate cursor skips
    /// exactly the entries already seen.
    #[test]
    fn notes_since_id_cursor_is_exclusive_and_orders_by_arrival() {
        register_sqlite_vec();
        let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "test-model")
            .expect("open in-memory server db");
        let project = db
            .upsert_project("acme/widget", 4, "test-model")
            .expect("project");
        for title in ["first", "second", "third"] {
            db.add_note(project.id, "note", title, "b", &[], &[], None, None)
                .expect("add note");
            // `sync_id` is a UUIDv7 minted per-call with no cross-call
            // monotonic counter guaranteed; force millisecond separation so
            // this test's arrival-order assertion isn't a same-millisecond
            // coin flip.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let nil = "00000000-0000-0000-0000-000000000000";
        let all = db
            .notes_since_id(project.id, nil, 100)
            .expect("full catch-up");
        assert_eq!(all.len(), 3);
        let titles: Vec<&str> = all.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["first", "second", "third"],
            "must be ordered by arrival (sync_id ASC), matching insertion order"
        );

        // Cursor on the first entry's sync_id returns only what came after.
        let after_first = db
            .notes_since_id(project.id, &all[0].sync_id, 100)
            .expect("cursor after first");
        assert_eq!(
            after_first
                .iter()
                .map(|r| r.title.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "third"]
        );

        // Cursor on the last entry's sync_id returns nothing (exclusive, no
        // re-delivery of the boundary row).
        let after_last = db
            .notes_since_id(project.id, &all[2].sync_id, 100)
            .expect("cursor after last");
        assert!(
            after_last.is_empty(),
            "the boundary entry itself must not be re-delivered: {after_last:?}"
        );
    }

    /// `notes_since_id` excludes archived entries, matching `notes_since`'s
    /// existing behaviour (tombstone propagation over a team server is a
    /// separate, not-yet-implemented gap).
    #[test]
    fn notes_since_id_excludes_archived_entries() {
        register_sqlite_vec();
        let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "test-model")
            .expect("open in-memory server db");
        let project = db
            .upsert_project("acme/widget", 4, "test-model")
            .expect("project");
        let (id, _sync_id) = db
            .add_note(
                project.id,
                "note",
                "will-archive",
                "b",
                &[],
                &[],
                None,
                None,
            )
            .expect("add note");
        db.archive_note(project.id, id).expect("archive");

        let nil = "00000000-0000-0000-0000-000000000000";
        let rows = db
            .notes_since_id(project.id, nil, 100)
            .expect("cursor query");
        assert!(
            rows.is_empty(),
            "an archived note must not appear in the since_id cursor: {rows:?}"
        );
    }

    /// `notes_since_id` surfaces `source_commit` extracted from the
    /// `git:<sha>` tag convention, mirroring how `push_memory_batch` stores
    /// it and `harvested_shas` reads it back.
    #[test]
    fn notes_since_id_extracts_source_commit_from_git_tag() {
        register_sqlite_vec();
        let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "test-model")
            .expect("open in-memory server db");
        let project = db
            .upsert_project("acme/widget", 4, "test-model")
            .expect("project");
        db.add_note(
            project.id,
            "decision",
            "T",
            "B",
            &["git:deadbeef".to_string()],
            &[],
            None,
            None,
        )
        .expect("add note with git tag");

        let nil = "00000000-0000-0000-0000-000000000000";
        let rows = db
            .notes_since_id(project.id, nil, 100)
            .expect("cursor query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source_commit.as_deref(), Some("deadbeef"));
    }
}
