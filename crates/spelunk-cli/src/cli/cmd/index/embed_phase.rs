use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Serialize;

use super::super::ui::is_tty;
use crate::{
    capability::{ServerLimits, Tier},
    config::Config,
    storage::Database,
};

/// Hard ceiling on chunks per request; the server returns 413 above this.
const MAX_BATCH: usize = 256;

/// Calibration ceiling when `--batch-size` is unset (0): the server's hard
/// limit. `--batch-size` only lowers this ceiling, never picks a fixed size
/// (see `resolve_batch_ceiling`).
const DEFAULT_BATCH_CEILING: usize = MAX_BATCH;

/// First request is a single chunk: yields an initial per-entry estimate almost
/// immediately and gets the progress bar moving before any full batch lands.
const CALIBRATION_BATCH_1: usize = 1;

/// Second request: refines the estimate from `CALIBRATION_BATCH_1` (dominated by
/// one-off cold-start) before committing to a steady-state size.
const CALIBRATION_BATCH_2: usize = 4;

/// Wall-clock time each steady-state batch aims to stay under; a batch is
/// sized so its token sum fits this budget at the measured token rate.
const TARGET_BATCH_SECONDS: u64 = 240;

/// Floor for a calibrated per-request timeout, to absorb transient latency spikes.
const MIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Ceiling for a calibrated per-request timeout, so a pathologically slow sample
/// can't derive an unbounded deadline.
const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(1800);

/// Headroom multiple over a batch's expected duration when deriving its timeout.
const TIMEOUT_SAFETY_FACTOR: u32 = 4;

/// Effective ceiling the calibrated batch size may grow to: `--batch-size`
/// (0 → `DEFAULT_BATCH_CEILING`) clamped to `MAX_BATCH` and, when advertised,
/// the server's own `max_batch_chunks` (413 above it). Only an upper bound —
/// actual size is calibrated (see `next_batch_size`).
fn resolve_batch_ceiling(requested: usize, server_max_batch_chunks: Option<usize>) -> usize {
    let ceiling = if requested == 0 {
        DEFAULT_BATCH_CEILING
    } else {
        requested.min(MAX_BATCH)
    };
    match server_max_batch_chunks {
        Some(server_max) => ceiling.min(server_max),
        None => ceiling,
    }
}

/// Per-request budget assumed for a server that pre-dates the `/v1/health`
/// `limits` field: the old blanket 30s `TimeoutLayer` with no `/index/embed`
/// exemption.
const LEGACY_SERVER_REQUEST_BUDGET_SECS: u64 = 30;

/// Fraction of the server's per-request budget a calibrated batch targets,
/// leaving headroom for jitter between the calibration sample and the batch sent.
const SERVER_BUDGET_TARGET_FRACTION: f64 = 2.0 / 3.0;

/// Effective target batch duration (seconds), clamped to fit the server's
/// advertised `/index/embed` budget. Absent `limits` (older server) falls back
/// to `SERVER_BUDGET_TARGET_FRACTION × LEGACY_SERVER_REQUEST_BUDGET_SECS`. The
/// 408-triggered shrink in `run_embed_phase` is the fallback.
fn resolve_target_batch_seconds(server_limits: Option<ServerLimits>) -> u64 {
    let budget_secs = server_limits
        .map(|l| l.embed_request_timeout_secs)
        .unwrap_or(LEGACY_SERVER_REQUEST_BUDGET_SECS);
    let safe_budget = (budget_secs as f64 * SERVER_BUDGET_TARGET_FRACTION).floor() as u64;
    TARGET_BATCH_SECONDS.min(safe_budget.max(1))
}

/// Max multiple of the previous batch's size the next calibrated batch may grow
/// to in one step, so one fast sample can't leap to a size nothing has measured.
const GROWTH_FACTOR: usize = 8;

/// Choose the next steady-state batch length (in chunks) so the batch's
/// **token** sum fits ~`target_seconds` at the measured `per_token` rate.
/// `token_tail` is the per-chunk token counts of the queue from the cursor on.
/// Clamped to `[1, ceiling]` and to at most `GROWTH_FACTOR ×
/// previous_batch_size` (both in chunks).
///
/// Sizing by tokens rather than chunk count is what keeps the derived deadline
/// honest across a size transition in the queue: per-chunk cost grows ~4x
/// through an id-ordered queue, so a chunk-count budget calibrated on early
/// (small) chunks over-fills a batch of late (large) ones.
fn next_batch_len(
    per_token: Duration,
    token_tail: &[usize],
    ceiling: usize,
    previous_batch_size: usize,
    target_seconds: u64,
) -> usize {
    let growth_cap = previous_batch_size
        .max(1)
        .saturating_mul(GROWTH_FACTOR)
        .min(ceiling.max(1));

    if per_token.is_zero() {
        return growth_cap.min(token_tail.len().max(1));
    }
    let target_tokens = Duration::from_secs(target_seconds).as_secs_f64() / per_token.as_secs_f64();

    // Take chunks while their cumulative token sum stays within the target;
    // always at least one so progress can't stall.
    let mut len = 0usize;
    let mut tokens = 0f64;
    for &tc in token_tail.iter().take(growth_cap) {
        tokens += tc.max(1) as f64;
        if len > 0 && tokens > target_tokens {
            break;
        }
        len += 1;
    }
    len.max(1)
}

/// Per-request timeout for a batch: `TIMEOUT_SAFETY_FACTOR ×` its expected
/// duration at `per_token` over the batch's token sum, clamped to
/// `[MIN_REQUEST_TIMEOUT, MAX_REQUEST_TIMEOUT]`. The rate and the deadline
/// share the token unit, so a size transition in the queue moves the deadline
/// with the batch's real cost instead of consuming the safety margin.
fn batch_timeout(per_token: Duration, batch_tokens: u64) -> Duration {
    let expected_secs = per_token.as_secs_f64() * batch_tokens.max(1) as f64;
    let budget_secs = (expected_secs * TIMEOUT_SAFETY_FACTOR as f64)
        .clamp(0.0, MAX_REQUEST_TIMEOUT.as_secs_f64());
    Duration::from_secs_f64(budget_secs).clamp(MIN_REQUEST_TIMEOUT, MAX_REQUEST_TIMEOUT)
}

/// Timeout for the very first (single-chunk) request, before any rate is known.
/// Pessimistic to absorb one-off model cold-start.
const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Weight given to `CALIBRATION_BATCH_1`'s 1-entry sample when the second sample
/// arrives. Small: that sample is dominated by one-off per-request overhead that
/// doesn't repeat, so it must not carry the weight of a real multi-entry
/// measurement. Both the sizing decision and the displayed rate read the same
/// blended estimate.
const CALIBRATION_BATCH_1_WEIGHT: f64 = 0.1;

/// Running estimate of this run's embedding throughput, refined after every
/// batch so mid-run drift (thermal throttling, GPU contention) is picked up.
///
/// Single authoritative rate source: `next_batch_len`, `batch_timeout`, and the
/// displayed ETA (`format_eta`) all read `per_token()` from the same instance so
/// they can't disagree. Deliberately not indicatif's `{eta}`, which infers rate
/// from `bar.inc(1)` timing — wrong for this phase's bursty increments (see
/// `format_eta`).
///
/// The rate is **per estimated token**, not per chunk: per-chunk cost is not
/// stationary through an id-ordered queue (~4x growth), so a per-chunk rate is
/// systematically biased in one direction across the run. The token estimate's
/// own corpus-dependent bias cancels here because the rate is calibrated from
/// estimated tokens and only ever multiplied by estimated tokens; the rate
/// must stay measured per-run, never cached across runs or repos.
struct RateEstimate {
    /// Exponentially-weighted per-token duration. `None` until the first batch.
    per_token: Option<Duration>,
    /// Batches folded in so far, so `update` can tell the batch-1 cold sample
    /// (== 1) from later steady-state blends.
    samples_seen: u32,
}

impl RateEstimate {
    fn new() -> Self {
        Self {
            per_token: None,
            samples_seen: 0,
        }
    }

    /// Fold in a batch: `elapsed` for `tokens` estimated tokens. First
    /// observation seeds the estimate; the second de-weights the batch-1 cold
    /// sample (`CALIBRATION_BATCH_1_WEIGHT`); from the third onward, a 50/50
    /// EMA so mid-run rate changes are reflected within a couple of batches.
    fn update(&mut self, elapsed: Duration, tokens: u64) {
        if tokens == 0 {
            return;
        }
        let sample = elapsed.div_f64(tokens as f64);
        self.per_token = Some(match self.per_token {
            None => sample,
            Some(prev) if self.samples_seen == 1 => {
                // Superseding the batch-1 cold sample: de-weight it.
                let w = CALIBRATION_BATCH_1_WEIGHT;
                let blended = prev.as_secs_f64() * w + sample.as_secs_f64() * (1.0 - w);
                Duration::from_secs_f64(blended)
            }
            Some(prev) => {
                // Steady-state 50/50 EMA.
                let blended = (prev.as_secs_f64() + sample.as_secs_f64()) / 2.0;
                Duration::from_secs_f64(blended)
            }
        });
        self.samples_seen += 1;
    }

    /// Current best estimate, or `None` before the first batch has landed.
    fn per_token(&self) -> Option<Duration> {
        self.per_token
    }
}

/// Progress style for the embed phase. Does NOT use indicatif's `{eta}`:
/// embedding is bursty (a batch's `bar.inc(1)` calls land together, then a long
/// silent gap), which indicatif reads as rate ≈ 0 and extrapolates absurd ETAs.
/// The ETA is computed from `RateEstimate` via `format_eta` and rendered into
/// `{wide_msg}` by `run_embed_phase` instead.
fn embed_progress_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.cyan} Embedding [{bar:38.cyan/blue}] {pos}/{len}  {wide_msg}",
    )
    .unwrap()
    .progress_chars("=>-")
}

/// Ceiling on the displayed ETA: anything at or above shows `ETA >24h` rather
/// than a literal (possibly absurd) computed duration.
const ETA_DISPLAY_CAP: Duration = Duration::from_secs(24 * 60 * 60);

/// Render the displayed ETA from the measured `RateEstimate` and estimated
/// tokens remaining.
///
/// - `None` per_token (pre-first-batch): a calibrating placeholder, not a guess.
/// - Else `per_token * remaining tokens`, computed in f64 and clamped BEFORE
///   converting back to `Duration` (a pathological rate can overflow/produce
///   `inf`), so a bad sample yields the `>24h` string, never a panic.
/// - Compact format: seconds / minutes(+seconds) / hours+minutes.
fn format_eta(remaining_tokens: u64, per_token: Option<Duration>) -> String {
    let Some(per_token) = per_token else {
        return "ETA calibrating…".to_string();
    };
    if remaining_tokens == 0 {
        return "ETA 0s".to_string();
    }

    let seconds = (per_token.as_secs_f64() * remaining_tokens as f64).clamp(0.0, f64::MAX);
    if !seconds.is_finite() || seconds >= ETA_DISPLAY_CAP.as_secs_f64() {
        return "ETA >24h".to_string();
    }
    let remaining_duration = Duration::from_secs_f64(seconds);

    let total_secs = remaining_duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        format!("ETA {hours}h{minutes:02}m")
    } else if minutes > 0 {
        if secs > 0 {
            format!("ETA {minutes}m{secs}s")
        } else {
            format!("ETA {minutes}m")
        }
    } else {
        format!("ETA {secs}s")
    }
}

/// Integer percentage of `done` over `total`, 0 when `total` is 0. Callers
/// must label the result with its denominator; a bare percentage is banned
/// from every embedding-state surface.
fn pct(done: u64, total: u64) -> u64 {
    done.saturating_mul(100).checked_div(total).unwrap_or(0)
}

#[derive(Serialize)]
struct EmbedRequest {
    chunks: Vec<ReqChunk>,
}

#[derive(Serialize)]
struct ReqChunk {
    chunk_id: String,
    content: String,
}

/// Report an unrecoverable embed-phase failure: abandon the progress bar and
/// print an actionable message to stderr (naming the server request budget).
/// Does NOT return `Err` — callers report the
/// count embedded so far via `Ok(embedded)`.
fn report_embed_failure(
    bar: &ProgressBar,
    embedded: u64,
    total: u64,
    server_url: &str,
    err: anyhow::Error,
) {
    bar.abandon_with_message(format!(
        "batch failed after {embedded}/{total} embedded; re-run `spelunk index` to finish the rest",
    ));
    eprintln!("Embedding stopped after {embedded}/{total} chunks embedded and saved: {err:#}");
    eprintln!(
        "Re-run `spelunk index` to embed the remaining {} chunk(s); already-embedded chunks \
         are skipped.",
        total - embedded,
    );
    eprintln!(
        "If this keeps happening: the spelunk-server at {server_url} may be enforcing a \
         smaller request budget than this batch needs."
    );
}

/// Send pending chunks to `spelunk-server` for embedding and write the returned
/// vectors into the local DB.
///
/// `chunk_ids_and_texts` items are `(chunk_id, embedding_text, token_count)`;
/// the token counts weight the progress/ETA display, batch sizing, and request
/// deadlines (all through the same `RateEstimate`).
///
/// Returns the number of chunks successfully embedded.
///
/// Requires `Tier::Server`; returns `Ok(0)` immediately for `Tier::Offline`.
pub(super) async fn run_embed_phase(
    chunk_ids_and_texts: Vec<(i64, String, usize)>,
    db: &Database,
    cfg: &Config,
    tier: &Tier,
    project_root: &std::path::Path,
    batch_size: usize,
    mp: &MultiProgress,
) -> Result<u64> {
    let (server_url, server_key) = match tier {
        Tier::Server { url, .. } => (url.clone(), cfg.bearer_for(url)?),
        Tier::Offline => return Ok(0),
    };
    // Refuse to append vectors from a different model into an existing index;
    // stamps provenance on a fresh/legacy DB.
    db.ensure_embedding_model(spelunk_core::embeddings::MODEL_ID)?;
    let server_limits = tier.server_limits();

    // Ceiling the calibrated batch size may grow to (see `next_batch_size`),
    // clamped to the server's advertised `max_batch_chunks` when known.
    let server_max_batch_chunks = server_limits.map(|l| l.max_batch_chunks);
    let ceiling = resolve_batch_ceiling(batch_size, server_max_batch_chunks);

    // Target batch duration, clamped to the server's advertised (or legacy)
    // `/index/embed` budget — see `resolve_target_batch_seconds`. The 408
    // shrink below is the fallback.
    let target_batch_seconds = resolve_target_batch_seconds(server_limits);
    if server_limits.is_none() {
        // Older server: no `limits` field, so it may still enforce the blanket
        // 30s budget. Target smaller batches to keep the run working.
        eprintln!(
            "Note: spelunk-server at {server_url} did not report its /index/embed request \
             budget; assuming a conservative {LEGACY_SERVER_REQUEST_BUDGET_SECS}s budget and \
             targeting smaller batches accordingly."
        );
    }

    // Loopback auto-discovered servers may lack `cfg.project_id`; derive it from
    // the project root, matching `Config::resolve_project_id`.
    let project_id_owned = cfg.resolve_project_id(project_root);
    let project_id = project_id_owned.as_str();

    // No client-wide timeout: a PER-REQUEST timeout is applied below, derived
    // from the measured rate (pessimistic for the first, single-chunk request).
    // A single fixed deadline let a slow first batch expire with nothing saved.
    let client = spelunk_core::config::apply_server_ca(
        reqwest::Client::builder(),
        cfg.server_ca.as_deref().map(std::path::Path::new),
    )?
    .build()
    .context("building HTTP client for embed phase")?;

    let total = chunk_ids_and_texts.len() as u64;
    let bar = if is_tty() && !crate::utils::is_agent_mode() {
        let b = mp.add(ProgressBar::new(total));
        b.set_style(embed_progress_style());
        b
    } else {
        ProgressBar::hidden()
    };

    // Draw the bar before the first request fires so the phase shows movement
    // immediately; the steady tick animates the spinner while a request is in
    // flight so a slow batch never looks frozen.
    bar.set_message("calibrating batch size\u{2026}");
    bar.enable_steady_tick(std::time::Duration::from_millis(120));
    bar.tick();

    let mut rate = RateEstimate::new();
    let mut embedded = 0u64;
    let mut cursor = 0usize;
    let mut batch_num = 0u64;
    let mut previous_batch_size = 1usize;
    let remaining = chunk_ids_and_texts.len();
    // Token-weighted work totals: the ETA and the "of work done" percentage
    // run over these, never over chunk counts (chunk fraction is coverage, a
    // different question; see `status`).
    let total_tokens: u64 = chunk_ids_and_texts
        .iter()
        .map(|(_, _, tc)| (*tc).max(1) as u64)
        .sum();
    let mut tokens_done = 0u64;
    // Percent-encode the project_id segment: slugs contain `/`
    // (`local/<hex>`, `github.com/owner/repo`) which would otherwise split the
    // segment and break axum routing → 404.
    let url = format!(
        "{}/v1/projects/{}/index/embed",
        server_url.trim_end_matches('/'),
        crate::server_client::encode_project_id(project_id),
    );

    while cursor < remaining {
        batch_num += 1;
        let left = remaining - cursor;

        // Calibration: first request 1 chunk, second 4 chunks (both clamped to
        // what's left), to gather timing before committing to a steady-state size.
        let mut this_batch_size = match batch_num {
            1 => CALIBRATION_BATCH_1,
            2 => CALIBRATION_BATCH_2,
            _ => {
                let per_token = rate
                    .per_token()
                    .expect("rate is seeded after the first batch completes");
                let token_tail: Vec<usize> = chunk_ids_and_texts[cursor..]
                    .iter()
                    .map(|(_, _, tc)| *tc)
                    .collect();
                next_batch_len(
                    per_token,
                    &token_tail,
                    ceiling,
                    previous_batch_size,
                    target_batch_seconds,
                )
            }
        }
        .clamp(1, left);

        // Retry loop for THIS batch: a 408/timeout is recoverable — escalate
        // patience (calibration batch 1, no rate estimate yet) or shrink and
        // retry, rather than aborting at 0 embedded. Any other failure aborts.
        let mut escalated_calibration_once = false;
        let bytes = 'retry: loop {
            let batch_tokens: u64 = chunk_ids_and_texts[cursor..cursor + this_batch_size]
                .iter()
                .map(|(_, _, tc)| (*tc).max(1) as u64)
                .sum();
            let request_timeout = match rate.per_token() {
                Some(per_token) => batch_timeout(per_token, batch_tokens),
                None if escalated_calibration_once => MAX_REQUEST_TIMEOUT,
                None => FIRST_REQUEST_TIMEOUT,
            };

            // Show which chunks are in flight, prefixed with the `RateEstimate`
            // ETA (not indicatif's `{eta}`; see `format_eta`). Work-fraction
            // percentages are token-weighted and always name their denominator.
            let eta_str = format_eta(total_tokens.saturating_sub(tokens_done), rate.per_token());
            let work_pct = pct(tokens_done, total_tokens);
            bar.set_message(format!(
                "{eta_str}  \u{00b7}  sent {this_batch_size} chunk(s) ({embedded}/{total} chunks, \
                 {work_pct}% of work done), awaiting response\u{2026}",
            ));

            let batch = &chunk_ids_and_texts[cursor..cursor + this_batch_size];

            let req_chunks: Vec<ReqChunk> = batch
                .iter()
                .map(|(id, text, _)| ReqChunk {
                    chunk_id: id.to_string(),
                    content: text.clone(),
                })
                .collect();

            let started = Instant::now();
            let outcome = embed_one_batch(
                &client,
                &url,
                server_key.as_deref(),
                EmbedRequest { chunks: req_chunks },
                batch.len(),
                request_timeout,
            )
            .await;

            match outcome {
                Ok(bytes) => {
                    // Fold this batch's rate in so later sizes/timeouts track
                    // the current rate. Also what the `bar.inc(1)` loop below
                    // reads for the displayed ETA (via `format_eta`).
                    rate.update(started.elapsed(), batch_tokens);
                    break 'retry bytes;
                }
                Err(EmbedBatchError::BudgetExceeded(e)) if this_batch_size == 1 => {
                    // Can't shrink below 1 chunk. On calibration batch 1 (no
                    // rate estimate yet), escalate patience once
                    // (FIRST_REQUEST_TIMEOUT → MAX_REQUEST_TIMEOUT) before
                    // giving up: a cold single chunk on slow hardware may still
                    // finish given the full budget.
                    if !escalated_calibration_once && rate.per_token().is_none() {
                        escalated_calibration_once = true;
                        eprintln!(
                            "First embed request timed out (server request budget \
                             may be smaller than expected); retrying with more patience\u{2026}"
                        );
                        continue 'retry;
                    }
                    // Don't abort the whole run: prior batches stay committed
                    // and a re-run backfills the rest. Return the count so far —
                    // an `Err` would unwind before `stats()` and discard the
                    // visible progress.
                    report_embed_failure(&bar, embedded, total, &server_url, e);
                    return Ok(embedded);
                }
                Err(EmbedBatchError::BudgetExceeded(e)) => {
                    // Steady-state batch exceeded the server's budget: shrink
                    // (halve, floor 1) and retry rather than discarding progress.
                    let shrunk = (this_batch_size / 2).max(1);
                    if shrunk == this_batch_size {
                        // Already at the floor and still failing (the batch-of-1
                        // branch above handles this; guards against an infinite
                        // loop otherwise).
                        report_embed_failure(&bar, embedded, total, &server_url, e);
                        return Ok(embedded);
                    }
                    tracing::warn!(
                        "index/embed batch of {this_batch_size} chunks exceeded the server's \
                         request budget (408) — shrinking to {shrunk} chunk(s) and retrying: {e:#}",
                    );
                    // Fold in a pessimistic per-token sample (the failed timeout
                    // over the batch's tokens) so future `next_batch_len` calls
                    // don't re-derive the same too-large batch.
                    rate.update(request_timeout, batch_tokens);
                    this_batch_size = shrunk;
                    continue 'retry;
                }
                Err(EmbedBatchError::Other(e)) => {
                    // Any other failure: prior batches stay committed and a
                    // re-run backfills the rest. Report and stop rather than
                    // propagating an `Err` that would discard the visible progress.
                    report_embed_failure(&bar, embedded, total, &server_url, e);
                    return Ok(embedded);
                }
            }
        };

        let dim = spelunk_core::embeddings::EMBEDDING_DIM;
        let stride = dim * 4;
        let batch = &chunk_ids_and_texts[cursor..cursor + this_batch_size];

        // Decode this batch's vectors and commit them in a single transaction
        // (see `Database::insert_embeddings`): one commit per batch instead of
        // one implicit autocommit per row. The whole batch's compute is already
        // sunk by now, so the commit boundary is the batch — an untimely kill
        // rolls the batch back atomically and `chunks_missing_embeddings`
        // re-queues it whole on the next run (ADR-070 D2).
        let embeddings: Vec<(i64, Vec<f32>)> = batch
            .iter()
            .enumerate()
            .map(|(i, (row_id, _text, _token_count))| {
                let vector =
                    spelunk_core::embeddings::blob_to_vec(&bytes[i * stride..(i + 1) * stride]);
                (*row_id, vector)
            })
            .collect();
        db.insert_embeddings(&embeddings)?;

        // The batch is now durable; advance the counters and repaint the ETA
        // per chunk so it still counts down through a batch, not once per request.
        for (_row_id, _text, token_count) in batch.iter() {
            embedded += 1;
            tokens_done += (*token_count).max(1) as u64;
            bar.inc(1);
            let eta_str = format_eta(total_tokens.saturating_sub(tokens_done), rate.per_token());
            let work_pct = pct(tokens_done, total_tokens);
            bar.set_message(format!(
                "{eta_str}  \u{00b7}  {embedded}/{total} chunks embedded \
                 ({work_pct}% of work done)"
            ));
        }

        previous_batch_size = this_batch_size;
        cursor += this_batch_size;
    }

    bar.finish_with_message(format!("{embedded} chunks embedded"));
    Ok(embedded)
}

/// An `embed_one_batch` failure, distinguishing "the request budget was too
/// small for this batch" (408, or a client-side timeout expiring first) from
/// every other failure — only the former is worth shrinking and retrying (see
/// `run_embed_phase`).
enum EmbedBatchError {
    /// Server returned 408, or the client-side `timeout` elapsed first.
    BudgetExceeded(anyhow::Error),
    /// Any other failure (network error, non-408 status, malformed body).
    Other(anyhow::Error),
}

/// Send one embed batch and return the raw little-endian f32 response bytes: one
/// `EMBEDDING_DIM`-float vector per chunk, in request order. Applies a
/// per-request `timeout` (see `batch_timeout`) and validates the response length.
/// Distinguishes a 408/timeout from other failures — see [`EmbedBatchError`].
async fn embed_one_batch(
    client: &reqwest::Client,
    url: &str,
    server_key: Option<&str>,
    body: EmbedRequest,
    batch_len: usize,
    timeout: Duration,
) -> Result<Vec<u8>, EmbedBatchError> {
    let mut req = client.post(url).timeout(timeout).json(&body);
    if let Some(k) = server_key {
        req = req.bearer_auth(k);
    }

    let send_result = req.send().await;
    let resp = match send_result {
        Ok(resp) => resp,
        Err(e) if e.is_timeout() => {
            return Err(EmbedBatchError::BudgetExceeded(
                anyhow::Error::new(e).context(format!(
                    "calling {url} (client-side timeout of {timeout:?} elapsed)"
                )),
            ));
        }
        Err(e) => {
            return Err(EmbedBatchError::Other(
                anyhow::Error::new(e).context(format!("calling {url}")),
            ));
        }
    };

    if resp.status() == reqwest::StatusCode::REQUEST_TIMEOUT {
        return Err(EmbedBatchError::BudgetExceeded(anyhow::anyhow!(
            "server returned 408 Request Timeout for index/embed \
             (batch of {batch_len} chunk(s) exceeded the server's request budget)"
        )));
    }

    let resp = match resp.error_for_status() {
        Ok(resp) => resp,
        Err(e) => {
            return Err(EmbedBatchError::Other(
                anyhow::Error::new(e).context("server returned an error for index/embed"),
            ));
        }
    };

    let bytes = resp
        .bytes()
        .await
        .context("reading index/embed response")
        .map_err(EmbedBatchError::Other)?;

    let dim = spelunk_core::embeddings::EMBEDDING_DIM;
    let stride = dim * 4;
    let expected = batch_len * stride;
    if bytes.len() != expected {
        return Err(EmbedBatchError::Other(anyhow::anyhow!(
            "index/embed returned {} bytes, expected {expected} ({batch_len} × {dim}-dim f32)",
            bytes.len(),
        )));
    }
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_batch_ceiling_passes_through_valid_values() {
        // A user-supplied value within range is used verbatim as the ceiling
        // that calibration may grow the batch size up to.
        assert_eq!(resolve_batch_ceiling(1, None), 1);
        assert_eq!(resolve_batch_ceiling(32, None), 32);
        assert_eq!(resolve_batch_ceiling(64, None), 64);
        assert_eq!(resolve_batch_ceiling(200, None), 200);
        assert_eq!(resolve_batch_ceiling(MAX_BATCH, None), MAX_BATCH);
    }

    #[test]
    fn resolve_batch_ceiling_falls_back_to_default_for_zero() {
        // 0 means the user left `--batch-size` at its default: the ceiling is
        // the server's own hard limit, not some fixed pre-calibration size.
        assert_eq!(resolve_batch_ceiling(0, None), DEFAULT_BATCH_CEILING);
        assert_eq!(DEFAULT_BATCH_CEILING, MAX_BATCH);
    }

    #[test]
    fn resolve_batch_ceiling_clamps_above_server_ceiling() {
        assert_eq!(resolve_batch_ceiling(MAX_BATCH + 1, None), MAX_BATCH);
        assert_eq!(resolve_batch_ceiling(10_000, None), MAX_BATCH);
    }

    #[test]
    fn resolve_batch_ceiling_clamps_to_server_advertised_max() {
        // A server advertising a smaller max_batch_chunks than our MAX_BATCH
        // guess must win — never plan around a count the server won't accept.
        assert_eq!(resolve_batch_ceiling(0, Some(32)), 32);
        assert_eq!(resolve_batch_ceiling(200, Some(32)), 32);
        // A server-advertised max ABOVE the user's/default ceiling doesn't
        // raise it — it's a min(), not a replacement.
        assert_eq!(resolve_batch_ceiling(16, Some(256)), 16);
    }

    // ── resolve_target_batch_seconds: server-limits-aware target clamping ───

    #[test]
    fn resolve_target_batch_seconds_uses_default_when_server_budget_is_generous() {
        // A server advertising the new EMBED_REQUEST_TIMEOUT (1800s) budget
        // comfortably fits the default 240s target — no clamping needed.
        let limits = ServerLimits {
            embed_request_timeout_secs: 1800,
            max_batch_chunks: 256,
            embedder_token_cap: None,
        };
        assert_eq!(
            resolve_target_batch_seconds(Some(limits)),
            TARGET_BATCH_SECONDS
        );
    }

    #[test]
    fn resolve_target_batch_seconds_clamps_down_for_small_server_budget() {
        // A server advertising a smaller budget than the default target
        // forces a smaller target, at SERVER_BUDGET_TARGET_FRACTION of that
        // budget (leaving headroom rather than targeting the hard edge).
        let limits = ServerLimits {
            embed_request_timeout_secs: 60,
            max_batch_chunks: 256,
            embedder_token_cap: None,
        };
        assert_eq!(resolve_target_batch_seconds(Some(limits)), 40); // 60 * 2/3
    }

    #[test]
    fn resolve_target_batch_seconds_assumes_legacy_budget_when_server_limits_absent() {
        // THE version-skew case: a server that pre-dates the `limits` field
        // still enforces the old blanket 30s budget with no /index/embed
        // exemption. Absent `limits` must NOT be read as "no limit" — it must
        // fall back to the conservative legacy assumption.
        assert_eq!(
            resolve_target_batch_seconds(None),
            20 // 30 * 2/3, floored
        );
    }

    // ── next_batch_len: calibration-driven, token-weighted batch sizing ─────
    //
    // A tail of 1-token chunks makes token math equal chunk math, so these
    // first cases pin the same behaviour the old chunk-count sizing had; the
    // token-skew cases after them pin what changed.

    /// A uniform queue tail of 1-token chunks.
    fn unit_tail(n: usize) -> Vec<usize> {
        vec![1; n]
    }

    #[test]
    fn next_batch_len_shrinks_for_slow_hardware() {
        // ~60 s/token over 1-token chunks: a 240 s budget fits ~4 chunks.
        // previous_batch_size=256 so the growth cap doesn't bind here.
        assert_eq!(
            next_batch_len(
                Duration::from_secs(60),
                &unit_tail(256),
                256,
                256,
                TARGET_BATCH_SECONDS
            ),
            4
        );
    }

    #[test]
    fn next_batch_len_grows_for_fast_hardware_but_respects_growth_cap() {
        // ~1 s/token over 1-token chunks: a 240 s budget fits 240 chunks, but
        // growth from a previous batch of 4 is capped to GROWTH_FACTOR (8) × 4.
        assert_eq!(
            next_batch_len(
                Duration::from_secs(1),
                &unit_tail(256),
                256,
                4,
                TARGET_BATCH_SECONDS
            ),
            32
        );
    }

    #[test]
    fn next_batch_len_reaches_budget_once_previous_batch_is_already_large() {
        // Once the previous batch was large enough that GROWTH_FACTOR × it
        // exceeds the ceiling, the token budget (not the growth cap) is the
        // binding constraint, so growth isn't artificially stalled forever.
        assert_eq!(
            next_batch_len(
                Duration::from_secs(1),
                &unit_tail(256),
                256,
                64,
                TARGET_BATCH_SECONDS
            ),
            240 // budget-derived value, below both the 512 growth cap and the 256 ceiling
        );
    }

    #[test]
    fn next_batch_len_clamps_to_ceiling() {
        // A very fast rate would derive a batch above the ceiling; the ceiling
        // wins even when the growth cap would otherwise allow more.
        let t = next_batch_len(
            Duration::from_millis(1),
            &unit_tail(512),
            256,
            256,
            TARGET_BATCH_SECONDS,
        );
        assert_eq!(t, 256);
        let t = next_batch_len(
            Duration::from_millis(1),
            &unit_tail(512),
            32,
            32,
            TARGET_BATCH_SECONDS,
        );
        assert_eq!(t, 32);
    }

    #[test]
    fn next_batch_len_floors_at_one_for_extremely_slow_hardware() {
        // If a single chunk alone blows the whole per-batch budget, we still
        // must send at least one chunk per request.
        let t = next_batch_len(
            Duration::from_secs(10_000),
            &unit_tail(256),
            256,
            4,
            TARGET_BATCH_SECONDS,
        );
        assert_eq!(t, 1);
    }

    #[test]
    fn next_batch_len_handles_zero_duration_without_panicking() {
        // A degenerate zero-duration sample (e.g. a clock quirk) must not
        // divide-by-zero; falls back to the growth cap since the rate is
        // unmeasurably fast (growth is still capped per step even here).
        let t = next_batch_len(
            Duration::ZERO,
            &unit_tail(256),
            256,
            4,
            TARGET_BATCH_SECONDS,
        );
        assert_eq!(t, 32); // growth_cap = 4 * GROWTH_FACTOR(8)
    }

    #[test]
    fn next_batch_len_uses_smaller_clamped_target_when_passed() {
        // A caller passing a smaller target_seconds (e.g. because
        // resolve_target_batch_seconds clamped it down for a small-budget
        // server) must derive a proportionally smaller batch, not always
        // TARGET_BATCH_SECONDS.
        let t = next_batch_len(Duration::from_secs(1), &unit_tail(256), 256, 256, 20);
        assert_eq!(t, 20);
    }

    #[test]
    fn next_batch_len_fills_by_token_sum_not_chunk_count() {
        // 100-token chunks at 1 s/token: the 240 s budget fits 2 whole chunks
        // (300 tokens would overshoot), NOT the 240 chunks a chunk-count
        // budget calibrated on small chunks would have asked for. This is the
        // sizing half of the D6 wasted-GPU defect.
        let tail = vec![100usize; 256];
        let t = next_batch_len(
            Duration::from_secs(1),
            &tail,
            256,
            256,
            TARGET_BATCH_SECONDS,
        );
        assert_eq!(t, 2);
    }

    #[test]
    fn next_batch_len_stops_at_a_size_transition_in_the_queue() {
        // A queue crossing from tiny chunks into huge ones (the measured 7.4x
        // jump) must not fill the batch past the transition: three 1-token
        // chunks fit, and the 1000-token chunk that follows is left for the
        // next batch instead of silently consuming the deadline's margin.
        let mut tail = vec![1usize, 1, 1];
        tail.extend(vec![1000usize; 64]);
        let t = next_batch_len(
            Duration::from_secs(1),
            &tail,
            256,
            256,
            TARGET_BATCH_SECONDS,
        );
        assert_eq!(t, 3);
    }

    #[test]
    fn next_batch_len_zero_token_chunks_are_floored_not_free() {
        // A pre-backfill row can carry token_count 0; it must cost at least 1
        // token so a run of zeros can't derive an unbounded batch.
        let tail = vec![0usize; 512];
        let t = next_batch_len(
            Duration::from_secs(60),
            &tail,
            256,
            256,
            TARGET_BATCH_SECONDS,
        );
        assert_eq!(t, 4); // identical to the 1-token case
    }

    // ── batch_timeout: derive a per-request deadline from the measured rate ──

    #[test]
    fn batch_timeout_scales_with_expected_batch_duration() {
        // At 60 s/token, a 4-token batch is expected to take 240 s; with the
        // 4x safety factor that's 960 s, inside the 1800 s ceiling.
        let t = batch_timeout(Duration::from_secs(60), 4);
        assert_eq!(t, Duration::from_secs(960));
    }

    #[test]
    fn batch_timeout_clamps_to_floor_for_fast_hardware() {
        // At 1 s/token, a 4-token batch is expected to take 4 s; even with the
        // 4x safety factor (16 s) that's far below the floor, which must win
        // so transient latency spikes are still absorbed.
        let t = batch_timeout(Duration::from_secs(1), 4);
        assert_eq!(t, MIN_REQUEST_TIMEOUT);
    }

    #[test]
    fn batch_timeout_clamps_to_ceiling_for_pathologically_slow_rate() {
        let t = batch_timeout(Duration::from_secs(100_000), 256);
        assert_eq!(t, MAX_REQUEST_TIMEOUT);
    }

    #[test]
    fn batch_timeout_never_panics_on_degenerate_inputs() {
        let t = batch_timeout(Duration::ZERO, 0);
        assert!(t >= MIN_REQUEST_TIMEOUT && t <= MAX_REQUEST_TIMEOUT);
    }

    #[test]
    fn batch_timeout_tracks_batch_token_sum_not_chunk_count() {
        // The deadline is derived from the batch's token sum, so two batches
        // of equal chunk count but 10x different token weight get 10x
        // different deadlines. Under chunk-count sizing both would have shared
        // one deadline and the heavy batch would consume its entire safety
        // margin (the D6 field failure).
        let per_token = Duration::from_secs(1);
        let light = batch_timeout(per_token, 100);
        let heavy = batch_timeout(per_token, 1000);
        assert_eq!(light, Duration::from_secs(400));
        assert_eq!(heavy, MAX_REQUEST_TIMEOUT); // 4000s clamped to 1800s
        assert!(heavy > light);
    }

    // ── RateEstimate: continuously re-estimate the per-token rate ───────────

    #[test]
    fn rate_estimate_seeds_from_first_observation() {
        let mut r = RateEstimate::new();
        assert!(r.per_token().is_none());
        r.update(Duration::from_secs(2), 1);
        assert_eq!(r.per_token(), Some(Duration::from_secs(2)));
    }

    #[test]
    fn rate_estimate_deweights_the_batch_1_cold_sample_on_second_observation() {
        // Batch 1: 1 token in 10 s ⇒ 10 s/token (cold). Batch 2 (1 s/token)
        // must dominate: only CALIBRATION_BATCH_1_WEIGHT (0.1) of the cold
        // sample survives, not a 50/50 split.
        let mut r = RateEstimate::new();
        r.update(Duration::from_secs(10), 1);
        r.update(Duration::from_secs(4), 4); // 1 s/token
        let blended = r.per_token().unwrap();
        // Exact expected value: 10*0.1 + 1*0.9 = 1.9s.
        assert!(
            (blended.as_secs_f64() - 1.9).abs() < 1e-9,
            "expected the de-weighted blend 10*0.1 + 1*0.9 = 1.9s, got {blended:?}"
        );
        assert!(
            blended < Duration::from_secs(10),
            "the rate must move toward the newer, faster sample, got {blended:?}"
        );
    }

    #[test]
    fn rate_estimate_third_sample_onward_blends_50_50() {
        // From the third observation onward (batch-1 cold sample already
        // superseded), later samples blend evenly with the running estimate.
        let mut r = RateEstimate::new();
        r.update(Duration::from_secs(10), 1); // batch 1 (cold): 10s/token
        r.update(Duration::from_secs(4), 4); // batch 2: 1s/token -> blended 1.9s/token
        r.update(Duration::from_secs(3), 1); // batch 3: 3s/token -> 50/50 blend with 1.9
        let blended = r.per_token().unwrap();
        let expected = (1.9 + 3.0) / 2.0;
        assert!(
            (blended.as_secs_f64() - expected).abs() < 1e-9,
            "expected a plain 50/50 blend from the third sample onward: {expected}, got {blended:?}"
        );
    }

    #[test]
    fn rate_estimate_reproduces_field_failure_scenario_with_fix() {
        // Batch 1 (1 token) ~25s cold; batch 2 (4 tokens) ~4.8s (~1.2s/token
        // warm). The single shared estimate (de-weighted + growth-capped) must
        // derive a small, internally consistent batch, not the unblended
        // ~50x leap an earlier build produced.
        let mut r = RateEstimate::new();
        r.update(Duration::from_secs(25), 1); // batch 1: cold
        r.update(Duration::from_millis(4800), 4); // batch 2: 1.2s/token warm
        let per_token = r.per_token().unwrap();
        // 25*0.1 + 1.2*0.9 = 3.58s/token.
        assert!(
            (per_token.as_secs_f64() - 3.58).abs() < 1e-9,
            "expected 3.58s/token, got {per_token:?}"
        );
        // The same estimate feeds next_batch_len, growth-capped from the
        // previous batch of 4 (1-token chunks keep token math == chunk math).
        let batch_3_size = next_batch_len(per_token, &unit_tail(256), 256, 4, TARGET_BATCH_SECONDS);
        assert_eq!(
            batch_3_size, 32,
            "growth-capped (GROWTH_FACTOR=8 * previous batch of 4) at 32, not the raw \
             240/3.58≈67 the estimate alone would suggest, and nowhere near the field \
             failure's 200"
        );
        // The resulting batch's expected duration must stay well under ~240s;
        // uses the uncapped TARGET_BATCH_SECONDS so the growth cap alone is under test.
        let expected_duration = per_token.as_secs_f64() * batch_3_size as f64;
        assert!(
            expected_duration < 150.0,
            "batch 3's expected duration ({expected_duration:.1}s) must be far below the \
             field failure's ~240s (200 chunks @ ~1.2s/token)"
        );
    }

    #[test]
    fn rate_estimate_ignores_zero_token_batches() {
        let mut r = RateEstimate::new();
        r.update(Duration::from_secs(1), 0);
        assert!(r.per_token().is_none());
    }

    // ── pct + token-weighted progress: work fraction ≠ chunk fraction ───────

    #[test]
    fn work_fraction_diverges_from_chunk_fraction_on_a_token_skewed_queue() {
        // The D4/D6 estimator defect in miniature: a queue whose late chunks
        // are far heavier than its early ones. After the first two of four
        // chunks land, HALF the chunks are searchable but almost none of the
        // work is done; the two percentages must diverge, and each must be
        // computed over its own denominator.
        let queue: Vec<(i64, String, usize)> = vec![
            (1, "a".into(), 10),
            (2, "b".into(), 10),
            (3, "c".into(), 400),
            (4, "d".into(), 400),
        ];
        let total_tokens: u64 = queue.iter().map(|(_, _, tc)| *tc as u64).sum();
        let tokens_done: u64 = queue[..2].iter().map(|(_, _, tc)| *tc as u64).sum();

        let chunk_pct = pct(2, queue.len() as u64);
        let work_pct = pct(tokens_done, total_tokens);

        assert_eq!(chunk_pct, 50);
        assert_eq!(work_pct, 2); // 20 / 820
        assert_ne!(
            chunk_pct, work_pct,
            "chunk fraction is coverage, token fraction is progress; on a skewed \
             queue they must not coincide"
        );
    }

    #[test]
    fn pct_is_zero_over_an_empty_denominator() {
        assert_eq!(pct(0, 0), 0);
        assert_eq!(pct(5, 0), 0);
    }

    #[test]
    fn eta_is_token_weighted_not_chunk_weighted() {
        // Same rate, same remaining CHUNK count, 100x the remaining tokens:
        // the displayed ETA must scale with tokens. A chunk-weighted ETA would
        // print the same string for both (the 3.2x under-report in the field).
        let per_token = Some(Duration::from_secs(1));
        let light = format_eta(60, per_token);
        let heavy = format_eta(6000, per_token);
        assert_eq!(light, "ETA 1m");
        assert_eq!(heavy, "ETA 1h40m");
    }

    // ── embed_progress_style: the message-only-ETA template must build ──────

    #[test]
    fn embed_progress_style_builds_without_indicatif_eta_token() {
        // A malformed template would panic at the `.unwrap()` the first time the
        // embed phase runs; building and applying it here proves it's well-formed.
        let style = embed_progress_style();
        let bar = ProgressBar::hidden();
        bar.set_style(style);
        // Driving the bar the way the embed phase does must not panic.
        bar.enable_steady_tick(Duration::from_millis(120));
        bar.set_length(10);
        bar.tick();
        bar.set_message(format_eta(9, Some(Duration::from_secs(2))));
        bar.inc(1);
        bar.finish_and_clear();
    }

    // ── format_eta: display ETA derived from the measured RateEstimate ──────

    #[test]
    fn format_eta_shows_calibrating_when_rate_unknown() {
        // Before the first batch has landed there is no measurement to derive
        // an ETA from at all — show a calibrating placeholder, not a guess.
        assert_eq!(format_eta(41, None), "ETA calibrating…");
    }

    #[test]
    fn format_eta_shows_seconds_for_sub_minute_remaining() {
        // 1 s/entry * 12 remaining = 12s.
        assert_eq!(format_eta(12, Some(Duration::from_secs(1))), "ETA 12s");
    }

    #[test]
    fn format_eta_shows_zero_seconds_when_nothing_remains() {
        assert_eq!(format_eta(0, Some(Duration::from_secs(5))), "ETA 0s");
    }

    #[test]
    fn format_eta_shows_minutes_and_seconds() {
        // 2 s/entry * 100 remaining = 200s = 3m20s.
        assert_eq!(format_eta(100, Some(Duration::from_secs(2))), "ETA 3m20s");
    }

    #[test]
    fn format_eta_shows_bare_minutes_when_no_remainder_seconds() {
        // 1 s/entry * 180 remaining = 180s = 3m exactly.
        assert_eq!(format_eta(180, Some(Duration::from_secs(1))), "ETA 3m");
    }

    #[test]
    fn format_eta_shows_hours_and_minutes() {
        // 60 s/entry * 65 remaining = 3900s = 1h05m.
        assert_eq!(format_eta(65, Some(Duration::from_secs(60))), "ETA 1h05m");
    }

    #[test]
    fn format_eta_caps_pathologically_large_duration_instead_of_showing_absurd_value() {
        // A pathological per_entry times a large remaining count must render the
        // capped ">24h" string — never an overflowed, panicking, or years-scale value.
        let eta = format_eta(1_000_000, Some(Duration::from_secs(10_000_000)));
        assert_eq!(eta, "ETA >24h");
        assert!(
            !eta.contains('y'),
            "must never render a years-scale duration like the field-observed 153y bug: {eta}"
        );
    }

    #[test]
    fn format_eta_caps_at_boundary_just_above_24h() {
        // At/above the 24h cap must show the capped string, not a literal
        // "24h00m" — the cap is a hard display ceiling, not just an overflow guard.
        let eta = format_eta(1, Some(Duration::from_secs(24 * 60 * 60 + 1)));
        assert_eq!(eta, "ETA >24h");
    }

    #[test]
    fn format_eta_does_not_panic_on_overflow_prone_inputs() {
        // Duration::MAX times a large remaining count would overflow a naive
        // `Duration * u32`/`Duration::saturating_mul` computation; this must
        // still return the capped string without panicking.
        let eta = format_eta(u64::MAX, Some(Duration::MAX));
        assert_eq!(eta, "ETA >24h");
    }

    // ── run_embed_phase: a mid-run batch failure must not discard earlier,
    //    already-committed embeddings ──────────────────────────────────────────

    use std::sync::OnceLock;

    use crate::capability::{Capabilities, EmbedderState, ServerLimits};
    use spelunk_core::config::Config;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Register the sqlite-vec extension exactly once per test process so the
    /// in-memory DB can create the `vec0` embeddings table.
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

    /// One constant `EMBEDDING_DIM`-vector of little-endian f32 per request
    /// chunk, matching the server's wire format (response[i] → chunk[i]).
    struct OkEmbedResponder;
    impl wiremock::Respond for OkEmbedResponder {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            #[derive(serde::Deserialize)]
            struct ReqBody {
                chunks: Vec<serde_json::Value>,
            }
            let body: ReqBody =
                serde_json::from_slice(&request.body).unwrap_or(ReqBody { chunks: vec![] });
            let dim = spelunk_core::embeddings::EMBEDDING_DIM;
            let mut bytes = Vec::with_capacity(body.chunks.len() * dim * 4);
            for _ in &body.chunks {
                for _ in 0..dim {
                    bytes.extend_from_slice(&0.1f32.to_le_bytes());
                }
            }
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_bytes(bytes)
        }
    }

    /// Insert `n` chunks into a fresh in-memory DB and return it plus their ids.
    fn seed_chunks(n: usize) -> (Database, Vec<i64>) {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open in-memory DB");
        let file_id = db
            .upsert_file("src/lib.rs", Some("rust"), "hash0", 0)
            .unwrap();
        let ids = (0..n)
            .map(|i| {
                db.insert_chunk(
                    file_id,
                    "function",
                    Some(&format!("f{i}")),
                    i,
                    i + 1,
                    &format!("fn f{i}() {{}}"),
                    None,
                    1,
                )
                .unwrap()
            })
            .collect();
        (db, ids)
    }

    fn server_tier(url: String) -> Tier {
        server_tier_with_limits(url, None)
    }

    /// Same as [`server_tier`], but with `server_limits` set — for tests that
    /// exercise the version-skew clamping.
    fn server_tier_with_limits(url: String, server_limits: Option<ServerLimits>) -> Tier {
        Tier::Server {
            url,
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits,
        }
    }

    #[tokio::test]
    async fn batch_failure_keeps_prior_batches_and_stops_gracefully() {
        // 6 chunks, small ceiling; the mock's third response fails with 500.
        // The run must persist every chunk embedded before the failure, NOT
        // error, and report only the successfully-embedded count.
        let mock = MockServer::start().await;
        // The first two requests (calibration: 1 chunk, then up to 4 chunks)
        // succeed; everything after that fails, so the run stops partway
        // through a small index without ever reaching a "finished" state.
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(6);
        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

        let cfg = Config::default();
        let tier = server_tier(mock.uri());
        let mp = MultiProgress::new();

        let embedded = run_embed_phase(
            chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            4, // batch_size ceiling
            &mp,
        )
        .await
        .expect("a failing batch must NOT return Err; it stops gracefully");

        // Calibration sends batch 1 (1 chunk) then batch 2 (up to 4 chunks,
        // clamped to what's left); both succeed here, so exactly
        // 1 + min(4, 5) = 5 chunks land before the third request fails.
        assert_eq!(
            embedded, 5,
            "the two successful calibration batches (1 + 4 chunks) must be reported as embedded"
        );
        assert_eq!(
            db.stats().unwrap().embedding_count,
            5,
            "the 5 embeddings from the successful batches must be persisted in the DB, \
             not rolled back when the next batch failed"
        );
    }

    #[tokio::test]
    async fn all_batches_success_embeds_everything() {
        // Control case: when every batch succeeds, all chunks are embedded and
        // persisted (guards against the failure path over-triggering), across
        // calibration batches and into steady state.
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(50);
        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

        let cfg = Config::default();
        let tier = server_tier(mock.uri());
        let mp = MultiProgress::new();

        let embedded = run_embed_phase(
            chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            8,
            &mp,
        )
        .await
        .expect("all-success run");

        assert_eq!(embedded, 50);
        assert_eq!(db.stats().unwrap().embedding_count, 50);
    }

    #[tokio::test]
    async fn small_index_below_calibration_size_still_embeds_everything() {
        // An index with fewer chunks than even the first calibration batch
        // (or between the two) must not panic on slicing and must still embed
        // every chunk — regression guard for the `.min(left)` clamps.
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .mount(&mock)
            .await;

        for n in [1usize, 2, 3] {
            let (db, ids) = seed_chunks(n);
            let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
                .iter()
                .map(|id| (*id, format!("text {id}"), 3))
                .collect();

            let cfg = Config::default();
            let tier = server_tier(mock.uri());
            let mp = MultiProgress::new();

            let embedded = run_embed_phase(
                chunk_ids_and_texts,
                &db,
                &cfg,
                &tier,
                std::path::Path::new("/tmp/proj"),
                64,
                &mp,
            )
            .await
            .unwrap_or_else(|e| panic!("n={n} must succeed: {e:#}"));

            assert_eq!(embedded, n as u64, "n={n}");
        }
    }

    #[tokio::test]
    async fn empty_queue_returns_immediately_without_any_request() {
        // Nothing to embed (e.g. a re-run where every chunk already has an
        // embedding) must not enter the batch loop at all — a regression
        // guard for the `while cursor < remaining` loop that replaced the old
        // fixed-size `.chunks()` iterator, which handled a zero-length slice
        // for free.
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let (db, _ids) = seed_chunks(0);
        let cfg = Config::default();
        let tier = server_tier(mock.uri());
        let mp = MultiProgress::new();

        let embedded = run_embed_phase(
            Vec::new(),
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            64,
            &mp,
        )
        .await
        .expect("an empty queue must succeed trivially");

        assert_eq!(embedded, 0);
    }

    // ── 408/timeout retry-then-shrink behaviour ────────────────────────────

    #[tokio::test]
    async fn calibration_batch_1_408_is_retried_and_succeeds() {
        // The first request (calibration batch of 1) 408s once, then succeeds
        // on retry — must not be fatal at 0/total embedded.
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(408))
            .up_to_n_times(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(3);
        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

        let cfg = Config::default();
        let tier = server_tier(mock.uri());
        let mp = MultiProgress::new();

        let embedded = run_embed_phase(
            chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            64,
            &mp,
        )
        .await
        .expect("a single 408 on calibration batch 1 must be retried, not fatal");

        assert_eq!(
            embedded, 3,
            "all chunks must be embedded once the retried calibration request succeeds"
        );
        assert_eq!(db.stats().unwrap().embedding_count, 3);
    }

    #[tokio::test]
    async fn calibration_batch_1_408_twice_gives_up_gracefully() {
        // If the retried calibration request ALSO 408s, the phase must still
        // return Ok(0) (not Err) — the caller (`run_embed_phases`/`index()`)
        // depends on this to still print stats and exit cleanly rather than
        // unwinding via `?` before `db.stats()` runs.
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(408))
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(3);
        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

        let cfg = Config::default();
        let tier = server_tier(mock.uri());
        let mp = MultiProgress::new();

        let embedded = run_embed_phase(
            chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            64,
            &mp,
        )
        .await
        .expect("must return Ok(embedded), never Err, even after exhausting the retry");

        assert_eq!(embedded, 0, "nothing embedded when both attempts 408");
        assert_eq!(db.stats().unwrap().embedding_count, 0);
    }

    #[tokio::test]
    async fn steady_state_408_shrinks_batch_and_retries_instead_of_aborting() {
        // A steady-state (post-calibration) batch that 408s must shrink and
        // retry rather than discarding all subsequent progress. Set up: 20
        // chunks, a large `--batch-size` so calibration ramps toward a big
        // batch quickly, and the mock 408s on any request >4 chunks — forcing
        // the shrink-and-retry path to run at least once, ending with
        // everything eventually embedded.
        let mock = MockServer::start().await;

        struct ShrinkUntilSmallResponder;
        impl wiremock::Respond for ShrinkUntilSmallResponder {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                #[derive(serde::Deserialize)]
                struct ReqBody {
                    chunks: Vec<serde_json::Value>,
                }
                let body: ReqBody =
                    serde_json::from_slice(&request.body).unwrap_or(ReqBody { chunks: vec![] });
                if body.chunks.len() > 4 {
                    return ResponseTemplate::new(408);
                }
                let dim = spelunk_core::embeddings::EMBEDDING_DIM;
                let mut bytes = Vec::with_capacity(body.chunks.len() * dim * 4);
                for _ in &body.chunks {
                    for _ in 0..dim {
                        bytes.extend_from_slice(&0.1f32.to_le_bytes());
                    }
                }
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/octet-stream")
                    .set_body_bytes(bytes)
            }
        }

        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ShrinkUntilSmallResponder)
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(20);
        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

        let cfg = Config::default();
        let tier = server_tier(mock.uri());
        let mp = MultiProgress::new();

        let embedded = run_embed_phase(
            chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            64, // ceiling well above the mock's 4-chunk cliff
            &mp,
        )
        .await
        .expect("steady-state 408s must shrink and retry, not abort");

        assert_eq!(
            embedded, 20,
            "every chunk must eventually be embedded once the batch size shrinks below \
             the mock's 4-chunk cliff"
        );
        assert_eq!(db.stats().unwrap().embedding_count, 20);
    }

    #[tokio::test]
    async fn server_advertised_limits_clamp_batch_size_below_default_ceiling() {
        // A server whose /v1/health advertises a small max_batch_chunks must
        // have that respected even when the user's --batch-size (here: 0,
        // i.e. "use the default") would otherwise allow much larger batches.
        // We prove this indirectly: mount a mock that 413s any batch above
        // the advertised limit, and confirm the run still succeeds (i.e. the
        // client never actually sent an oversized batch).
        let mock = MockServer::start().await;

        struct RejectAboveLimitResponder {
            limit: usize,
        }
        impl wiremock::Respond for RejectAboveLimitResponder {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                #[derive(serde::Deserialize)]
                struct ReqBody {
                    chunks: Vec<serde_json::Value>,
                }
                let body: ReqBody =
                    serde_json::from_slice(&request.body).unwrap_or(ReqBody { chunks: vec![] });
                if body.chunks.len() > self.limit {
                    return ResponseTemplate::new(413);
                }
                let dim = spelunk_core::embeddings::EMBEDDING_DIM;
                let mut bytes = Vec::with_capacity(body.chunks.len() * dim * 4);
                for _ in &body.chunks {
                    for _ in 0..dim {
                        bytes.extend_from_slice(&0.1f32.to_le_bytes());
                    }
                }
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/octet-stream")
                    .set_body_bytes(bytes)
            }
        }

        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(RejectAboveLimitResponder { limit: 8 })
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(30);
        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

        let cfg = Config::default();
        let limits = ServerLimits {
            embed_request_timeout_secs: 1800,
            max_batch_chunks: 8,
            embedder_token_cap: None,
        };
        let tier = server_tier_with_limits(mock.uri(), Some(limits));
        let mp = MultiProgress::new();

        let embedded = run_embed_phase(
            chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            0, // user did not set --batch-size: default ceiling would be MAX_BATCH (256)
            &mp,
        )
        .await
        .expect("batches must stay within the server-advertised max_batch_chunks");

        assert_eq!(
            embedded, 30,
            "every chunk must embed successfully — a 413 here would mean the client sent \
             a batch larger than the server-advertised max_batch_chunks"
        );
        assert_eq!(db.stats().unwrap().embedding_count, 30);
    }

    // ── resume after an interrupted run (ADR-070 D2: per-batch granularity) ──

    #[tokio::test]
    async fn resume_after_interrupted_run_reembeds_the_missing_queue_without_dupes() {
        // The resume story end-to-end. A run stops partway with one batch never
        // committed (per-batch transaction: it landed nothing). A re-run
        // rebuilds the queue from `chunks_missing_embeddings` and embeds exactly
        // the remainder — every chunk ends embedded once, none skipped, none
        // duplicated. This relies on `chunks_missing_embeddings` never
        // re-sending a chunk_id that already has an embedding row, not on
        // `INSERT OR REPLACE` actually replacing one — see
        // `storage::db::tests::insert_embedding_single_row_path_does_not_actually_replace_a_repeated_chunk_id`
        // (spelunk-core) for why that distinction matters: OR REPLACE against
        // the `embeddings` vec0 table does not work today, so idempotency here
        // depends entirely on the queue never producing a same-key collision.
        let (db, ids) = seed_chunks(6);
        let cfg = Config::default();
        let mp = MultiProgress::new();

        // ── Run 1: the two calibration batches (1 + 4 chunks) succeed, the
        //    next request 500s, so the run stops with 5 of 6 embedded. ──
        let mock1 = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .up_to_n_times(2)
            .mount(&mock1)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock1)
            .await;

        let queue1: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();
        let embedded1 = run_embed_phase(
            queue1,
            &db,
            &cfg,
            &server_tier(mock1.uri()),
            std::path::Path::new("/tmp/proj"),
            4,
            &mp,
        )
        .await
        .expect("run 1 stops gracefully, not Err");
        assert_eq!(
            embedded1, 5,
            "the 1+4 calibration batches commit; the 500'd batch commits nothing"
        );
        assert_eq!(db.stats().unwrap().embedding_count, 5);

        // ── Rebuild the queue exactly as a re-run does: the interrupted batch
        //    left no partial rows, so exactly the one un-embedded chunk is
        //    re-queued. ──
        let missing = db.chunks_missing_embeddings().unwrap();
        assert_eq!(
            missing.len(),
            1,
            "the interrupted batch committed nothing, so exactly the unembedded chunk remains"
        );
        let queue2: Vec<(i64, String, usize)> = missing
            .iter()
            .map(|(id, _name, _meta, _summary, content, tc)| (*id, content.clone(), *tc))
            .collect();

        // ── Run 2: everything succeeds; only the missing chunk is embedded. ──
        let mock2 = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .mount(&mock2)
            .await;
        let embedded2 = run_embed_phase(
            queue2,
            &db,
            &cfg,
            &server_tier(mock2.uri()),
            std::path::Path::new("/tmp/proj"),
            4,
            &mp,
        )
        .await
        .expect("run 2 backfills the remainder");
        assert_eq!(
            embedded2, 1,
            "only the one missing chunk is embedded on the re-run"
        );
        assert_eq!(
            db.stats().unwrap().embedding_count,
            6,
            "all six chunks embedded exactly once — no duplicate row, no lost chunk"
        );
    }
}
