use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use spelunk_core::embeddings::blob_to_vec;

/// Shared state for all DB operations on the server.
pub struct ServerDb {
    pub conn: Connection,
    pub embedding_dim: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct Project {
    pub id: i64,
    pub slug: String,
    pub embedding_dim: usize,
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
    /// Cosine distance from query (only present in search results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f64>,
}

impl ServerDb {
    pub fn open(path: &std::path::Path, embedding_dim: usize) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening server db at {}", path.display()))?;
        // WAL mode for concurrent readers.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let db = Self {
            conn,
            embedding_dim,
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
    /// embedding dimension for subsequent validation.
    pub fn upsert_project(&self, slug: &str, incoming_dim: usize) -> Result<Project> {
        // Check if project exists.
        let existing: Option<Project> = self
            .conn
            .query_row(
                "SELECT id, slug, embedding_dim, created_at FROM projects WHERE slug = ?1",
                rusqlite::params![slug],
                row_to_project,
            )
            .optional()
            .context("querying project")?;

        if let Some(mut p) = existing {
            // Validate dimension if already set.
            if p.embedding_dim != 0 && p.embedding_dim != incoming_dim {
                anyhow::bail!(
                    "embedding dimension mismatch for project '{}': server expects {}, got {}. \
                     All clients on the same project must use the same embedding model.",
                    slug,
                    p.embedding_dim,
                    incoming_dim
                );
            }
            // Set dimension on first note.
            if p.embedding_dim == 0 {
                self.conn.execute(
                    "UPDATE projects SET embedding_dim = ?1 WHERE id = ?2",
                    rusqlite::params![incoming_dim as i64, p.id],
                )?;
                p.embedding_dim = incoming_dim;
            }
            Ok(p)
        } else {
            // Auto-create.
            self.conn.execute(
                "INSERT INTO projects (slug, embedding_dim) VALUES (?1, ?2)",
                rusqlite::params![slug, incoming_dim as i64],
            )?;
            let id = self.conn.last_insert_rowid();
            Ok(Project {
                id,
                slug: slug.to_string(),
                embedding_dim: incoming_dim,
                created_at: now_unix(),
            })
        }
    }

    pub fn get_project(&self, slug: &str) -> Result<Option<Project>> {
        self.conn
            .query_row(
                "SELECT id, slug, embedding_dim, created_at FROM projects WHERE slug = ?1",
                rusqlite::params![slug],
                row_to_project,
            )
            .optional()
            .context("querying project")
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, slug, embedding_dim, created_at FROM projects ORDER BY slug")?;
        let projects = stmt
            .query_map([], row_to_project)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(projects)
    }

    // ── Notes ─────────────────────────────────────────────────────────────────

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
    ) -> Result<i64> {
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
        self.conn.execute(
            "INSERT INTO notes (project_id, kind, title, body, tags, linked_files)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![project_id, kind, title, body, tags_csv, files_csv],
        )?;
        let note_id = self.conn.last_insert_rowid();

        if let Some(vec) = embedding {
            let blob = spelunk_core::embeddings::vec_to_blob(vec);
            self.conn.execute(
                "INSERT INTO note_embeddings (note_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![note_id, blob],
            )?;
        }
        Ok(note_id)
    }

    pub fn get_note(&self, project_id: i64, note_id: i64) -> Result<Option<ServerNote>> {
        self.conn
            .query_row(
                "SELECT id, kind, title, body, tags, linked_files, created_at, status, superseded_by
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
                        "SELECT id, kind, title, body, tags, linked_files, created_at, status, superseded_by
                         FROM notes WHERE project_id = ?1 AND kind = ?2 {status_clause}
                         ORDER BY created_at DESC LIMIT {limit}"
                    ),
                    vec![Box::new(project_id), Box::new(kind.to_string())],
                )
        } else {
            (
                    format!(
                        "SELECT id, kind, title, body, tags, linked_files, created_at, status, superseded_by
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
                    n.created_at, n.status, n.superseded_by, CAST(k.distance AS REAL)
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
                    n.created_at, n.status, n.superseded_by, CAST(k.distance AS REAL)
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
            "SELECT id, kind, title, body, tags, linked_files, created_at, status, superseded_by
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
        created_at: row.get(3)?,
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
        distance: Some(row.get(9)?),
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

    #[test]
    fn get_or_create_instance_id_is_stable_and_canonical() {
        // End-to-end through the public method against a real (in-memory) DB:
        // the persisted v7 UUID is returned verbatim and is stable across calls.
        register_sqlite_vec();
        let db = ServerDb::open(std::path::Path::new(":memory:"), 768)
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
}
