//! The pre-batch local-embedding repair shared by `spelunk sync` and
//! `spelunk memory push`.
//!
//! A push used to leave `memory.db` exactly as it found it, so an entry that
//! had never been embedded stayed invisible to semantic `memory search` after a
//! successful push, with nothing telling the user. This module embeds the push
//! set locally first and commits each vector as it completes.

use anyhow::Result;

use super::PushSummary;
use crate::{
    capability,
    config::{Config, SyncMode},
    embeddings::vec_to_blob,
    server_client::ServerInferenceClient,
    storage::{MemoryStore, SyncRow},
};

/// Whether a push repairs the local store's missing embeddings before building
/// the batch, and the config the local embedder is resolved from.
///
/// Constructed by the command layer via [`LocalEmbedPolicy::for_push`] so
/// `spelunk sync` and `spelunk memory push` decide it identically.
pub(in crate::cli::cmd::memory) enum LocalEmbedPolicy<'a> {
    /// Embed every push-set row that lacks a usable local vector, through the
    /// loopback embedder, and commit the result to `memory.db`.
    Repair {
        cfg: &'a Config,
        project_root: std::path::PathBuf,
    },
    /// Leave local embeddings alone.
    Skip,
}

impl<'a> LocalEmbedPolicy<'a> {
    /// Decide the policy for a push against `mem_path`.
    ///
    /// `cloud_first` with a team `server_url` relocates the store of record off
    /// `memory.db`, so there is nothing local to repair. This is the exact
    /// condition `memory reindex` refuses under, and the two commands must not
    /// disagree about when local embeddings are meaningful.
    pub(in crate::cli::cmd::memory) fn for_push(
        cfg: &'a Config,
        mem_path: &std::path::Path,
    ) -> Self {
        if cfg.resolve_mode() == SyncMode::CloudFirst && cfg.server_url.is_some() {
            return Self::Skip;
        }
        Self::Repair {
            cfg,
            project_root: mem_path.parent().unwrap_or(mem_path).to_path_buf(),
        }
    }
}

/// Counted outcome of the pre-batch local-embedding repair.
pub(super) struct RepairCounts {
    pub(super) embedded: usize,
    pub(super) without_vector: usize,
}

/// The one user-facing warning for entries that were pushed with no local
/// embedding. Emitted once per run by the command layer, which owns all
/// user-facing output; the shared push pass only counts.
pub(in crate::cli::cmd::memory) fn unembedded_warning(count: usize) -> String {
    let entries = if count == 1 { "entry" } else { "entries" };
    format!(
        "warning: {count} {entries} pushed without a local embedding, so \
         `spelunk memory search` cannot surface {} in this project until \
         `spelunk memory reindex` is run.",
        if count == 1 { "it" } else { "them" }
    )
}

/// Local-embedding clause for a push/sync summary line. Empty when the repair
/// neither minted nor missed a vector, so an unaffected push reads exactly as
/// it did before.
pub(in crate::cli::cmd::memory) fn local_embed_summary(summary: &PushSummary) -> String {
    match (summary.embedded_locally, summary.without_local_vector) {
        (0, 0) => String::new(),
        (embedded, 0) => format!(" Embedded {embedded} locally."),
        (embedded, missing) => {
            format!(" Embedded {embedded} locally, {missing} without a local embedding.")
        }
    }
}

/// Decode a stored embedding blob into a vector usable for this push: `None`
/// when the row has no embedding at all, or when the blob does not decode to
/// exactly `EMBEDDING_DIM` floats (a torn write). A row with no usable vector
/// is treated as unembedded by both the repair pass and the batch build.
pub(super) fn usable_vector(blob: Option<Vec<u8>>) -> Option<Vec<f32>> {
    blob.map(|b| spelunk_core::embeddings::blob_to_vec(&b))
        .filter(|v| v.len() == spelunk_core::embeddings::EMBEDDING_DIM)
}

/// Resolve the embedder the repair pass runs through.
///
/// `get_inference_tier` (not `get_tier`) is what keeps this on the loopback
/// embedder: outside `cloud_first` it probes loopback only, so the embed can
/// never be routed to a configured team `server_url`. Routing it there would
/// re-create the exact server-side re-embedding this repair exists to stop.
async fn resolve_local_embedder(
    cfg: &Config,
    project_root: &std::path::Path,
) -> Option<ServerInferenceClient> {
    let tier = capability::get_inference_tier(cfg).await;
    // An auto-discovered loopback server sets the tier without populating
    // `server_url`, so bridge it into an effective config first (ADR-004),
    // exactly as `memory reindex` does.
    let eff_cfg = tier.effective_config(cfg, project_root);
    ServerInferenceClient::from_config(&eff_cfg)
}

/// Embed every push-set row that lacks a usable local vector and commit it to
/// `memory.db`, so a pushed row is searchable locally afterwards rather than
/// silently invisible to semantic `memory search`.
///
/// Never fails the push: with no embedder reachable, or on a single row's embed
/// error, the affected rows go out text-only and are counted for the caller's
/// warning.
pub(super) async fn repair_local_embeddings(
    local: &MemoryStore,
    live: &[&SyncRow],
    policy: &LocalEmbedPolicy<'_>,
) -> Result<RepairCounts> {
    let (cfg, project_root) = match policy {
        LocalEmbedPolicy::Skip => {
            return Ok(RepairCounts {
                embedded: 0,
                without_vector: 0,
            });
        }
        LocalEmbedPolicy::Repair { cfg, project_root } => (*cfg, project_root),
    };

    let mut missing: Vec<&SyncRow> = Vec::new();
    for r in live {
        if usable_vector(local.get_embedding(r.local_id)?).is_none() {
            missing.push(r);
        }
    }
    // Resolve the embedder only once a row actually needs one: an empty or
    // fully-embedded push set must not pay for a discovery probe, and must not
    // warn about an embedder it never needed.
    if missing.is_empty() {
        return Ok(RepairCounts {
            embedded: 0,
            without_vector: 0,
        });
    }

    let Some(client) = resolve_local_embedder(cfg, project_root).await else {
        // Text-only rather than a refusal: failing here would break scripted and
        // CI pushes that work today and never needed a local embedder.
        return Ok(RepairCounts {
            embedded: 0,
            without_vector: missing.len(),
        });
    };

    let mut embedded = 0usize;
    let mut without_vector = 0usize;
    for r in missing {
        // Byte-identical to `memory reindex` / `memory add`'s document string,
        // embedded document-side (`embed_text`, never `embed_query`, which
        // prepends the F2LLM `Instruct:/Query:` prefix): a vector minted here
        // must be interchangeable with theirs or the repaired row is ranked
        // against a different space.
        let doc = format!("title: {} | text: {}", r.title, r.body);
        match client.embed_text(&doc).await {
            Ok(vec) => {
                // Committed per row, so an interrupted push keeps every vector
                // it minted and a re-run embeds only the remainder.
                local.insert_embedding(r.local_id, &vec_to_blob(&vec))?;
                embedded += 1;
            }
            Err(e) => {
                // One row's embed failure must not abort a push that is
                // otherwise fine; it ships text-only and is counted instead.
                tracing::warn!("embedding note #{} before push failed: {e:#}", r.local_id);
                without_vector += 1;
            }
        }
    }

    Ok(RepairCounts {
        embedded,
        without_vector,
    })
}
