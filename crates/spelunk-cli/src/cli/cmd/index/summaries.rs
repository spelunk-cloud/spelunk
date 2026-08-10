use anyhow::Result;
use std::sync::Arc;

use crate::{
    capability::{self, LlmFeature},
    config::Config,
    server_client::ServerLlmAdapter,
    storage::Database,
};

/// Run the optional LLM summary generation pass.
///
/// Fetches chunks without summaries in batches, calls the LLM via
/// spelunk-server, and stores results.
///
/// Summaries are optional, so every no-LLM outcome is a skip with a notice and
/// an `Ok`, never a failed index.
pub(super) async fn generate_summaries(
    no_summaries: bool,
    summary_batch_size: usize,
    cfg: &Config,
    db: &Database,
    project_root: &std::path::Path,
) -> Result<()> {
    if no_summaries {
        return Ok(());
    }

    // Count total chunks needing summaries for progress reporting. Checked
    // before routing so a re-index with nothing to summarise neither probes
    // nor prints a notice about an LLM it was never going to call.
    let batch_size = summary_batch_size.max(1);
    let first_batch = db.chunks_without_summaries(1)?;
    if first_batch.is_empty() {
        return Ok(());
    }

    let route = capability::resolve_llm_route(cfg, project_root).await;
    let Some(client) = route.client() else {
        if let Some(reason) = route.reason() {
            eprintln!(
                "{}",
                capability::no_llm_message(reason, LlmFeature::Summaries)
            );
        }
        return Ok(());
    };
    let llm = ServerLlmAdapter(Arc::new(client));

    // Count pending chunks for progress display.
    let pending = db.chunks_without_summaries(usize::MAX)?;
    let total_chunks = pending.len();
    let total_batches = total_chunks.div_ceil(batch_size);

    eprintln!("Generating summaries ({total_chunks} chunks, batch size {batch_size})\u{2026}");

    let mut batch_num = 0usize;
    let mut failed_batches = 0usize;
    loop {
        let batch = db.chunks_without_summaries(batch_size)?;
        if batch.is_empty() {
            break;
        }
        batch_num += 1;
        eprintln!("  Summarising batch {batch_num}/{total_batches}\u{2026}");

        match crate::indexer::summariser::summarise_batch(&llm, &batch).await {
            Ok(summaries) => {
                // summarise_batch reports LLM/transport failure as an empty result.
                if summaries.is_empty() {
                    failed_batches += 1;
                }
                let mut summarised_ids = std::collections::HashSet::new();
                for (chunk_id, summary) in summaries {
                    // A secret can appear in an LLM-generated summary even when the
                    // underlying chunk was clean (the model may echo surrounding
                    // context). Scan before storing, since the summary is prepended
                    // into `embedding_text()` and thus gets embedded. Best-effort
                    // defense-in-depth, not a security boundary — see secrets.rs.
                    let summary_to_store = if crate::indexer::secrets::contains_secret(&summary) {
                        tracing::warn!(
                            "dropping summary for chunk {chunk_id} (possible secret detected)"
                        );
                        ""
                    } else {
                        summary.as_str()
                    };
                    if let Err(e) = db.update_chunk_summary(chunk_id, summary_to_store) {
                        tracing::warn!("failed to store summary for chunk {chunk_id}: {e}");
                    } else {
                        summarised_ids.insert(chunk_id);
                    }
                }
                // Mark chunks that received no summary with "" so they aren't
                // re-fetched on the next pass (chunks_without_summaries checks IS NULL).
                for (id, _, _, _) in &batch {
                    if !summarised_ids.contains(id) {
                        let _ = db.update_chunk_summary(*id, "");
                    }
                }
            }
            Err(e) => {
                failed_batches += 1;
                tracing::warn!("summarise_batch failed: {e}");
                // Mark the batch as attempted so we don't loop forever.
                for (id, _, _, _) in &batch {
                    let _ = db.update_chunk_summary(*id, "");
                }
            }
        }
    }

    let ok_batches = batch_num - failed_batches;
    eprintln!("  Summarised {ok_batches} batch(es).");
    if failed_batches > 0 {
        // Failed chunks are stored as "" (not NULL), so a plain re-run skips
        // them; --force reparses and clears the summary back to NULL.
        eprintln!(
            "Warning: {failed_batches} of {batch_num} summary batch(es) produced no summary; \
             those chunks are indexed without one. Re-run with `spelunk index --force` to retry \
             (`RUST_LOG=warn` shows the cause)."
        );
    }
    Ok(())
}
