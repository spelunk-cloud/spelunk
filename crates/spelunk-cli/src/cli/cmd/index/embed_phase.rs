use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar};
use serde::{Deserialize, Serialize};

use super::super::ui::{is_tty, progress_style};
use crate::{capability::Tier, config::Config, embeddings::vec_to_blob, storage::Database};

/// Maximum chunks per request — enforced by the server (returns 413 if exceeded).
const MAX_BATCH: usize = 256;

#[derive(Serialize)]
struct EmbedRequest {
    chunks: Vec<ReqChunk>,
}

#[derive(Serialize)]
struct ReqChunk {
    chunk_id: String,
    content: String,
}

#[derive(Deserialize)]
struct EmbedResponse {
    chunks: Vec<RespChunk>,
}

#[derive(Deserialize)]
struct RespChunk {
    chunk_id: String,
    vector: Vec<f32>,
}

/// Send pending chunks to `spelunk-server` for embedding and write the returned
/// vectors into the local DB.
///
/// Returns the number of chunks successfully embedded.
///
/// Requires `Tier::Server`; returns `Ok(0)` immediately for `Tier::Offline`.
pub(super) async fn run_embed_phase(
    chunk_ids_and_texts: Vec<(i64, String)>,
    db: &Database,
    cfg: &Config,
    tier: &Tier,
    project_root: &std::path::Path,
    mp: &MultiProgress,
) -> Result<u64> {
    let (server_url, server_key) = match tier {
        Tier::Server { url, .. } => (url.clone(), cfg.server_key.clone()),
        Tier::Offline => return Ok(0),
    };

    // Use `resolve_project_id` so that loopback auto-discovered servers (where
    // `cfg.project_id` may be absent) derive the id from the project root path,
    // matching `Config::resolve_project_id` behaviour (see spelunk#307).
    let project_id_owned = cfg.resolve_project_id(project_root);
    let project_id = project_id_owned.as_str();

    // 600 s: the native CPU embedder can take ~400 ms/chunk; at MAX_BATCH=256
    // that is ~100 s per request.  Give 6× headroom so large codebases and
    // slow machines don't hit the deadline.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .context("building HTTP client for embed phase")?;

    let total = chunk_ids_and_texts.len() as u64;
    let bar = if is_tty() && !crate::utils::is_agent_mode() {
        let b = mp.add(ProgressBar::new(total));
        b.set_style(progress_style("Embedding"));
        b
    } else {
        ProgressBar::hidden()
    };

    let mut embedded = 0u64;

    for batch in chunk_ids_and_texts.chunks(MAX_BATCH) {
        let req_chunks: Vec<ReqChunk> = batch
            .iter()
            .map(|(id, text)| ReqChunk {
                chunk_id: id.to_string(),
                content: text.clone(),
            })
            .collect();

        // Percent-encode the project_id path segment: slugs contain `/`
        // (`local/<hex>`, `github.com/owner/repo`) which would otherwise split
        // the segment and break axum routing → 404. See spelunk decision #106.
        let url = format!(
            "{}/v1/projects/{}/index/embed",
            server_url.trim_end_matches('/'),
            crate::server_client::encode_project_id(project_id),
        );

        let mut req = client.post(&url).json(&EmbedRequest { chunks: req_chunks });
        if let Some(k) = &server_key {
            req = req.bearer_auth(k);
        }

        let resp: EmbedResponse = req
            .send()
            .await
            .with_context(|| format!("calling {url}"))?
            .error_for_status()
            .context("server returned an error for index/embed")?
            .json()
            .await
            .context("parsing index/embed response")?;

        for item in &resp.chunks {
            if let Ok(row_id) = item.chunk_id.parse::<i64>() {
                let blob = vec_to_blob(&item.vector);
                db.insert_embedding(row_id, &blob)?;
                embedded += 1;
                bar.inc(1);
            }
        }
    }

    bar.finish_with_message(format!("{embedded} chunks embedded"));
    Ok(embedded)
}
