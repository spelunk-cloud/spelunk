//! Phase-runner entry points for `spelunk index`, plus the embedder-readiness
//! wait and the notices printed when embedding is skipped.
//!
//! `run_phases_3_to_5` (graph rank, summaries, convention extraction) is
//! shared between the inline foreground path and `run_background_phases`
//! (the `--_background-phases` child). `run_embed_phases` is the entry point
//! for the `--_embed-phases` child: it rebuilds the embed queue from the DB,
//! waits for the embedder via `wait_for_embedder`, then runs phases 3–5 too.

use anyhow::Result;
use indicatif::MultiProgress;

use super::IndexArgs;
use crate::cli::cmd::embed_worker::EmbedWorkerGuard;
use crate::{capability, config::Config, registry::Registry, storage::Database};

use super::{embed_phase, parse_phase, summaries};

/// First delay of the embed worker's readiness-wait backoff.
const EMBED_WAIT_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);
/// Backoff growth is bounded at this interval; the wait itself is not
/// time-bounded while the embedder reports `loading` (a model download can
/// legitimately take many minutes, and the queue is durable).
const EMBED_WAIT_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);
/// Consecutive offline probes tolerated before the worker concludes the server
/// is gone (crashed after spawning us) rather than momentarily unreachable.
const EMBED_WAIT_MAX_OFFLINE_PROBES: u32 = 10;

/// Wait until the server's embedder can serve, polling `/v1/health` with a
/// bounded backoff. Returns the final observed tier; the caller re-derives
/// `index_embed` from it.
///
/// A not-ready embedder is a transient condition to wait on, not a terminal
/// condition to skip: `ensure_server_running` waits for liveness only (health
/// goes live at socket bind, before the model loads), so a fresh machine
/// reaches the worker with the embedder still `loading`. Only `unavailable`
/// and `disabled` (or a server with no embedder at all) are terminal; each
/// keeps its distinct notice via `eprint_embed_skipped_notice`. `loading` is
/// never a reason to abandon durable queued work.
///
/// `get_inference_tier_fresh` (not `probe_tier_fresh`): local_first always
/// prefers the local loopback embedder, even with an explicit server_url set
/// (2026-07-23 founder decision), and this poller must keep re-observing
/// that same local-vs-remote routing decision on every iteration rather than
/// freezing on `get_tier`'s cached first probe of an unrelated server_url.
async fn wait_for_embedder(
    cfg: &Config,
    initial_backoff: std::time::Duration,
    max_backoff: std::time::Duration,
) -> capability::Tier {
    let mut backoff = initial_backoff;
    let mut offline_probes = 0u32;
    let mut announced = false;
    loop {
        let tier = capability::get_inference_tier_fresh(cfg).await;
        match &tier {
            capability::Tier::Server { .. } => {
                if matches!(tier.caps(), Some(c) if c.index_embed) {
                    return tier;
                }
                if !matches!(
                    tier.embedder_state(),
                    Some(capability::EmbedderState::Loading)
                ) {
                    // unavailable / disabled / no embedder: terminal here.
                    return tier;
                }
                offline_probes = 0;
                if !announced {
                    eprintln!("Waiting for the embedder to finish loading\u{2026}");
                    announced = true;
                }
            }
            capability::Tier::Offline => {
                offline_probes += 1;
                if offline_probes >= EMBED_WAIT_MAX_OFFLINE_PROBES {
                    return tier;
                }
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

/// Embed-only entry point for the detached `--_embed-phases` subprocess: rebuild
/// the embed queue from the chunks already in the DB (no re-parse), wait for
/// the embedder to become ready, run the embed phase, then phases 3–5.
pub(super) async fn run_embed_phases(
    args: &IndexArgs,
    cfg: &Config,
    db: &Database,
    project_root: &std::path::Path,
    root_canonical: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<()> {
    // Liveness marker for `spelunk status` (dropped on exit; a killed worker
    // leaves it behind for status to classify as a dead pid). Held through the
    // readiness wait too: a worker waiting on a loading embedder is running,
    // and status must not advise a resume that would double it up.
    let worker_guard = EmbedWorkerGuard::acquire(db, db_path);

    let tier = wait_for_embedder(cfg, EMBED_WAIT_INITIAL_BACKOFF, EMBED_WAIT_MAX_BACKOFF).await;
    let embed_ready = matches!(tier.caps(), Some(c) if c.index_embed);
    if tier.is_server() && embed_ready {
        let chunk_ids_and_texts = parse_phase::missing_embedding_texts(db)?;
        if !chunk_ids_and_texts.is_empty() {
            let mp = MultiProgress::new();
            embed_phase::run_embed_phase(
                chunk_ids_and_texts,
                db,
                cfg,
                &tier,
                project_root,
                args.batch_size,
                &mp,
            )
            .await?;
        }
    } else {
        eprint_embed_skipped_notice(&tier, cfg);
    }
    drop(worker_guard);

    run_phases_3_to_5(args, cfg, db, root_canonical, db_path).await
}

/// Build the differentiated notice lines shown when the embedding phase is
/// skipped, so an unembedded index is never a silent surprise. Pure so it can
/// be unit-tested; the four cases mirror the server's readiness contract.
/// `server_url` is `cfg.server_url` (used only for the offline case).
///
/// `remote_url` is `Some` when the probed server came from an explicit
/// `server_url` (not loopback auto-discovery). The unavailable-embedder
/// notice must then name that server instead of pointing at `spelunk server
/// logs`, which only reads the local auto-daemon's log and would show clean
/// logs for a failure that lives on the remote server.
///
/// `is_windows` gates the Windows Defender Firewall hint in the offline
/// case: that hint is a real cause only on Windows, and printing it on every
/// platform actively misdirects a macOS/Linux user away from the real
/// problem (an unreachable configured `server_url`). Callers pass
/// `cfg!(windows)`; injected here so the platform-specific behaviour is
/// testable without `#[cfg(windows)]` test gating.
fn embed_skipped_lines(
    embedder_state: Option<capability::EmbedderState>,
    server_url: Option<&str>,
    remote_url: Option<&str>,
    is_windows: bool,
) -> Vec<String> {
    use capability::EmbedderState;
    match embedder_state {
        Some(EmbedderState::Loading) => vec![
            "Note: the embedder is still warming up — chunks indexed for text/ast-grep search."
                .to_string(),
            "Re-run `spelunk index` in a moment to add embeddings (check `spelunk server status`)."
                .to_string(),
        ],
        Some(EmbedderState::Unavailable) => match remote_url {
            Some(url) => vec![
                format!(
                    "Warning: the embedder failed to load on team server {url}; chunks indexed \
                     for text/ast-grep search only."
                ),
                "Check that server's own logs for the load error, then re-run `spelunk index`."
                    .to_string(),
            ],
            None => vec![
                "Warning: the embedder failed to load; chunks indexed for text/ast-grep search \
                 only."
                    .to_string(),
                "See `spelunk server logs` for the load error, then re-run `spelunk index`."
                    .to_string(),
            ],
        },
        // Reachable server without a ready embedder for any other reason
        // (`disabled`, or an older server that never advertised `index.embed`).
        Some(_) => vec![
            "Note: this server has no embedder — chunks indexed for text/ast-grep search only."
                .to_string(),
        ],
        // Offline: no server reachable. Reaching this arm with `server_url`
        // set means the probe took the explicit-URL path (see
        // `capability::probe::probe`): an auto-discovered loopback miss never
        // carries a `server_url`, so the message can unconditionally say
        // "configured server_url" rather than guessing.
        None => {
            if let Some(url) = server_url {
                let mut lines = vec![format!(
                    "Warning: server_url is explicitly configured to {url}, which is \
                     unreachable, so the embedding phase is skipped. This overrides the \
                     auto-discovered local server, so a healthy `spelunk server start` \
                     daemon elsewhere will not be used while server_url is set."
                )];
                if is_windows {
                    lines.push(
                        "On Windows, allow the loopback listener through Defender Firewall \
                         (accept the prompt on `spelunk server start`)."
                            .to_string(),
                    );
                }
                lines.push(
                    "Chunks are indexed for text/ast-grep search. Re-run `spelunk index` once \
                     the server is reachable to add embeddings."
                        .to_string(),
                );
                lines
            } else {
                vec![
                    "Note: start a local server (`spelunk server start`) to enable semantic search."
                        .to_string(),
                ]
            }
        }
    }
}

/// Print the embed-skipped notice to stderr.
pub(super) fn eprint_embed_skipped_notice(tier: &capability::Tier, cfg: &Config) {
    for line in embed_skipped_lines(
        tier.embedder_state(),
        cfg.server_url.as_deref(),
        tier.explicit_remote_url(),
        cfg!(windows),
    ) {
        eprintln!("{line}");
    }
}

// ── Phases 3–5 (shared between inline and background-phases mode) ─────────────

pub(super) async fn run_phases_3_to_5(
    args: &IndexArgs,
    cfg: &Config,
    db: &Database,
    root_canonical: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<()> {
    // Phase 3: PageRank
    eprintln!("Computing graph rank…");
    let edges = db.graph_edges_all()?;
    if !edges.is_empty() {
        let pr_scores = crate::indexer::pagerank::compute_pagerank(&edges, 20, 0.85);
        let named_chunks = db.chunks_with_names()?;
        let updates: Vec<(i64, f32)> = named_chunks
            .into_iter()
            .filter_map(|(id, name)| name.and_then(|n| pr_scores.get(&n).copied().map(|s| (id, s))))
            .collect();
        if !updates.is_empty() {
            db.update_graph_ranks(&updates)?;
        }
    }

    // Phase 4: LLM summaries. Must finish before the process exits: an in-flight
    // summary is silently lost. Backgrounding here is process-level
    // (--detach, --detach-embed, the phases-3-5 spawn), never a thread.
    if let Err(e) = summaries::generate_summaries(
        args.no_summaries,
        args.summary_batch_size,
        cfg,
        db,
        root_canonical,
    )
    .await
    {
        eprintln!("Warning: summary generation failed: {e:#}");
    }

    // Phase 5: convention extraction (heuristic, no LLM).
    eprintln!("Extracting conventions\u{2026}");
    match crate::conventions::run_extraction(db) {
        Ok(records) => {
            if !records.is_empty() {
                eprintln!("Conventions: {} record(s) detected.", records.len());
            }
        }
        Err(e) => tracing::warn!("convention extraction failed (non-fatal): {e}"),
    }

    // Register / update this project in the global registry.
    if let Ok(reg) = Registry::open() {
        let db_canonical = spelunk_core::utils::canonicalize(db_path);
        if let Err(e) = reg.register(root_canonical, &db_canonical) {
            tracing::warn!("registry update failed: {e}");
        }
    }
    Ok(())
}

pub(super) async fn run_background_phases(
    args: &IndexArgs,
    cfg: &Config,
    db: &Database,
    root_canonical: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<()> {
    run_phases_3_to_5(args, cfg, db, root_canonical, db_path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── embed_skipped_lines: 0-chunks / offline notice (#5) ─────────────────────

    #[test]
    fn embed_skipped_loading_advises_retry() {
        let lines =
            embed_skipped_lines(Some(capability::EmbedderState::Loading), None, None, false);
        assert!(!lines.is_empty(), "notice must not be silent");
        let joined = lines.join("\n");
        assert!(joined.contains("warming up"));
        assert!(joined.contains("Re-run `spelunk index`"));
    }

    #[test]
    fn embed_skipped_unavailable_loopback_points_at_logs() {
        // Loopback auto-discovery: the failing embedder IS the local daemon,
        // so `spelunk server logs` is the right place to look.
        let lines = embed_skipped_lines(
            Some(capability::EmbedderState::Unavailable),
            None,
            None,
            false,
        );
        let joined = lines.join("\n");
        assert!(joined.contains("failed to load"));
        assert!(joined.contains("spelunk server logs"));
    }

    #[test]
    fn embed_skipped_unavailable_remote_names_that_server_never_local_logs() {
        // Explicit server_url: `spelunk server logs` reads the LOCAL daemon's
        // log, which is clean when the failure lives on the team server. The
        // notice must name the probed server instead.
        let lines = embed_skipped_lines(
            Some(capability::EmbedderState::Unavailable),
            None,
            Some("https://team.example:7777"),
            false,
        );
        let joined = lines.join("\n");
        assert!(joined.contains("failed to load"));
        assert!(
            joined.contains("https://team.example:7777"),
            "got: {joined}"
        );
        assert!(
            !joined.contains("spelunk server logs"),
            "must not point a remote failure at local logs: {joined}"
        );
    }

    #[test]
    fn embed_skipped_unreachable_server_names_configured_server_url() {
        // Offline (no reachable server) with a configured server_url: the notice
        // must name the actual URL attempted AND say explicitly that it came
        // from a configured `server_url` (not the auto-discovered loopback
        // daemon). Without this, a user with a healthy loopback daemon running
        // has no path from the message to the real cause: the daemon was
        // never being used because server_url overrides it.
        let lines = embed_skipped_lines(None, Some("http://127.0.0.1:7777"), None, false);
        let joined = lines.join("\n");
        assert!(joined.contains("http://127.0.0.1:7777"), "got: {joined}");
        assert!(joined.contains("unreachable"), "got: {joined}");
        assert!(joined.contains("server_url"), "got: {joined}");
        assert!(
            joined.contains("configured"),
            "must say the target came from a *configured* server_url, not just name \
             `server_url` in passing (this is the specific wording the defect asked for, \
             distinguishing it from the auto-discovered daemon): got: {joined}"
        );
        assert!(
            joined.contains("overrides") || joined.contains("override"),
            "must explain that an explicit server_url overrides the auto-discovered \
             local daemon, so a healthy daemon elsewhere is not the fix: got: {joined}"
        );
    }

    #[test]
    fn embed_skipped_unreachable_server_shows_firewall_hint_only_on_windows() {
        // The Windows Defender Firewall hint is a real cause ONLY on Windows;
        // printing it unconditionally (the field bug, hit on macOS) actively
        // misdirects a user on any other platform.
        let windows_lines = embed_skipped_lines(None, Some("http://127.0.0.1:7777"), None, true);
        assert!(
            windows_lines.join("\n").contains("Firewall"),
            "the Windows hint must still show when the host platform is Windows"
        );

        let non_windows_lines =
            embed_skipped_lines(None, Some("http://127.0.0.1:7777"), None, false);
        assert!(
            !non_windows_lines.join("\n").contains("Firewall"),
            "the Windows-only hint must not print on a non-Windows host: got: {:?}",
            non_windows_lines
        );
    }

    #[test]
    fn embed_skipped_no_server_suggests_starting_one() {
        let lines = embed_skipped_lines(None, None, None, false);
        let joined = lines.join("\n");
        assert!(joined.contains("spelunk server start"));
    }

    // ── wait_for_embedder: the worker owns the readiness wait (ADR-070 D2) ────

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// `/v1/health` body for an embedder in `state`. `index.embed` is
    /// advertised only when ready, mirroring the real server's contract.
    fn health_body(state: &str) -> serde_json::Value {
        let (caps, dim) = if state == "ready" {
            (
                vec!["memory", "index.embed", "search.semantic"],
                spelunk_core::embeddings::EMBEDDING_DIM,
            )
        } else {
            (vec!["memory"], 0)
        };
        serde_json::json!({
            "status": "ok",
            "version": "0.9.3",
            "capabilities": caps,
            "instance_id": "00000000-0000-0000-0000-000000000001",
            "embedding_dim": dim,
            "embedder": { "state": state, "detail": null }
        })
    }

    // `mode = "cloud_first"`: every test below drives the wait loop's polling
    // logic (loading/ready/unavailable/disabled transitions, the offline
    // give-up bound) by mocking `/v1/health` directly at `url` and expecting
    // `wait_for_embedder` to probe exactly that URL. Under the default
    // `local_first` mode, `get_inference_tier_fresh` routes inference to the
    // local loopback embedder instead and never touches `server_url` at all
    // (see `wait_for_embedder_local_first_routes_loopback_transition_not_server_url`
    // below for that path); `cloud_first` is the mode where an explicit
    // `server_url` legitimately serves inference, which is what every test
    // here needs to still be exercising the polling logic against `url`.
    fn cfg_for(url: String) -> Config {
        Config {
            server_url: Some(url),
            project_id: Some("local/test".to_string()),
            mode: Some(crate::config::SyncMode::CloudFirst),
            ..Default::default()
        }
    }

    const TEST_BACKOFF: std::time::Duration = std::time::Duration::from_millis(1);

    #[tokio::test]
    async fn wait_for_embedder_outlasts_a_loading_embedder() {
        // The readiness gate the cold-start bug lives behind: health reports
        // `loading` (twice here) before flipping to `ready`. The wait must
        // keep polling through `loading` and come back with `index.embed`.
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("loading")))
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("ready")))
            .mount(&mock)
            .await;

        let tier = wait_for_embedder(&cfg_for(mock.uri()), TEST_BACKOFF, TEST_BACKOFF).await;
        assert!(
            matches!(tier.caps(), Some(c) if c.index_embed),
            "the wait must return only once the embedder serves; got {tier:?}"
        );
        assert_eq!(
            tier.embedder_state(),
            Some(capability::EmbedderState::Ready)
        );
    }

    #[tokio::test]
    async fn wait_for_embedder_treats_unavailable_as_terminal() {
        // A failed model load is terminal for this server process: return at
        // the first probe (no retries burned) and preserve the state so the
        // caller prints the distinct `unavailable` notice.
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("unavailable")))
            .expect(1)
            .mount(&mock)
            .await;

        let tier = wait_for_embedder(&cfg_for(mock.uri()), TEST_BACKOFF, TEST_BACKOFF).await;
        assert_eq!(
            tier.embedder_state(),
            Some(capability::EmbedderState::Unavailable)
        );
        assert!(!matches!(tier.caps(), Some(c) if c.index_embed));
    }

    #[tokio::test]
    async fn wait_for_embedder_treats_disabled_as_terminal() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("disabled")))
            .expect(1)
            .mount(&mock)
            .await;

        let tier = wait_for_embedder(&cfg_for(mock.uri()), TEST_BACKOFF, TEST_BACKOFF).await;
        assert_eq!(
            tier.embedder_state(),
            Some(capability::EmbedderState::Disabled)
        );
    }

    #[tokio::test]
    async fn wait_for_embedder_loading_then_unavailable_is_terminal() {
        // The embedder can flip loading -> unavailable mid-wait (model load
        // fails after the worker started polling). The wait must exit at the
        // transition with the terminal state preserved, so the caller prints
        // the distinct `unavailable` notice; it must not keep polling.
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("loading")))
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("unavailable")))
            .mount(&mock)
            .await;

        let tier = wait_for_embedder(&cfg_for(mock.uri()), TEST_BACKOFF, TEST_BACKOFF).await;
        assert_eq!(
            tier.embedder_state(),
            Some(capability::EmbedderState::Unavailable),
            "the terminal state observed mid-wait must be returned as-is"
        );
        assert!(!matches!(tier.caps(), Some(c) if c.index_embed));
    }

    #[tokio::test]
    async fn wait_for_embedder_offline_counter_resets_on_a_reachable_probe() {
        // The give-up counter is CONSECUTIVE offline probes, not cumulative: a
        // server that flaps (down, briefly back while loading, down again)
        // must not have its earlier misses counted against the later ones.
        // 7 offline + 1 loading + 7 offline = 14 cumulative misses, but never
        // 10 in a row, so the wait must survive to the final `ready`.
        // (A non-2xx health response probes as Tier::Offline.)
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(7)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("loading")))
            .up_to_n_times(1)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(7)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("ready")))
            .mount(&mock)
            .await;

        let tier = wait_for_embedder(&cfg_for(mock.uri()), TEST_BACKOFF, TEST_BACKOFF).await;
        assert!(
            matches!(tier.caps(), Some(c) if c.index_embed),
            "14 cumulative but never {EMBED_WAIT_MAX_OFFLINE_PROBES} consecutive offline \
             probes must not trip the give-up; got {tier:?}"
        );
    }

    #[tokio::test]
    async fn wait_for_embedder_gives_up_after_bounded_offline_probes() {
        // A vanished server (crashed after spawning the worker) must not hang
        // the worker forever: bounded consecutive offline probes, then return
        // Offline so the skip notice prints and the durable queue stays put.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let dead_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        drop(listener); // port is real but nothing serves it

        let started = std::time::Instant::now();
        let tier = wait_for_embedder(&cfg_for(dead_url), TEST_BACKOFF, TEST_BACKOFF).await;
        assert!(matches!(tier, capability::Tier::Offline));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the offline give-up must be bounded"
        );
    }

    // ── wait_for_embedder backoff/give-up constants ──────────────────────────
    //
    // Every wait_for_embedder test above drives the function with
    // `TEST_BACKOFF` (1ms) so the suite doesn't take the ~150s the real
    // constants would need to reach the give-up bound. That substitution is
    // only faithful to production if the constants it stands in for keep
    // their documented values; pin them here so a silent edit (e.g. raising
    // `EMBED_WAIT_MAX_OFFLINE_PROBES` past what the give-up test's runtime
    // budget assumes) fails loudly instead of just changing real-world
    // worker wait time unnoticed. Mirrors the `loopback_probe_timeout_is_250ms`
    // -style constant pins in `capability/probe.rs`.
    #[test]
    fn embed_wait_initial_backoff_is_1s() {
        assert_eq!(EMBED_WAIT_INITIAL_BACKOFF.as_secs(), 1);
    }

    #[test]
    fn embed_wait_max_backoff_is_30s() {
        assert_eq!(EMBED_WAIT_MAX_BACKOFF.as_secs(), 30);
    }

    #[test]
    fn embed_wait_max_offline_probes_is_10() {
        assert_eq!(EMBED_WAIT_MAX_OFFLINE_PROBES, 10);
    }

    // ── wait_for_embedder: local_first routes to loopback, not server_url ────
    //
    // The routing-bug regression this story fixes: before, the wait loop
    // probed `cfg.server_url` directly (`probe_tier_fresh`) regardless of
    // mode, so a `local_first` project with an explicit `server_url` never
    // reached its local embedder from the detached worker either.

    #[tokio::test]
    #[serial_test::serial(spelunk_no_server_env, server_state_dir_env)]
    async fn wait_for_embedder_local_first_routes_loopback_transition_not_server_url() {
        // Under `local_first` (the default once `server_url` is set, with no
        // explicit `mode`), the wait loop must poll the LOCAL loopback
        // embedder, never the configured `server_url` — even while observing
        // a loading -> ready transition across several polls. `server_url` is
        // deliberately unroutable, so an accidental fallback to it surfaces
        // as a connection/DNS error, not a silent wrong-but-passing result.
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };

        let loopback = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("loading")))
            .up_to_n_times(2)
            .mount(&loopback)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("ready")))
            .mount(&loopback)
            .await;

        let loopback_port: u16 = loopback
            .uri()
            .rsplit(':')
            .next()
            .expect("uri has a port")
            .trim_end_matches('/')
            .parse()
            .expect("uri port is numeric");

        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("server.port"), format!("{loopback_port}\n")).unwrap();

        let prev_state_dir = std::env::var_os("SPELUNK_STATE_DIR");
        // SAFETY: serialised via #[serial(server_state_dir_env)] against
        // every other test touching this var.
        unsafe { std::env::set_var("SPELUNK_STATE_DIR", &state_dir) };

        let cfg = Config {
            server_url: Some("https://cloud.invalid.example:1".to_string()),
            project_id: Some("local/test".to_string()),
            mode: None, // defaults to local_first because server_url is set
            ..Default::default()
        };
        assert_eq!(cfg.resolve_mode(), crate::config::SyncMode::LocalFirst);

        let tier = wait_for_embedder(&cfg, TEST_BACKOFF, TEST_BACKOFF).await;

        unsafe {
            match prev_state_dir {
                Some(v) => std::env::set_var("SPELUNK_STATE_DIR", v),
                None => std::env::remove_var("SPELUNK_STATE_DIR"),
            }
        }

        assert!(
            matches!(tier.caps(), Some(c) if c.index_embed),
            "the wait must observe the loopback's loading -> ready transition; got {tier:?}"
        );
        assert_eq!(
            tier.server_url(),
            Some(format!("http://127.0.0.1:{loopback_port}")).as_deref(),
            "local_first must route the wait loop to the loopback server, not the \
             configured (and unreachable) server_url; got {tier:?}"
        );
    }

    #[test]
    fn embed_skipped_is_never_silent() {
        for state in [
            Some(capability::EmbedderState::Loading),
            Some(capability::EmbedderState::Unavailable),
            Some(capability::EmbedderState::Disabled),
            Some(capability::EmbedderState::Unknown),
            None,
        ] {
            for url in [Some("http://x:1"), None] {
                for remote_url in [None, Some("https://team.example:7777")] {
                    for is_windows in [false, true] {
                        assert!(
                            !embed_skipped_lines(state, url, remote_url, is_windows).is_empty(),
                            "state {state:?} url {url:?} remote_url {remote_url:?} \
                             is_windows {is_windows} produced no notice"
                        );
                    }
                }
            }
        }
    }
}
