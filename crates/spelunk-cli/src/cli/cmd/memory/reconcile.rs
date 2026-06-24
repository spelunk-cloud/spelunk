//! `spelunk memory reconcile` — import unique notes from server.db into memory.db.
//!
//! Discovers notes that exist in the local daemon's `server.db` but are absent
//! from `memory.db` for the active project, and imports them in a single
//! all-or-nothing transaction.  Dedup is by computed content hash, not rowid.
//!
//! See issue #391 and ADR-004 for the full interface contract.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;

use super::MemoryReconcileArgs;
use crate::{config::Config, server_client::ServerInferenceClient, storage::MemoryStore};

// ── Content-hash ──────────────────────────────────────────────────────────────

/// Compute the stable identity hash for a note.
///
/// Hash input: `kind \x1f title \x1f body \x1f normalize(tags) \x1f normalize(linked_files) \x1f created_at`
///
/// `normalize(csv)`: split on `,`, trim, drop empties, sort, rejoin with `,`.
/// `created_at` is included so two distinct notes with identical text don't collapse.
/// `status` and `superseded_by` are excluded so an archived note still matches its twin.
fn content_hash(
    kind: &str,
    title: &str,
    body: &str,
    tags_csv: &str,
    files_csv: &str,
    created_at: i64,
) -> blake3::Hash {
    let tags = normalize_csv(tags_csv);
    let files = normalize_csv(files_csv);
    let created_str = created_at.to_string();
    let mut hasher = blake3::Hasher::new();
    for part in [kind, title, body, &tags, &files, &created_str] {
        hasher.update(part.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.finalize()
}

fn normalize_csv(csv: &str) -> String {
    let mut parts: Vec<&str> = csv
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    parts.sort_unstable();
    parts.join(",")
}

// ── Candidate row from server.db ──────────────────────────────────────────────

/// A row read from `server.db`.
///
/// `superseded_by` carries the server-local rowid.  It is NOT portable across
/// stores; `build_supersede_map` re-reads server.db to resolve the links by
/// rowid and maps them back to candidate indices.  The field is kept here for
/// status-detection: if a note's status is `archived` we set it archived on
/// import regardless of whether we can resolve the successor.
#[derive(Debug, Clone)]
struct ServerNote {
    kind: String,
    title: String,
    body: String,
    tags: String, // raw CSV as stored in server.db
    linked_files: String,
    created_at: i64,
    status: String,
    #[allow(dead_code)] // resolved via SQL in build_supersede_map, not field access
    superseded_by: Option<i64>,
}

impl ServerNote {
    fn hash(&self) -> blake3::Hash {
        content_hash(
            &self.kind,
            &self.title,
            &self.body,
            &self.tags,
            &self.linked_files,
            self.created_at,
        )
    }
}

// ── Summary / NDJSON reporting ────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ReconcileError {
    stage: String,
    message: String,
}

#[derive(Debug, Serialize)]
struct ReconcileSummary {
    source_db: String,
    project_slug: String,
    candidates: usize,
    already_present: usize,
    imported: usize,
    would_import: usize,
    imported_without_embedding: usize,
    skipped_archived_supersede_unresolved: usize,
    errors: Vec<ReconcileError>,
}

// ── Main entry point ──────────────────────────────────────────────────────────

pub(super) async fn memory_reconcile(
    args: MemoryReconcileArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
) -> Result<()> {
    let json = crate::utils::effective_format(&args.format) == "json";

    // Resolve source server.db path.
    let server_db_path = args
        .source_db
        .clone()
        .unwrap_or_else(default_server_db_path);

    // If server.db doesn't exist — no-op success.
    if !server_db_path.exists() {
        let summary = ReconcileSummary {
            source_db: server_db_path.display().to_string(),
            project_slug: String::new(),
            candidates: 0,
            already_present: 0,
            imported: 0,
            would_import: 0,
            imported_without_embedding: 0,
            skipped_archived_supersede_unresolved: 0,
            errors: vec![],
        };
        emit_summary(&summary, json, args.dry_run);
        return Ok(());
    }

    // Resolve project slug.
    let project_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let slug = cfg.resolve_project_id(&project_root);

    if args.all_projects {
        run_all_projects(&server_db_path, mem_path, cfg, &args, json).await
    } else {
        let result = reconcile_project(&slug, &server_db_path, mem_path, cfg, &args, json).await;
        // Non-zero exit on fault already handled inside reconcile_project via ?
        result
    }
}

async fn run_all_projects(
    server_db_path: &std::path::Path,
    mem_path: &std::path::Path,
    cfg: &Config,
    args: &MemoryReconcileArgs,
    json: bool,
) -> Result<()> {
    let slugs = list_server_project_slugs(server_db_path)?;
    if slugs.is_empty() {
        if !json {
            eprintln!("[spelunk] No projects found in server.db.");
        }
        return Ok(());
    }
    for slug in &slugs {
        reconcile_project(slug, server_db_path, mem_path, cfg, args, json).await?;
    }
    Ok(())
}

fn list_server_project_slugs(server_db_path: &std::path::Path) -> Result<Vec<String>> {
    let conn = open_server_db_readonly(server_db_path)?;
    let mut stmt = conn.prepare("SELECT slug FROM projects ORDER BY id")?;
    let slugs: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(slugs)
}

async fn reconcile_project(
    slug: &str,
    server_db_path: &std::path::Path,
    mem_path: &std::path::Path,
    cfg: &Config,
    args: &MemoryReconcileArgs,
    json: bool,
) -> Result<()> {
    let source_db_str = server_db_path.display().to_string();

    let mut summary = ReconcileSummary {
        source_db: source_db_str.clone(),
        project_slug: slug.to_string(),
        candidates: 0,
        already_present: 0,
        imported: 0,
        would_import: 0,
        imported_without_embedding: 0,
        skipped_archived_supersede_unresolved: 0,
        errors: vec![],
    };

    // ── Step 1: open server.db read-only, look up project_id ────────────────
    let server_conn = match open_server_db_readonly(server_db_path) {
        Ok(c) => c,
        Err(e) => {
            summary.errors.push(ReconcileError {
                stage: "open_source_db".to_string(),
                message: format!("{e:#}"),
            });
            emit_summary(&summary, json, args.dry_run);
            anyhow::bail!("could not open source db: {e:#}");
        }
    };

    let project_id: Option<i64> = server_conn
        .query_row(
            "SELECT id FROM projects WHERE slug = ?1",
            rusqlite::params![slug],
            |r| r.get(0),
        )
        .optional()
        .context("querying projects table in server.db")?;

    let Some(project_id) = project_id else {
        // Project not present in server.db — no-op.
        emit_summary(&summary, json, args.dry_run);
        return Ok(());
    };

    // ── Step 2: read candidate rows from server.db ───────────────────────────
    let candidates =
        read_server_notes(&server_conn, project_id).context("reading notes from server.db")?;
    drop(server_conn); // release read connection; we no longer need server.db

    summary.candidates = candidates.len();

    if candidates.is_empty() {
        emit_summary(&summary, json, args.dry_run);
        return Ok(());
    }

    // ── Step 3: open memory.db, compute existing content-hash set ────────────
    let mem_store = MemoryStore::open(mem_path)
        .with_context(|| format!("opening memory.db at {}", mem_path.display()))?;

    let existing_notes = mem_store
        .all_notes_for_dedup()
        .context("reading existing memory.db notes for dedup")?;

    let existing_hashes: std::collections::HashSet<String> = existing_notes
        .iter()
        .map(|n| {
            content_hash(
                &n.kind,
                &n.title,
                &n.body,
                &n.tags.join(","),
                &n.linked_files.join(","),
                n.created_at,
            )
            .to_hex()
            .to_string()
        })
        .collect();

    // ── Step 4: build reconcile set (source rows not in memory.db) ───────────
    // Sort by created_at ASC to preserve supersede chains.
    let mut to_import: Vec<&ServerNote> = candidates
        .iter()
        .filter(|c| !existing_hashes.contains(&c.hash().to_hex().to_string()))
        .collect();
    to_import.sort_by_key(|n| n.created_at);

    summary.already_present = candidates.len() - to_import.len();

    if to_import.is_empty() {
        emit_summary(&summary, json, args.dry_run);
        return Ok(());
    }

    // Dry-run: report and stop.
    if args.dry_run {
        summary.would_import = to_import.len();
        emit_summary(&summary, json, args.dry_run);
        return Ok(());
    }

    // ── Step 5: embed via server (best-effort) ───────────────────────────────
    // Attempt to obtain an embedding for each candidate; missing = None.
    let embed_client = ServerInferenceClient::from_config(cfg);

    // Build embeddings upfront so we don't embed inside the transaction.
    let mut embeddings: Vec<Option<Vec<u8>>> = Vec::with_capacity(to_import.len());
    for note in &to_import {
        let text = format!("title: {} | text: {}", note.title, note.body);
        let blob = try_embed(&embed_client, &text).await;
        if blob.is_none() {
            summary.imported_without_embedding += 1;
        }
        embeddings.push(blob);
    }

    // ── Step 6: single all-or-nothing transaction per project ─────────────────
    let import_result =
        import_batch(&mem_store, &to_import, &embeddings).context("inserting notes into memory.db");

    match import_result {
        Ok(imported_ids) => {
            summary.imported = imported_ids.len();

            // ── Step 7: resolve supersede links ──────────────────────────────
            // Build a hash → local_id map from the just-imported notes plus
            // the pre-existing notes, so we can relink supersede chains.
            let mut hash_to_local: HashMap<String, i64> = existing_notes
                .iter()
                .map(|n| {
                    (
                        content_hash(
                            &n.kind,
                            &n.title,
                            &n.body,
                            &n.tags.join(","),
                            &n.linked_files.join(","),
                            n.created_at,
                        )
                        .to_hex()
                        .to_string(),
                        n.id,
                    )
                })
                .collect();
            for (note, local_id) in to_import.iter().zip(imported_ids.iter()) {
                hash_to_local.insert(note.hash().to_hex().to_string(), *local_id);
            }

            // For imported notes that were originally superseded, try to find
            // the successor in the local id map by looking for a candidate whose
            // server-side id matches the superseded_by.  Because server rowids
            // aren't portable we skip candidates whose successor we can't resolve.
            let server_id_to_hash: HashMap<usize, String> = candidates
                .iter()
                .enumerate()
                .map(|(i, n)| (i, n.hash().to_hex().to_string()))
                .collect();

            // Map server note index → its server-side original id is not directly
            // stored.  Re-read the server.db to get server IDs for supersede resolution.
            // We do this with a lightweight re-open to avoid holding the connection.
            let supersede_map = build_supersede_map(server_db_path, project_id, &candidates)?;

            let mut unresolved = 0usize;
            for (idx, (note, local_id)) in to_import.iter().zip(imported_ids.iter()).enumerate() {
                if note.status == "archived"
                    && let Some(server_successor_hash) = supersede_map
                        .get(&idx)
                        .and_then(|server_succ_idx| server_id_to_hash.get(server_succ_idx))
                {
                    if let Some(&succ_local_id) = hash_to_local.get(server_successor_hash) {
                        if let Err(e) = mem_store.set_superseded_by(*local_id, succ_local_id) {
                            tracing::warn!(
                                "reconcile: could not set superseded_by for #{local_id}: {e}"
                            );
                            unresolved += 1;
                        }
                    } else {
                        unresolved += 1;
                    }
                }
            }
            summary.skipped_archived_supersede_unresolved = unresolved;

            emit_summary(&summary, json, args.dry_run);
            Ok(())
        }
        Err(e) => {
            summary.errors.push(ReconcileError {
                stage: "import_transaction".to_string(),
                message: format!("{e:#}"),
            });
            emit_summary(&summary, json, args.dry_run);
            anyhow::bail!("{e:#}");
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Default path for the daemon's server.db: `~/.local/state/spelunk/server.db`.
fn default_server_db_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("state")
        .join("spelunk")
        .join("server.db")
}

/// Open server.db in immutable read-only mode.  The daemon owns this file;
/// we must never write to it.
fn open_server_db_readonly(path: &std::path::Path) -> Result<Connection> {
    // SQLITE_OPEN_READONLY | SQLITE_OPEN_URI
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening server.db read-only at {}", path.display()))?;
    // Use WAL read-mode so we don't block the daemon's writers.
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    Ok(conn)
}

fn read_server_notes(conn: &Connection, project_id: i64) -> Result<Vec<ServerNote>> {
    let mut stmt = conn.prepare(
        "SELECT kind, title, body, \
               COALESCE(tags, ''), COALESCE(linked_files, ''), \
               created_at, status, superseded_by \
         FROM notes \
         WHERE project_id = ?1 \
         ORDER BY created_at ASC",
    )?;
    let notes = stmt
        .query_map(rusqlite::params![project_id], |row| {
            Ok(ServerNote {
                kind: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                tags: row.get(3)?,
                linked_files: row.get(4)?,
                created_at: row.get(5)?,
                status: row.get(6)?,
                superseded_by: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(notes)
}

/// Insert the batch in a single transaction. Returns the list of new local ids.
fn import_batch(
    store: &MemoryStore,
    notes: &[&ServerNote],
    embeddings: &[Option<Vec<u8>>],
) -> Result<Vec<i64>> {
    store
        .execute_batch("BEGIN IMMEDIATE")
        .context("beginning import transaction")?;

    let mut ids = Vec::with_capacity(notes.len());
    let result: Result<()> = (|| {
        for (note, embedding) in notes.iter().zip(embeddings.iter()) {
            // Compute normalized tags/files as owned strings first, then slice.
            let norm_tags_owned = normalize_csv(&note.tags);
            let norm_files_owned = normalize_csv(&note.linked_files);
            let tag_parts: Vec<&str> = norm_tags_owned
                .split(',')
                .filter(|s| !s.is_empty())
                .collect();
            let file_parts: Vec<&str> = norm_files_owned
                .split(',')
                .filter(|s| !s.is_empty())
                .collect();

            // Determine import status: archived source rows stay archived.
            let status = if note.status == "archived" {
                "archived"
            } else {
                "active"
            };

            let id = store.add_note_with_created_at(
                &note.kind,
                &note.title,
                &note.body,
                &tag_parts,
                &file_parts,
                Some("reconcile:server.db"),
                status,
                note.created_at,
            )?;
            ids.push(id);

            if let Some(blob) = embedding {
                store.insert_embedding(id, blob)?;
            }
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            store
                .execute_batch("COMMIT")
                .context("committing import transaction")?;
            Ok(ids)
        }
        Err(e) => {
            let _ = store.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Build a map from candidate index → candidate index of the successor,
/// using the server.db to obtain the original server-side IDs.
///
/// `superseded_by` in server.db is a server-local rowid.  We build a map
/// server_id → candidate_index first, then follow the link.
fn build_supersede_map(
    server_db_path: &std::path::Path,
    project_id: i64,
    candidates: &[ServerNote],
) -> Result<HashMap<usize, usize>> {
    let conn = open_server_db_readonly(server_db_path)?;
    let mut stmt = conn.prepare(
        "SELECT id, superseded_by FROM notes WHERE project_id = ?1 ORDER BY created_at ASC",
    )?;
    let rows: Vec<(i64, Option<i64>)> = stmt
        .query_map(rusqlite::params![project_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    // Map server_id → candidate_index (index into `candidates` vec, same order).
    let server_id_to_idx: HashMap<i64, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, (server_id, _))| (*server_id, i))
        .collect();

    let mut result = HashMap::new();
    for (idx, (_, superseded_by)) in rows.iter().enumerate() {
        if idx < candidates.len()
            && let Some(succ_server_id) = superseded_by
            && let Some(&succ_idx) = server_id_to_idx.get(succ_server_id)
        {
            result.insert(idx, succ_idx);
        }
    }
    Ok(result)
}

/// Try to obtain an embedding blob for `text`.  Returns `None` without
/// failing if the server is unavailable.
async fn try_embed(client: &Option<ServerInferenceClient>, text: &str) -> Option<Vec<u8>> {
    use crate::embeddings::vec_to_blob;
    let client = client.as_ref()?;
    match client.embed_text(text).await {
        Ok(vec) => Some(vec_to_blob(&vec)),
        Err(e) => {
            tracing::debug!("reconcile: embedding failed (non-fatal): {e}");
            None
        }
    }
}

fn emit_summary(summary: &ReconcileSummary, json: bool, dry_run: bool) {
    if json {
        println!("{}", serde_json::to_string(summary).unwrap_or_default());
    } else {
        print_human_summary(summary, dry_run);
    }
}

fn print_human_summary(s: &ReconcileSummary, dry_run: bool) {
    if dry_run {
        eprintln!(
            "[spelunk] reconcile (dry-run): source={} project={} candidates={} already_present={} would_import={}",
            s.source_db, s.project_slug, s.candidates, s.already_present, s.would_import
        );
        return;
    }
    eprintln!(
        "[spelunk] reconcile: source={} project={} candidates={} already_present={} imported={} without_embedding={} supersede_unresolved={}",
        s.source_db,
        s.project_slug,
        s.candidates,
        s.already_present,
        s.imported,
        s.imported_without_embedding,
        s.skipped_archived_supersede_unresolved,
    );
    if !s.errors.is_empty() {
        for e in &s.errors {
            eprintln!("[spelunk] reconcile error ({}): {}", e.stage, e.message);
        }
    }
}

// ── Discovery nudge helpers (called by list/search/context) ──────────────────

/// Return the number of notes in server.db for `slug` that are absent from
/// `memory.db`.  Used to drive the one-time discovery nudge.
///
/// Returns `None` if server.db doesn't exist or is unreadable (silent).
pub(super) fn count_reconcilable(
    server_db_path: &std::path::Path,
    mem_path: &std::path::Path,
    slug: &str,
) -> Option<usize> {
    let conn = open_server_db_readonly(server_db_path).ok()?;
    let project_id: i64 = conn
        .query_row(
            "SELECT id FROM projects WHERE slug = ?1",
            rusqlite::params![slug],
            |r| r.get(0),
        )
        .optional()
        .ok()??;

    let candidates = read_server_notes(&conn, project_id).ok()?;
    if candidates.is_empty() {
        return None;
    }
    drop(conn);

    let mem_store = MemoryStore::open(mem_path).ok()?;
    let existing = mem_store.all_notes_for_dedup().ok()?;
    let existing_hashes: std::collections::HashSet<String> = existing
        .iter()
        .map(|n| {
            content_hash(
                &n.kind,
                &n.title,
                &n.body,
                &n.tags.join(","),
                &n.linked_files.join(","),
                n.created_at,
            )
            .to_hex()
            .to_string()
        })
        .collect();

    let count = candidates
        .iter()
        .filter(|c| !existing_hashes.contains(&c.hash().to_hex().to_string()))
        .count();

    if count > 0 { Some(count) } else { None }
}

/// Print a one-time discovery nudge to stderr if there are reconcilable notes.
///
/// The nudge is suppressed when:
/// - `server.db` doesn't exist or has no new notes.
/// - Any note in `memory.db` has `source_ref = 'reconcile:server.db'` (prior
///   reconcile has already run — we trust the user has seen the nudge).
/// - `SPELUNK_NO_RECONCILE_NUDGE=1` is set (CI / scripting escape hatch).
pub(crate) fn maybe_emit_nudge(mem_path: &std::path::Path, cfg: &Config) {
    if std::env::var_os("SPELUNK_NO_RECONCILE_NUDGE").is_some() {
        return;
    }

    // Suppress if any imported-from-server note already exists (run already done).
    if let Ok(store) = MemoryStore::open(mem_path)
        && store.has_source_ref("reconcile:server.db").unwrap_or(false)
    {
        return;
    }

    let server_db = default_server_db_path();
    let project_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let slug = cfg.resolve_project_id(&project_root);

    if let Some(n) = count_reconcilable(&server_db, mem_path, &slug) {
        eprintln!(
            "[spelunk] {n} note(s) recorded by a local server aren't in this project's memory yet. \
             Run 'spelunk memory reconcile' to import them."
        );
    }
}
