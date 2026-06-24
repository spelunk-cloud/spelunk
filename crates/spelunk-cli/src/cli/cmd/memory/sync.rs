//! `spelunk sync` and `spelunk memory pull` — two-way local↔cloud memory sync
//! (ADR-037 D2/D3).
//!
//! - `pull`: delta-pull from `GET /memory/since?since_id=<cursor>` and apply
//!   locally. The cursor is the max cloud `remote_id` already synced
//!   (`MAX(remote_id)` over local notes), a UUIDv7 — not a wall-clock watermark
//!   (decision #183), so it is immune to local↔remote clock drift.
//! - `sync`: push local rows `WHERE remote_id IS NULL` via `POST /memory/batch`
//!   (batched, not N single POSTs), then pull everything after the cursor and
//!   apply both.
//!
//! Properties (ADR-037 + ADR-005):
//! - **Idempotent.** Identity is the stable UUID; pushes carry it as the cloud
//!   `external_id` and pulls record the cloud UUID as the local `remote_id` and
//!   dedupe on it, so re-running never duplicates. Same-millisecond boundary
//!   entries are harmless: the cursor comparison is strict (`>`) and pull
//!   dedupes by `remote_id`, so a re-applied boundary row is a no-op.
//! - **Keep-both / Add-Wins.** Pulled entries are added, never overwriting local
//!   ones; semantic-dup detection is the server's job (it flags `contradicts`).
//! - **Lifecycle propagation.** `supersedes` and archive/tombstone state travel
//!   in both directions (previously hard-coded `None`/dropped).
//! - **Text-only.** Pushes never ship a vector; the server backfills embeddings
//!   (ADR-010/ADR-020).

use anyhow::{Context, Result};

use super::{MemoryPullArgs, MemorySyncArgs};
use crate::{
    capability,
    config::Config,
    storage::{BatchPushItem, CloudSyncClient, MemoryStore},
};

/// Resolve the cloud sync target (base URL, server-side project id, key).
///
/// Sync always speaks to an explicit `server_url` — it is the cloud-convergence
/// path, not the inference loopback. Errors with actionable guidance when the
/// project is offline or missing a `project_id`.
fn sync_target(cfg: &Config) -> Result<(String, String, Option<String>)> {
    let base_url = cfg.server_url.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "sync requires a server. Set `server_url` in your spelunk config \
             (e.g. ~/.config/spelunk/config.toml or .spelunk/config.toml)."
        )
    })?;
    let project_id = cfg.project_id.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "`project_id` is not configured. Set it in `.spelunk/config.toml` \
             or via `SPELUNK_PROJECT_ID` so sync can address the project."
        )
    })?;
    Ok((base_url, project_id, cfg.server_key.clone()))
}

/// `spelunk memory pull` — one-way delta pull + apply.
pub async fn memory_pull(
    _args: MemoryPullArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
) -> Result<()> {
    let tier = capability::get_tier(cfg).await;
    capability::require_tier1("memory pull", tier, cfg.server_url.as_deref())?;
    let (base_url, project_id, key) = sync_target(cfg)?;

    let local = MemoryStore::open(mem_path)
        .with_context(|| format!("opening local memory at {}", mem_path.display()))?;
    let client = CloudSyncClient::new(&base_url, &project_id, key.as_deref())?;

    let pulled = pull_and_apply(&local, &client).await?;
    println!("Pull complete. Applied {pulled} new remote entries.");
    Ok(())
}

/// `spelunk sync` (and `spelunk memory push`'s successor) — two-way sync.
pub async fn memory_sync(
    args: MemorySyncArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
) -> Result<()> {
    let tier = capability::get_tier(cfg).await;
    capability::require_tier1("sync", tier, cfg.server_url.as_deref())?;
    let (base_url, project_id, key) = sync_target(cfg)?;

    let src_path = args.source.as_deref().unwrap_or(mem_path);
    let local = MemoryStore::open(src_path)
        .with_context(|| format!("opening local memory at {}", src_path.display()))?;
    let client = CloudSyncClient::new(&base_url, &project_id, key.as_deref())?;

    // ── Push local → cloud (batched, text-only, idempotent on UUID) ─────────
    let pushed = push_local(&local, &client, args.include_archived).await?;

    // ── Pull cloud → local (delta after the UUID cursor, keep-both) ─────────
    let pulled = pull_and_apply(&local, &client).await?;

    println!(
        "Sync complete. Pushed {} entries (created {}, skipped {}), applied {} new remote entries.",
        pushed.attempted, pushed.created, pushed.skipped, pulled
    );
    Ok(())
}

/// Outcome of a push pass (shared by `sync` and the one-way `memory push`).
pub(super) struct PushSummary {
    pub attempted: usize,
    pub created: u32,
    pub skipped: u32,
}

/// One-way push entry point reused by `spelunk memory push`.
pub(super) async fn push_local_oneway(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
) -> Result<PushSummary> {
    push_local(local, client, include_archived).await
}

/// Push local entries to the cloud as text-only batches, then propagate
/// tombstones for any archived rows that exist cloud-side.
async fn push_local(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
) -> Result<PushSummary> {
    let rows = local.rows_for_sync(include_archived)?;
    let attempted = rows.len();
    if rows.is_empty() {
        return Ok(PushSummary {
            attempted: 0,
            created: 0,
            skipped: 0,
        });
    }

    // Split into live entries (batch-created/upserted by external_id) and
    // archived entries already known to the cloud (tombstoned via DELETE).
    let mut created = 0u32;
    let mut skipped = 0u32;

    // Push set (decision #183): live entries not yet on the cloud — i.e.
    // `WHERE remote_id IS NULL`. Already-synced rows carry a `remote_id` and are
    // skipped here (the cloud already has them; re-pushing would only earn a 207
    // `skipped`). Archived rows are handled by the tombstone pass below.
    let live: Vec<&_> = rows
        .iter()
        .filter(|r| !r.archived && r.remote_id.is_none())
        .collect();
    // Map external_id (local uuid) → local_id so we can record the cloud-minted
    // id returned in the 207 result back onto the local row.
    for chunk in live.chunks(200) {
        let items: Vec<BatchPushItem> = chunk
            .iter()
            .map(|r| BatchPushItem {
                kind: r.kind.clone(),
                title: r.title.clone(),
                body: if r.body.is_empty() {
                    None
                } else {
                    Some(r.body.clone())
                },
                external_id: r.uuid.clone(),
                source_commit: r.source_ref.clone(),
            })
            .collect();
        let res = client.push_batch(items).await?;
        created += res.created;
        skipped += res.skipped;

        // Record cloud ids for created entries so a later pull dedupes them and
        // a later archive can tombstone them by id.
        for item in &res.results {
            if let (Some(ext), Some(cloud_id)) = (item.external_id.as_deref(), item.id.as_deref())
                && let Some(row) = chunk.iter().find(|r| r.uuid == ext)
            {
                local.set_remote_id(row.local_id, cloud_id)?;
            }
            if item.status == "failed" {
                eprintln!(
                    "  [push-fail] {}",
                    item.external_id.as_deref().unwrap_or("<unknown>")
                );
            }
        }
    }

    // Tombstone archived entries that the cloud already knows about. An archived
    // row with no `remote_id` was never pushed live, so there is nothing to
    // delete cloud-side; we skip it.
    if include_archived {
        for r in rows.iter().filter(|r| r.archived) {
            if let Some(remote_id) = r.remote_id.as_deref() {
                client.delete_remote(remote_id).await?;
            }
        }
    }

    Ok(PushSummary {
        attempted,
        created,
        skipped,
    })
}

/// Pull remote entries after the UUID cursor and apply them idempotently.
/// Returns the number of newly-inserted local rows.
///
/// The cursor is derived from the store itself — `MAX(remote_id)` over local
/// notes (decision #183) — so there is no persisted watermark to advance: the
/// next run re-derives the cursor from the rows just applied. This is what makes
/// the pull immune to clock drift and trivially resumable.
async fn pull_and_apply(local: &MemoryStore, client: &CloudSyncClient) -> Result<usize> {
    let cursor = local.max_remote_id()?;
    let entries = client.pull_since(cursor.as_deref()).await?;

    let mut applied = 0usize;
    for e in &entries {
        let created_secs = parse_iso_to_secs(&e.created_at);
        let inserted = local.apply_remote_note(
            &e.id,
            &e.kind,
            &e.title,
            e.body.as_deref().unwrap_or(""),
            e.source_commit.as_deref(),
            created_secs,
            e.is_archived(),
        )?;
        if inserted {
            applied += 1;
        }
    }
    Ok(applied)
}

/// Parse an ISO 8601 / RFC 3339 timestamp to Unix epoch seconds.
///
/// Falls back to "now" if the server sends a value we cannot parse, so a single
/// odd row never aborts the whole sync.
fn parse_iso_to_secs(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|_| crate::storage::now_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_iso_to_secs_handles_utc_z() {
        // 2021-01-01T00:00:00Z = 1609459200
        assert_eq!(parse_iso_to_secs("2021-01-01T00:00:00Z"), 1_609_459_200);
    }

    #[test]
    fn parse_iso_to_secs_handles_offset() {
        // 2021-01-01T01:00:00+01:00 == 2021-01-01T00:00:00Z
        assert_eq!(
            parse_iso_to_secs("2021-01-01T01:00:00+01:00"),
            1_609_459_200
        );
    }

    #[test]
    fn parse_iso_to_secs_falls_back_on_garbage() {
        // Must not panic; returns some positive epoch (now).
        assert!(parse_iso_to_secs("not-a-timestamp") > 0);
    }
}
