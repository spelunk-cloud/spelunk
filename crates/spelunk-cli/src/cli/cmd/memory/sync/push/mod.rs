//! Push local memory entries to the cloud (`spelunk sync` and the one-way
//! `spelunk memory push`).

use std::collections::HashSet;

use anyhow::Result;

use crate::storage::{BatchPushItem, CloudSyncClient, MemoryStore, SyncEdgePush};

mod local_embed;

pub(in crate::cli::cmd::memory) use local_embed::{
    LocalEmbedPolicy, local_embed_summary, unembedded_warning,
};
use local_embed::{repair_local_embeddings, usable_vector};

/// How many entries go in each `POST /memory/batch` request. Kept small so even
/// the worst case (text-only, the server re-embedding a full chunk on a cold
/// embedder) finishes with wide margin under the request timeout, and a
/// with-vectors chunk (N x 896 fp32) is a sub-megabyte JSON body rather than
/// multi-MB; still large enough that a few hundred entries is only a handful of
/// requests. A finer chunk also bounds the loss on a mid-push failure to at most
/// this many entries before the resumable re-run continues. Fixed, not
/// configurable: no evidence a user needs to tune it, and a constant is trivial
/// to adjust later.
const PUSH_BATCH_CHUNK_SIZE: usize = 50;

/// Outcome of a push pass (shared by `sync` and the one-way `memory push`).
#[derive(Debug)]
pub(in crate::cli::cmd::memory) struct PushSummary {
    /// Rows actually sent to `push_batch` (the `live` set) — not the raw
    /// pre-filter row count, which would over-report when rows are already
    /// synced (`remote_id` already set) and no request is made at all.
    pub attempted: usize,
    /// Tallied from `results[].status`, not the server's own aggregate
    /// `created`/`skipped` ints — the two are independent wire fields
    /// (`BatchPushResult`) and can diverge (a server has been observed
    /// reporting aggregate `created: 0` for a batch whose per-item results
    /// showed entries durably persisted). `results[]` is the reconciled
    /// signal.
    pub created: u32,
    pub skipped: u32,
    /// Items whose status did not affirmatively mean "durably persisted"
    /// — anything other than `created`/`skipped` (`failed`, or an
    /// unrecognized status riding along with a result). Kept separate so a
    /// partial-failure batch still reports its real successes instead of
    /// reading as "nothing happened".
    pub failed: u32,
    /// Non-archived rows already carrying a `remote_id` — i.e. previously
    /// synced and excluded from `attempted`. Lets callers report an honest
    /// "nothing to push" message instead of implying a push happened.
    pub already_synced: usize,
    /// `Some(reason)` when a chunk failed mid-push and the loop stopped before
    /// the remaining chunks (see [`push_local`]); `None` on a clean run. Lets the
    /// command layer report honest partial progress plus a resume hint and exit
    /// non-zero, instead of discarding the chunks that already landed.
    pub interrupted: Option<String>,
    /// Push-set rows whose missing local vector this push minted and committed
    /// to `memory.db`. Reported separately from `created`/`skipped`/`failed`,
    /// which are all about what the *destination* did.
    pub embedded_locally: usize,
    /// Push-set rows that still have no usable local vector after the repair
    /// pass: no local embedder was reachable, or that row's embed call failed.
    /// Drives the one counted warning the command layer emits. Always 0 when
    /// the repair does not apply ([`LocalEmbedPolicy::Skip`]).
    pub without_local_vector: usize,
    /// `relates_to` edges the cloud confirmed it stored during this push (their
    /// second endpoint reached the cloud this round). Best-effort and secondary
    /// to entries: a failure to push them warns but never fails the push. See
    /// [`push_relates_to_edges`].
    pub edges_pushed: usize,
}

/// One-way push entry point reused by `spelunk memory push`.
///
/// `accepts_pushed_vectors` mirrors the destination server's `/v1/health`
/// capability: when true, each entry that has a local embedding
/// carries its fp32/896 vector so the server stores it as-is; when false the
/// push is text-only and the server re-embeds.
pub(in crate::cli::cmd::memory) async fn push_local_oneway(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
    accepts_pushed_vectors: bool,
    local_embed: &LocalEmbedPolicy<'_>,
) -> Result<PushSummary> {
    push_local(
        local,
        client,
        include_archived,
        accepts_pushed_vectors,
        local_embed,
    )
    .await
}

/// Push local entries to the cloud in batches, then propagate tombstones for any
/// archived rows that exist cloud-side.
///
/// Before the batch is built, `local_embed` decides whether push-set rows that
/// lack a local vector are embedded through the loopback embedder and committed
/// to `memory.db`: without that repair a pushed row stays invisible to semantic
/// `memory search` locally, with nothing telling the user.
///
/// Each entry is text-only unless
/// `accepts_pushed_vectors` is set and the row has a local fp32/896 embedding,
/// in which case that vector is attached (the server stores it without
/// re-embedding).
///
/// On a chunk-level failure the push stops at the first failed chunk and returns
/// a summary marked `interrupted` (rather than `?`-propagating and discarding
/// the chunks that already landed); the already-stamped chunks make the next run
/// resume from the remainder. See [`push_local_reporting`].
pub(super) async fn push_local(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
    accepts_pushed_vectors: bool,
    local_embed: &LocalEmbedPolicy<'_>,
) -> Result<PushSummary> {
    push_local_reporting(
        local,
        client,
        include_archived,
        accepts_pushed_vectors,
        local_embed,
        |done, total| {
            // Transient cumulative status on stderr; the final one-line summary
            // stays on stdout in the command layer.
            eprintln!("Pushed {done}/{total}…");
        },
    )
    .await
}

/// [`push_local`] with the per-chunk progress emission injected as `on_progress`
/// (called with cumulative `done`/`total` after each chunk that lands), so tests
/// can observe the progress sequence without capturing stderr. `push_local`
/// passes the stderr-writing closure.
async fn push_local_reporting(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
    accepts_pushed_vectors: bool,
    local_embed: &LocalEmbedPolicy<'_>,
    mut on_progress: impl FnMut(usize, usize),
) -> Result<PushSummary> {
    let rows = local.rows_for_sync(include_archived)?;
    if rows.is_empty() {
        return Ok(PushSummary {
            attempted: 0,
            created: 0,
            skipped: 0,
            failed: 0,
            already_synced: 0,
            interrupted: None,
            embedded_locally: 0,
            without_local_vector: 0,
            edges_pushed: 0,
        });
    }

    // Split into live entries (batch-created/upserted by external_id) and
    // archived entries already known to the cloud (tombstoned via DELETE).
    let mut created = 0u32;
    let mut skipped = 0u32;
    let mut failed = 0u32;
    // Local row ids whose `remote_id` this push stamped: the entries that
    // reached the cloud this round. A `relates_to` edge becomes pushable in the
    // round its second endpoint lands, so only edges touching this set are new
    // and worth posting (see `push_relates_to_edges`).
    let mut just_synced: HashSet<i64> = HashSet::new();
    // Set once a chunk fails: the loop stops and the tombstone pass is skipped.
    let mut interrupted: Option<String> = None;

    // Push set (decision #183): live entries not yet on the cloud — i.e.
    // `WHERE remote_id IS NULL`. Already-synced rows carry a `remote_id` and are
    // skipped here (the cloud already has them; re-pushing would only earn a 207
    // `skipped`). Archived rows are handled by the tombstone pass below.
    let live: Vec<&_> = rows
        .iter()
        .filter(|r| !r.archived && r.remote_id.is_none())
        .collect();
    let already_synced = rows
        .iter()
        .filter(|r| !r.archived && r.remote_id.is_some())
        .count();
    let attempted = live.len();
    // Repair the local store BEFORE the batch is built, so a freshly-minted
    // vector is available to `maybe_attach_vector` below and the pushed rows
    // are searchable locally afterwards.
    let repair = repair_local_embeddings(local, &live, local_embed).await?;
    // Progress is only worth emitting when the push actually spans multiple
    // chunks; a single-chunk push stays quiet (no noise on small pushes).
    let multi_chunk = attempted.div_ceil(PUSH_BATCH_CHUNK_SIZE) > 1;
    // Map external_id (local uuid) → local_id so we can record the cloud-minted
    // id returned in the 207 result back onto the local row.
    for chunk in live.chunks(PUSH_BATCH_CHUNK_SIZE) {
        let mut items: Vec<BatchPushItem> = Vec::with_capacity(chunk.len());
        for r in chunk {
            // Only read the local embedding when the server can accept it. The
            // stored blob is raw little-endian fp32 (`vec_to_blob`); decode it
            // and only attach a correctly-dimensioned (896) vector — a
            // wrong-length or missing embedding falls back to text-only rather
            // than poisoning the whole batch with a 4xx.
            let vector = if accepts_pushed_vectors {
                usable_vector(local.get_embedding(r.local_id)?)
            } else {
                None
            };
            items.push(
                BatchPushItem {
                    kind: r.kind.clone(),
                    title: r.title.clone(),
                    body: if r.body.is_empty() {
                        None
                    } else {
                        Some(r.body.clone())
                    },
                    external_id: r.uuid.clone(),
                    source_commit: r.source_ref.clone(),
                    vector: None,
                    vector_model: None,
                    vector_precision: None,
                }
                .maybe_attach_vector(accepts_pushed_vectors, vector),
            );
        }

        // A chunk failure (timeout / transport / non-2xx) usually means the
        // server is overloaded; pushing the remaining chunks would only make that
        // worse, and the resumable design (already-stamped chunks are filtered
        // out of `live` on the next run) means a re-run resumes from exactly
        // here. So stop at the first failed chunk and return the progress that
        // already landed instead of discarding it via `?`.
        let res = match client.push_batch(items).await {
            Ok(res) => res,
            Err(e) => {
                interrupted = Some(e.to_string());
                break;
            }
        };

        // `created`/`skipped`/`failed` (aggregate ints) and `results[]`
        // (per-item) are independent fields on `BatchPushResult` — nothing on
        // the wire guarantees they agree, and a server can send an aggregate
        // `created: 0` for a batch whose `results[]` shows the entries
        // durably persisted. The aggregate ints are NOT trusted here: tally
        // from `results[].status`, the reconciled signal, and only fall back
        // to the aggregate when the server sent no per-item detail at all to
        // reconcile against.
        if res.results.is_empty() {
            created += res.created;
            skipped += res.skipped;
            failed += res.failed;
        }

        // Record cloud ids for created entries so a later pull dedupes them and
        // a later archive can tombstone them by id.
        for item in &res.results {
            match item.status.as_str() {
                "created" => created += 1,
                "skipped" => skipped += 1,
                // Anything else — `"failed"`, or an unrecognized status — did
                // not affirmatively land; count it as failed rather than
                // silently dropping it from every tally.
                _ => failed += 1,
            }
            // Stamping `remote_id` is permanent (it's what excludes a row from
            // `live` on every future push), so only do it for a status that
            // affirmatively means the cloud durably has this row: `created`
            // (just persisted) or `skipped` (already persisted — dedup on
            // identity). Any other status — `failed`, or an id riding along
            // with a status that doesn't mean persisted — must not stamp, or
            // that row can never be retried again.
            let durably_persisted = item.status == "created" || item.status == "skipped";
            if durably_persisted
                && let (Some(ext), Some(cloud_id)) =
                    (item.external_id.as_deref(), item.id.as_deref())
                && let Some(row) = chunk.iter().find(|r| r.uuid == ext)
            {
                local.set_remote_id(row.local_id, cloud_id)?;
                just_synced.insert(row.local_id);
            }
            if item.status == "failed" {
                eprintln!(
                    "  [push-fail] {}",
                    item.external_id.as_deref().unwrap_or("<unknown>")
                );
            }
        }

        if multi_chunk {
            // `done` is the running durably-landed count (created + skipped),
            // which never exceeds `attempted`; the final `done` equals
            // `created + skipped`.
            on_progress((created + skipped) as usize, attempted);
        }
    }

    // Tombstone archived entries that the cloud already knows about. An archived
    // row with no `remote_id` was never pushed live, so there is nothing to
    // delete cloud-side; we skip it. Skipped entirely on an interrupted push: the
    // connection is already failing and the remaining live chunks were not sent.
    if interrupted.is_none() && include_archived {
        for r in rows.iter().filter(|r| r.archived) {
            if let Some(remote_id) = r.remote_id.as_deref() {
                client.delete_remote(remote_id).await?;
            }
        }
    }

    // Propagate local relates_to edges completed by this round's entry push.
    // Best-effort and secondary to entries: a failure here warns but does not
    // fail the push (the entries already landed). Skipped on an interrupted
    // push, since the connection is already failing.
    let edges_pushed = if interrupted.is_none() {
        match push_relates_to_edges(local, client, &just_synced).await {
            Ok(n) => n,
            Err(e) => {
                eprintln!("warning: failed to push relates_to edges to the cloud: {e:#}");
                0
            }
        }
    } else {
        0
    };

    Ok(PushSummary {
        attempted,
        created,
        skipped,
        failed,
        already_synced,
        interrupted,
        embedded_locally: repair.embedded,
        without_local_vector: repair.without_vector,
        edges_pushed,
    })
}

/// Push the local `relates_to` edges completed by this round's entry push.
///
/// `just_synced` is the set of local row ids whose `remote_id` this push
/// stamped (the entries that reached the cloud this round). An edge is
/// propagated when it touches one of those rows and BOTH its endpoints are
/// synced, so each edge is posted exactly in the round its second endpoint
/// lands, never re-posted on a later no-op sync. `relates_to` is the only kind
/// pushed: a `supersedes` edge rides its entry's lifecycle, and `contradicts`
/// is server-generated. Server-side the batch route dedupes
/// (`ON CONFLICT DO NOTHING`), so a redundant re-push is still harmless.
///
/// Since `memory add --relates-to` is the only producer of a `relates_to` edge
/// and always mints a fresh (unsynced) entry as the edge's `from` endpoint,
/// that endpoint is always in `just_synced` the round the edge first becomes
/// pushable, so this never silently drops a genuinely new edge.
///
/// Returns the number of edges the server confirmed it stored (an `unresolved`
/// edge is not counted; it is retried when its endpoint later syncs).
async fn push_relates_to_edges(
    local: &MemoryStore,
    client: &CloudSyncClient,
    just_synced: &HashSet<i64>,
) -> Result<usize> {
    if just_synced.is_empty() {
        return Ok(0);
    }
    let edges: Vec<SyncEdgePush> = local
        .relates_to_edges_for_sync()?
        .into_iter()
        .filter(|e| just_synced.contains(&e.from_local_id) || just_synced.contains(&e.to_local_id))
        .map(|e| SyncEdgePush {
            from_external_id: e.from_external_id,
            to_external_id: e.to_external_id,
            kind: "relates_to",
        })
        .collect();
    if edges.is_empty() {
        return Ok(0);
    }
    Ok(client.push_edges(edges).await?.applied())
}

#[cfg(test)]
mod chunking_tests;
#[cfg(test)]
mod counting_tests;
#[cfg(test)]
mod edge_tests;
#[cfg(test)]
mod embed_routing_tests;
#[cfg(test)]
mod embed_scope_tests;
#[cfg(test)]
mod embed_tests;
#[cfg(test)]
mod vector_tests;
