//! `spelunk memory push` — one-way push of local memory to the cloud.
//!
//! Changes vs. the MVP placeholder:
//! - **Text-only by default; optional pushed vector.** The client ships no
//!   vector and the server backfills the embedding with its configured model
//!   — unless the server advertises `accepts_pushed_vectors`, in which case an
//!   entry with a local fp32/896 embedding carries it (same model + dim as the
//!   server), so the server stores it as-is instead of re-embedding. The old
//!   unconditional `get_embedding` send path (which carried a *mismatched*
//!   local model's vectors into the cloud's space and broke KNN) stays gone;
//!   the gated path only sends a vector the server has said it will accept for
//!   its own space.
//! - **Batched.** Entries go via `POST /memory/batch`, not N single POSTs.
//! - **Idempotent.** Each entry carries its stable UUID as the cloud
//!   `external_id`, so re-pushing skips already-present entries instead of
//!   duplicating them.
//! - **Lifecycle.** `supersedes` and archive/tombstone state are propagated
//!   (the batch payload carries them via the shared sync path), not dropped.
//!
//! For two-way convergence (push + pull) use `spelunk sync`. This command is the
//! one-way "seed the cloud" operation.

use anyhow::{Context, Result};

use super::MemoryPushArgs;
use super::sync::push_local_oneway;
use crate::{
    capability,
    cli::cmd::auth_api,
    config::Config,
    storage::{CloudSyncClient, MemoryStore},
};

pub async fn memory_push(
    args: MemoryPushArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    _backend_override: Option<&str>,
) -> Result<()> {
    let tier = capability::get_tier(cfg).await;
    capability::require_tier1("memory push", tier, cfg.server_url.as_deref())?;

    let base_url = cfg
        .server_url
        .clone()
        .ok_or_else(|| anyhow::anyhow!("memory push requires `server_url` to be configured."))?;
    let project_id = cfg.project_id.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "`project_id` is not configured. Set it in `.spelunk/config.toml` \
             or via `SPELUNK_PROJECT_ID`."
        )
    })?;

    let src_path = args.source.as_deref().unwrap_or(mem_path);
    let local = MemoryStore::open(src_path)
        .with_context(|| format!("opening local memory at {}", src_path.display()))?;
    // Refresh a stale WorkOS access token before the cloud-api call.
    let key = auth_api::ensure_fresh_server_key(cfg, &base_url).await?;
    let client = CloudSyncClient::new(
        &base_url,
        &project_id,
        key.as_deref(),
        cfg.server_ca.as_deref().map(std::path::Path::new),
    )?;

    println!("Pushing local memory to {base_url}…");
    // Attach the local fp32/896 vector only when the server advertises it;
    // otherwise the push is text-only and the server re-embeds.
    let accepts_pushed_vectors = tier.caps().is_some_and(|c| c.accepts_pushed_vectors);
    let summary = push_local_oneway(
        &local,
        &client,
        args.include_archived,
        accepts_pushed_vectors,
    )
    .await?;
    if summary.attempted == 0 {
        if summary.already_synced > 0 {
            println!(
                "Nothing to push — {} entries already synced.",
                summary.already_synced
            );
        } else {
            println!("No local memory entries to push.");
        }
    } else if summary.created == 0 && summary.skipped == 0 {
        // Total failure: nothing durably landed. Must not read as success —
        // a caller skimming for "Done" or checking only the exit code would
        // otherwise miss a fully-failed batch (this is the data-loss shape
        // the bug was filed over).
        anyhow::bail!(
            "Push failed: 0 of {} entries reached the server ({} failed).",
            summary.attempted,
            summary.failed
        );
    } else if summary.failed > 0 {
        println!(
            "Done. Pushed {} entries (created {}, skipped {}, {} failed).",
            summary.attempted, summary.created, summary.skipped, summary.failed
        );
    } else {
        println!(
            "Done. Pushed {} entries (created {}, skipped {}).",
            summary.attempted, summary.created, summary.skipped
        );
    }
    Ok(())
}
