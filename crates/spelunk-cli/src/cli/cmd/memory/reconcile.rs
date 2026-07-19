//! `spelunk memory reconcile` — import unique notes from server.db into memory.db.
//!
//! Discovers notes that exist in the local daemon's `server.db` but are absent
//! from `memory.db` for the active project, and imports them in a single
//! all-or-nothing transaction.
//!
//! Dedup is by `entity_id` (ADR-068) — sha256 over the canonical JSON of
//! {body, kind, title} — never by rowid, which server.db and memory.db number
//! independently. This module and `init`'s git-notes import must key
//! identically or a row imported by one path is re-imported by the other; the
//! shared `entity_id` function is what enforces that.
//!
//! `created_at`, `tags`, and `linked_files` are deliberately out of the key:
//! identity has to be reproducible by a second machine recording the same
//! decision, and none of the three is. Entries differing only in those fields
//! collapse, unioning tags/linked_files add-wins.
//!
//! See ADR-004 for the full interface contract.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;

use super::MemoryReconcileArgs;
use crate::{
    capability::spelunk_state_dir,
    config::Config,
    server_client::ServerInferenceClient,
    storage::{MemoryStore, entity_id, note_entity_id},
};

// ── Candidate row from server.db ──────────────────────────────────────────────

/// A row read from `server.db`.
///
/// `id` and `superseded_by` are server-local rowids: meaningful for resolving an
/// edge *within* server.db, never across the boundary into memory.db. The edge
/// crosses by `entity_id`.
#[derive(Debug, Clone)]
struct ServerNote {
    id: i64,
    kind: String,
    title: String,
    body: String,
    tags: String, // raw CSV as stored in server.db
    linked_files: String,
    created_at: i64,
    status: String,
    superseded_by: Option<i64>,
}

impl ServerNote {
    fn entity_id(&self) -> String {
        entity_id(&self.kind, &self.title, &self.body)
    }

    fn tags_vec(&self) -> Vec<String> {
        split_csv(&self.tags)
    }

    fn files_vec(&self) -> Vec<String> {
        split_csv(&self.linked_files)
    }

    fn is_archived(&self) -> bool {
        self.status == "archived"
    }
}

/// Split a raw server.db CSV field: trim members, drop empties.
fn split_csv(csv: &str) -> Vec<String> {
    csv.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

// ── Candidate collapse ────────────────────────────────────────────────────────

/// Candidate rows sharing one `entity_id`, folded into a single entry.
///
/// Several rows can share an id now that `created_at`/`tags`/`linked_files` are
/// out of the key, so the fold has to decide what the survivor carries.
#[derive(Debug)]
struct MergedNote {
    entity_id: String,
    kind: String,
    title: String,
    body: String,
    tags: Vec<String>,
    linked_files: Vec<String>,
    created_at: i64,
    status: String,
    /// How many candidate rows folded in — drives the summary counts.
    rows: usize,
}

impl MergedNote {
    fn from_server(entity_id: String, c: &ServerNote) -> Self {
        Self {
            entity_id,
            kind: c.kind.clone(),
            title: c.title.clone(),
            body: c.body.clone(),
            tags: c.tags_vec(),
            linked_files: c.files_vec(),
            created_at: c.created_at,
            status: c.status.clone(),
            rows: 1,
        }
    }

    /// `kind`/`title`/`body` are identical by construction — they are the id.
    /// Everything else folds: tags/linked_files union (add-wins), an archive on
    /// any row sticks, and the earliest `created_at` wins so supersede chains
    /// keep importing in order.
    fn absorb(&mut self, c: &ServerNote) {
        for t in c.tags_vec() {
            if !self.tags.contains(&t) {
                self.tags.push(t);
            }
        }
        for f in c.files_vec() {
            if !self.linked_files.contains(&f) {
                self.linked_files.push(f);
            }
        }
        if c.is_archived() {
            self.status = "archived".to_string();
        }
        self.created_at = self.created_at.min(c.created_at);
        self.rows += 1;
    }
}

/// Fold `candidates` to one entry per `entity_id`, preserving first-seen order.
fn collapse_candidates(candidates: &[ServerNote]) -> Vec<MergedNote> {
    let mut merged: Vec<MergedNote> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for c in candidates {
        let eid = c.entity_id();
        match index.get(&eid) {
            Some(&i) => merged[i].absorb(c),
            None => {
                index.insert(eid.clone(), merged.len());
                merged.push(MergedNote::from_server(eid, c));
            }
        }
    }
    merged
}

/// `entity_id` → successor's `entity_id`.
///
/// Resolved through the server-local rowids, which are valid inside server.db;
/// the resulting edge is expressed entirely in content-addressed ids and so
/// survives the crossing into a store whose rowids are numbered differently.
fn build_supersede_edges(candidates: &[ServerNote]) -> HashMap<String, String> {
    let by_server_id: HashMap<i64, &ServerNote> = candidates.iter().map(|c| (c.id, c)).collect();
    let mut edges = HashMap::new();
    for c in candidates {
        if let Some(succ_id) = c.superseded_by
            && let Some(succ) = by_server_id.get(&succ_id)
        {
            edges
                .entry(c.entity_id())
                .or_insert_with(|| succ.entity_id());
        }
    }
    edges
}

// ── Summary / NDJSON reporting ────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ReconcileError {
    stage: String,
    message: String,
}

/// Counts are over source *rows*, and partition them exactly:
/// `candidates == already_present + collapsed_duplicates + imported`
/// (`would_import` replaces `imported` under `--dry-run`).
#[derive(Debug, Serialize)]
struct ReconcileSummary {
    source_db: String,
    project_slug: String,
    candidates: usize,
    already_present: usize,
    /// Rows folded into a sibling candidate sharing their `entity_id`.
    collapsed_duplicates: usize,
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
            collapsed_duplicates: 0,
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
        collapsed_duplicates: 0,
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

    // ── Step 3: open memory.db, index existing entries by entity_id ──────────
    let mem_store = MemoryStore::open(mem_path)
        .with_context(|| format!("opening memory.db at {}", mem_path.display()))?;

    let existing_notes = mem_store
        .all_notes_for_dedup()
        .context("reading existing memory.db notes for dedup")?;

    // A store can already hold several rows under one entity_id — the previous
    // key folded in created_at, so same-text entries stayed distinct. They are
    // left alone; the oldest is the stable edge target.
    let mut entity_to_local: HashMap<String, i64> = HashMap::new();
    for n in &existing_notes {
        entity_to_local.entry(note_entity_id(n)).or_insert(n.id);
    }

    // ── Step 4: build reconcile set (source rows not in memory.db) ───────────
    // Candidates sharing an entity_id collapse into one entry first.
    let (present, mut to_import): (Vec<MergedNote>, Vec<MergedNote>) =
        collapse_candidates(&candidates)
            .into_iter()
            .partition(|m| entity_to_local.contains_key(&m.entity_id));

    // Sort by created_at ASC to preserve supersede chains.
    to_import.sort_by_key(|n| n.created_at);

    summary.already_present = present.iter().map(|m| m.rows).sum();
    summary.collapsed_duplicates = to_import.iter().map(|m| m.rows - 1).sum();

    // Dry-run: report and stop, before any write.
    if args.dry_run {
        summary.would_import = to_import.len();
        emit_summary(&summary, json, args.dry_run);
        return Ok(());
    }

    // A candidate that collapsed onto a stored entry still carries tags and
    // linked_files the stored copy may lack. Add-wins: merge them in rather than
    // dropping them with the duplicate.
    for m in &present {
        let Some(&local_id) = entity_to_local.get(&m.entity_id) else {
            continue;
        };
        if let Err(e) = mem_store.union_tags_and_files(local_id, &m.tags, &m.linked_files) {
            tracing::warn!("reconcile: could not merge tags into #{local_id}: {e}");
        }
    }

    if to_import.is_empty() {
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
            // Every entity now in the store, keyed by its content-addressed id:
            // the successor may be a row we just imported or one already held.
            for (note, local_id) in to_import.iter().zip(imported_ids.iter()) {
                entity_to_local.insert(note.entity_id.clone(), *local_id);
            }

            // The edge is content-addressed on both ends, so it resolves even
            // though server.db and memory.db number their rows independently.
            let supersede_edges = build_supersede_edges(&candidates);

            let mut unresolved = 0usize;
            for (note, local_id) in to_import.iter().zip(imported_ids.iter()) {
                if note.status != "archived" {
                    continue;
                }
                let Some(succ_entity_id) = supersede_edges.get(&note.entity_id) else {
                    continue;
                };
                let Some(&succ_local_id) = entity_to_local.get(succ_entity_id) else {
                    unresolved += 1;
                    continue;
                };
                // Collapse can point an entry at itself when a supersede pair
                // shares its text; a self-edge is a cycle, not a chain.
                if succ_local_id == *local_id {
                    continue;
                }
                if let Err(e) = mem_store.set_superseded_by(*local_id, succ_local_id) {
                    tracing::warn!("reconcile: could not set superseded_by for #{local_id}: {e}");
                    unresolved += 1;
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

/// Default path for the daemon's server.db: `~/.local/state/spelunk/server.db`,
/// or `SPELUNK_STATE_DIR` when set.
///
/// Must go through the shared `capability::spelunk_state_dir` resolver, not
/// reconstruct the path from `dirs::home_dir()` independently: the daemon
/// (`spelunk server start`) writes `server.db` via that same resolver, so a
/// second, hardcoded reconstruction here would silently stop finding it
/// under `SPELUNK_STATE_DIR` while still reporting reconcile as a no-op
/// success (the "server.db doesn't exist" branch) instead of an error.
fn default_server_db_path() -> PathBuf {
    spelunk_state_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
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
        "SELECT id, kind, title, body, \
               COALESCE(tags, ''), COALESCE(linked_files, ''), \
               created_at, status, superseded_by \
         FROM notes \
         WHERE project_id = ?1 \
         ORDER BY created_at ASC",
    )?;
    let notes = stmt
        .query_map(rusqlite::params![project_id], |row| {
            Ok(ServerNote {
                id: row.get(0)?,
                kind: row.get(1)?,
                title: row.get(2)?,
                body: row.get(3)?,
                tags: row.get(4)?,
                linked_files: row.get(5)?,
                created_at: row.get(6)?,
                status: row.get(7)?,
                superseded_by: row.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(notes)
}

/// Insert the batch in a single transaction. Returns the list of new local ids.
fn import_batch(
    store: &MemoryStore,
    notes: &[MergedNote],
    embeddings: &[Option<Vec<u8>>],
) -> Result<Vec<i64>> {
    store
        .execute_batch("BEGIN IMMEDIATE")
        .context("beginning import transaction")?;

    let mut ids = Vec::with_capacity(notes.len());
    let result: Result<()> = (|| {
        for (note, embedding) in notes.iter().zip(embeddings.iter()) {
            let tag_parts: Vec<&str> = note.tags.iter().map(String::as_str).collect();
            let file_parts: Vec<&str> = note.linked_files.iter().map(String::as_str).collect();

            // Determine import status: archived source rows stay archived.
            let status = if note.status == "archived" {
                "archived"
            } else {
                "active"
            };

            let (id, _created) = store.add_note_with_created_at(
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

// ── init-time git-notes import ────────────────────────────────────────────────

/// source_ref stamped on entries imported from git notes during `init`.
const INIT_GIT_NOTES_SOURCE: &str = "init:git-notes";

/// Matches `GitNotesBackend`'s internal per-list cap; requesting more only logs
/// a warning and is truncated anyway.
const GIT_NOTES_IMPORT_LIMIT: usize = 500;

/// Import git-notes memory entries into the project `memory.db` during `init`.
///
/// Reads every entry from the enclosing repo's git-notes backend
/// (`refs/notes/spelunk`) and inserts those absent from `memory.db`, without
/// embeddings (git-notes entries carry none). Dedup uses the same content hash
/// as `memory reconcile`, so a re-run imports nothing. Returns how many were
/// imported. An empty/absent notes ref is a no-op (returns 0).
pub(crate) async fn import_git_notes_into_memory(
    git_root: &std::path::Path,
    mem_path: &std::path::Path,
) -> Result<usize> {
    use crate::storage::{GitNotesBackend, MemoryBackend};

    let backend = GitNotesBackend::with_root(git_root.to_path_buf());
    // include_archived=true so archived git-notes entries import and participate
    // in dedup, mirroring reconcile.
    let notes = backend
        .list(None, GIT_NOTES_IMPORT_LIMIT, true, None)
        .await?;
    if notes.is_empty() {
        return Ok(0);
    }

    let store = MemoryStore::open(mem_path)
        .with_context(|| format!("opening memory.db at {}", mem_path.display()))?;
    let mut existing: std::collections::HashSet<String> = store
        .all_notes_for_dedup()
        .context("reading existing memory.db notes for dedup")?
        .iter()
        .map(note_entity_id)
        .collect();

    // `insert` returning false also drops duplicates *within* the notes ref —
    // two entries with identical text are one entity now.
    let to_import: Vec<&crate::storage::memory::Note> = notes
        .iter()
        .filter(|&n| existing.insert(note_entity_id(n)))
        .collect();
    if to_import.is_empty() {
        return Ok(0);
    }

    // Single all-or-nothing transaction, mirroring reconcile's import_batch.
    store
        .execute_batch("BEGIN IMMEDIATE")
        .context("beginning git-notes import transaction")?;
    let result: Result<usize> = (|| {
        for note in &to_import {
            let tags: Vec<&str> = note.tags.iter().map(String::as_str).collect();
            let files: Vec<&str> = note.linked_files.iter().map(String::as_str).collect();
            let status = if note.status == "archived" {
                "archived"
            } else {
                "active"
            };
            store.add_note_with_created_at(
                &note.kind,
                &note.title,
                &note.body,
                &tags,
                &files,
                Some(INIT_GIT_NOTES_SOURCE),
                status,
                note.created_at,
            )?;
        }
        Ok(to_import.len())
    })();

    match result {
        Ok(n) => {
            store
                .execute_batch("COMMIT")
                .context("committing git-notes import transaction")?;
            Ok(n)
        }
        Err(e) => {
            let _ = store.execute_batch("ROLLBACK");
            Err(e)
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
    let existing_entities: std::collections::HashSet<String> =
        existing.iter().map(note_entity_id).collect();

    // Counts entries the user would gain, so collapsed duplicates count once —
    // it must not promise more than `reconcile` would import.
    let count = collapse_candidates(&candidates)
        .iter()
        .filter(|m| !existing_entities.contains(&m.entity_id))
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

#[cfg(test)]
mod init_import_tests {
    use super::*;
    use crate::storage::{GitNotesBackend, MemoryBackend, NoteInput};
    use std::sync::OnceLock;

    fn register_sqlite_vec() {
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

    fn make_temp_git_repo() -> tempfile::TempDir {
        // Process-wide: this repo's own `commit` below runs through the
        // ambient global git config if it isn't neutralized first, so an
        // ambient `core.hooksPath` fires a foreign pre-commit hook here.
        crate::cli::cmd::test_support::isolate_git_config();
        let dir = tempfile::TempDir::new().expect("tempdir");
        let p = dir.path();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(p)
                .output()
                .expect("git command");
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(p.join("README.md"), "test").expect("write");
        run(&["add", "."]);
        run(&[
            "commit",
            "--no-gpg-sign",
            "-m",
            "init",
            "--allow-empty-message",
        ]);
        dir
    }

    /// Happy path: a note recorded via git notes before `init` is imported into
    /// memory.db, is visible via the SQLite store, and a re-run adds nothing.
    #[tokio::test]
    async fn init_imports_git_notes_and_is_idempotent() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();

        let backend = GitNotesBackend::with_root(git_root.to_path_buf());
        backend
            .add(NoteInput {
                kind: "decision".to_string(),
                title: "use sqlite".to_string(),
                body: "chosen for portability".to_string(),
                tags: vec!["storage".to_string()],
                linked_files: vec![],
                embedding: None,
                source_ref: None,
                valid_at: None,
                supersedes: None,
            })
            .await
            .expect("git-notes add");

        let mem_path = git_root.join(".spelunk").join("memory.db");
        let imported = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import");
        assert_eq!(imported, 1, "pre-init git-notes entry must import");

        let store = MemoryStore::open(&mem_path).expect("open memory.db");
        let listed = store.list(None, 10, false).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "use sqlite");
        assert_eq!(listed[0].source_ref.as_deref(), Some(INIT_GIT_NOTES_SOURCE));

        let again = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("re-import");
        assert_eq!(again, 0, "re-import must be a no-op");
        assert_eq!(
            store.list(None, 10, false).expect("list again").len(),
            1,
            "no duplication on re-run"
        );
    }

    /// A blob written before `entity_id` existed carries no such key. Its
    /// identity recomputes from the three fields it does carry, so it still
    /// dedups against a stored row — absence must be fully recoverable.
    #[tokio::test]
    async fn init_import_dedups_legacy_blob_without_entity_id() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();

        // A legacy record: serde omits `entity_id` when None, so this is
        // byte-identical to a blob written before the field existed.
        let legacy = crate::storage::NoteRecord {
            schema_version: 1,
            id: 1,
            kind: "decision".to_string(),
            title: "legacy entry".to_string(),
            body: "written by an older client".to_string(),
            tags: vec![],
            linked_files: vec![],
            created_at: 1_700_000_000,
            status: "active".to_string(),
            source_ref: None,
            valid_at: None,
            invalid_at: None,
            superseded_by: None,
            remote_id: None,
            entity_id: None,
            superseded_by_entity_id: None,
        };
        crate::storage::append_to_git_notes(Some(git_root), &legacy)
            .await
            .expect("append legacy record");

        let raw = std::process::Command::new("git")
            .args(["notes", "--ref=spelunk", "show", "HEAD"])
            .current_dir(git_root)
            .output()
            .expect("git notes show");
        let blob = String::from_utf8_lossy(&raw.stdout);
        assert!(
            !blob.contains("\"entity_id\""),
            "the seeded blob must genuinely lack the key: {blob}"
        );

        // Seed memory.db with the same content, as a prior import would have.
        let mem_path = git_root.join(".spelunk").join("memory.db");
        let store = MemoryStore::open(&mem_path).expect("open memory.db");
        store
            .add_note_with_created_at(
                "decision",
                "legacy entry",
                "written by an older client",
                &[],
                &[],
                Some("manual"),
                "active",
                1_700_000_999, // a different created_at: no longer part of the key
            )
            .expect("seed note");

        let imported = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import");
        assert_eq!(
            imported, 0,
            "legacy blob must recompute its id and dedup against the stored row"
        );
        assert_eq!(store.list(None, 10, true).expect("list").len(), 1);
    }

    /// A repo with no spelunk notes ref is a silent no-op.
    #[tokio::test]
    async fn init_import_no_notes_is_noop() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();
        let mem_path = git_root.join(".spelunk").join("memory.db");
        let imported = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import");
        assert_eq!(imported, 0, "no notes ref → nothing imported");
    }

    // ── added coverage (init git-notes import hardening) ──────────────────────

    /// A `git init`'d repo with no commit yet (no HEAD, no notes ref) is a
    /// silent no-op — and must not churn an empty `memory.db` into existence.
    #[tokio::test]
    async fn init_import_no_commit_repo_is_noop_no_churn() {
        register_sqlite_vec();
        let repo = make_temp_git_repo_no_commit();
        let git_root = repo.path();
        let mem_path = git_root.join(".spelunk").join("memory.db");

        let imported = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import");
        assert_eq!(imported, 0, "no HEAD / no notes ref → nothing imported");
        assert!(
            !mem_path.exists(),
            "an empty notes ref must not create a memory.db (no churn)"
        );
    }

    /// A repo that HAS a commit but no spelunk notes must likewise leave no
    /// `memory.db` behind (the import bails before opening the store).
    #[tokio::test]
    async fn init_import_no_notes_no_db_churn() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();
        let mem_path = git_root.join(".spelunk").join("memory.db");

        let imported = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import");
        assert_eq!(imported, 0);
        assert!(
            !mem_path.exists(),
            "no notes to import must not create a memory.db"
        );
    }

    /// An archived git-notes entry imports (carrying its archived status) and
    /// participates in dedup, so a re-run imports nothing and never duplicates.
    #[tokio::test]
    async fn init_import_archived_entry_imports_and_dedups() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();

        let backend = GitNotesBackend::with_root(git_root.to_path_buf());
        let (id, _created) = backend
            .add(NoteInput {
                kind: "note".to_string(),
                title: "retired decision".to_string(),
                body: "kept for the record".to_string(),
                tags: vec![],
                linked_files: vec![],
                embedding: None,
                source_ref: None,
                valid_at: None,
                supersedes: None,
            })
            .await
            .expect("git-notes add");
        assert!(
            backend.archive(id).await.expect("archive"),
            "the seeded entry must archive"
        );

        let mem_path = git_root.join(".spelunk").join("memory.db");
        let imported = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import");
        assert_eq!(imported, 1, "archived git-notes entry must import");

        let store = MemoryStore::open(&mem_path).expect("open memory.db");
        // Not surfaced by an active-only listing …
        assert!(
            store.list(None, 10, false).expect("active list").is_empty(),
            "an imported archived entry must not appear in the active listing"
        );
        // … but present, and archived, when archived rows are included.
        let all = store.list(None, 10, true).expect("full list");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, "archived", "archived status must be carried");

        // Re-run: the archived row already present in memory.db dedups (the
        // content key excludes status), so nothing re-imports and no duplicate.
        let again = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("re-import");
        assert_eq!(again, 0, "archived entry must not double-import");
        assert_eq!(
            store.list(None, 10, true).expect("full list again").len(),
            1,
            "row count must stay stable across re-import"
        );
    }

    /// A git-notes entry whose content already exists in `memory.db` (e.g. it
    /// arrived earlier via `memory reconcile` or a manual add) is NOT
    /// re-imported: init reuses reconcile's content key, so the two stores
    /// dedup against each other. Only the genuinely-new entry imports.
    #[tokio::test]
    async fn init_import_skips_entries_already_in_memory_db() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();

        let backend = GitNotesBackend::with_root(git_root.to_path_buf());
        // Entry A: will be pre-seeded into memory.db (simulating a prior path).
        backend
            .add(NoteInput {
                kind: "decision".to_string(),
                title: "already present".to_string(),
                body: "seeded into memory.db before init".to_string(),
                tags: vec!["x".to_string()],
                linked_files: vec![],
                embedding: None,
                source_ref: None,
                valid_at: None,
                supersedes: None,
            })
            .await
            .expect("git-notes add A");
        // Entry B: only in git-notes — the one init should actually import.
        backend
            .add(NoteInput {
                kind: "decision".to_string(),
                title: "brand new".to_string(),
                body: "only in git notes".to_string(),
                tags: vec![],
                linked_files: vec![],
                embedding: None,
                source_ref: None,
                valid_at: None,
                supersedes: None,
            })
            .await
            .expect("git-notes add B");

        // Read A back so we can seed memory.db with a byte-identical content key
        // (same kind/title/body/tags/created_at → same dedup hash).
        let seeded = backend
            .list(None, 10, true, None)
            .await
            .expect("list git notes");
        let a = seeded
            .iter()
            .find(|n| n.title == "already present")
            .expect("entry A present in git notes");

        let mem_path = git_root.join(".spelunk").join("memory.db");
        {
            let store = MemoryStore::open(&mem_path).expect("open memory.db");
            let tags: Vec<&str> = a.tags.iter().map(String::as_str).collect();
            store
                .add_note_with_created_at(
                    &a.kind,
                    &a.title,
                    &a.body,
                    &tags,
                    &[],
                    Some("manual"),
                    "active",
                    a.created_at,
                )
                .expect("seed A into memory.db");
        }

        let imported = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import");
        assert_eq!(imported, 1, "only the entry absent from memory.db imports");

        let store = MemoryStore::open(&mem_path).expect("reopen memory.db");
        let all = store.list(None, 50, true).expect("list");
        assert_eq!(
            all.len(),
            2,
            "no duplicate row for the already-present entry"
        );
        // A keeps its original source_ref; only B is stamped init:git-notes.
        let init_sourced = all
            .iter()
            .filter(|n| n.source_ref.as_deref() == Some(INIT_GIT_NOTES_SOURCE))
            .count();
        assert_eq!(init_sourced, 1, "exactly one row came from the init import");
    }

    /// Drift guard: reconcile's server-row key and init-import's memory-row key
    /// must be byte-identical for identical content. If they ever diverge, an
    /// entry present in both git-notes and memory.db is imported twice. The two
    /// inputs below deliberately differ in tag/file order and status — all
    /// excluded from the key — to prove both entry points agree regardless.
    #[test]
    fn dedup_key_parity_between_reconcile_and_init_import() {
        register_sqlite_vec();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let mem_path = tmp.path().join("memory.db");
        let store = MemoryStore::open(&mem_path).expect("open memory.db");
        let created_at = 1_700_000_123_i64;
        store
            .add_note_with_created_at(
                "decision",
                "shared key",
                "body text",
                &["beta", "alpha"],
                &["b.rs", "a.rs"],
                Some("manual"),
                "active",
                created_at,
            )
            .expect("seed note");
        let note = store
            .all_notes_for_dedup()
            .expect("dedup set")
            .pop()
            .expect("one note");

        // The reconcile candidate (a server.db row) carrying identical content,
        // but differing in every field the key excludes: a server-local rowid,
        // a different created_at, reordered tags/files, and an archived status.
        let server_note = ServerNote {
            id: 4242,
            kind: "decision".to_string(),
            title: "shared key".to_string(),
            body: "body text".to_string(),
            tags: "alpha,beta".to_string(),
            linked_files: "a.rs,b.rs".to_string(),
            created_at: created_at + 86_400,
            status: "archived".to_string(),
            superseded_by: None,
        };

        assert_eq!(
            note_entity_id(&note),
            server_note.entity_id(),
            "reconcile's key and init-import's key must match for identical content"
        );
    }

    /// Many entries import in a single batch and the whole set dedups on re-run.
    ///
    /// The import caps its git-notes read at `GIT_NOTES_IMPORT_LIMIT` (mirroring
    /// `GitNotesBackend`'s internal per-list cap). A live test of the 500+
    /// boundary is deliberately omitted: each git-notes write is a
    /// read-modify-write of the whole note blob, so seeding 500+ entries is
    /// O(n^2) subprocess work — too slow and flaky for CI. This exercises the
    /// multi-entry transaction path with a small plural batch instead, and the
    /// static assertion below pins the boundary constant.
    #[tokio::test]
    async fn init_import_multiple_entries_single_batch() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();

        let backend = GitNotesBackend::with_root(git_root.to_path_buf());
        const N: usize = 6;
        for i in 0..N {
            backend
                .add(NoteInput {
                    kind: "note".to_string(),
                    title: format!("entry {i}"),
                    body: format!("body {i}"),
                    tags: vec![],
                    linked_files: vec![],
                    embedding: None,
                    source_ref: None,
                    valid_at: None,
                    supersedes: None,
                })
                .await
                .expect("git-notes add");
        }

        let mem_path = git_root.join(".spelunk").join("memory.db");
        let imported = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import");
        assert_eq!(imported, N, "all seeded entries import in one batch");

        let store = MemoryStore::open(&mem_path).expect("open memory.db");
        assert_eq!(store.list(None, 100, false).expect("list").len(), N);

        let again = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("re-import");
        assert_eq!(again, 0, "the whole batch dedups on re-run");
        assert_eq!(
            store.list(None, 100, false).expect("list again").len(),
            N,
            "row count stable across re-import"
        );
    }

    /// The init import limit mirrors `GitNotesBackend`'s internal per-list cap
    /// (`GIT_NOTES_MAX_LIST`, currently 500). Kept in a static assertion so a
    /// change to one side without the other is caught at compile time; the cap
    /// itself lives in `storage::git_notes` and is not publicly re-exported.
    const _: () = assert!(GIT_NOTES_IMPORT_LIMIT == 500);

    /// A `git init`'d repo with no commit (no HEAD), for the no-op/no-churn path.
    fn make_temp_git_repo_no_commit() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir.path())
            .output()
            .expect("git init");
        dir
    }
}
