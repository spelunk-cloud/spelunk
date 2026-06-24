//! `spelunk memory push` — one-way push of local memory to the cloud.
//!
//! ADR-037 D2/D3 changes vs. the MVP placeholder:
//! - **Text-only.** The client never ships a vector; the server backfills the
//!   embedding with its configured model (ADR-010/ADR-020). The old
//!   `local.get_embedding(...)` send path is gone — it carried the *local*
//!   model's vectors into the cloud's space and broke KNN over synced rows.
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
    let client = CloudSyncClient::new(&base_url, &project_id, cfg.server_key.as_deref())?;

    println!("Pushing local memory to {base_url}…");
    let summary = push_local_oneway(&local, &client, args.include_archived).await?;
    if summary.attempted == 0 {
        println!("No local memory entries to push.");
    } else {
        println!(
            "Done. Pushed {} entries (created {}, skipped {}).",
            summary.attempted, summary.created, summary.skipped
        );
    }
    Ok(())
}
