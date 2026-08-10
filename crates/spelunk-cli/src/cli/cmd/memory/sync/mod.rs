//! `spelunk sync` and `spelunk memory pull` — two-way local↔cloud memory sync.
//!
//! - `pull`: delta-pull from `GET /memory/since?since_id=<cursor>` and apply
//!   locally. The cursor is the max cloud `remote_id` already synced
//!   (`MAX(remote_id)` over local notes), a UUIDv7 — not a wall-clock watermark
//!   (decision #183), so it is immune to local↔remote clock drift.
//! - `sync`: push local rows `WHERE remote_id IS NULL` via `POST /memory/batch`
//!   (batched, not N single POSTs), then pull everything after the cursor and
//!   apply both.
//!
//! Properties:
//! - **Idempotent.** Identity is the stable UUID; pushes carry it as the cloud
//!   `external_id` and pulls record the cloud UUID as the local `remote_id` and
//!   dedupe on it, so re-running never duplicates. Same-millisecond boundary
//!   entries are harmless: the cursor comparison is strict (`>`) and pull
//!   dedupes by `remote_id`, so a re-applied boundary row is a no-op.
//! - **Entity-id reconciled.** A pulled entry that matches an existing local
//!   row's `kind`/`title`/`body` (`entity_id`) reuses that row instead of
//!   adding a duplicate: the row adopts the pulled `remote_id` if it had
//!   none, and archival propagates the same never-un-archive way a matching
//!   `remote_id` does; semantic-dup detection is the server's job (it flags
//!   `contradicts`).
//! - **Lifecycle propagation.** `supersedes` and archive/tombstone state travel
//!   in both directions (previously hard-coded `None`/dropped).
//! - **Text-only by default; optional pushed vector.** A push ships no vector
//!   and the server backfills the embedding (embedding-model conformance) —
//!   unless the server advertises `accepts_pushed_vectors`, in which case an
//!   entry with a local fp32/896 embedding carries it so the server stores it
//!   as-is instead of re-embedding.
//!
//! Split across submodules by concern: [`push`] (local → cloud), [`pull`]
//! (cloud → local, including pagination), and [`round`] (the two-phase
//! push+pull sequence `spelunk sync` runs). This file keeps only the command
//! entry points and the project/target resolution they share.

use anyhow::{Context, Result};

use super::{MemoryPullArgs, MemorySyncArgs};
use crate::{
    capability,
    cli::cmd::auth_api,
    config::Config,
    storage::{CloudSyncClient, MemoryStore},
};

mod pull;
mod push;
mod round;
#[cfg(test)]
mod test_support;

pub(super) use pull::parse_iso_to_secs;
use pull::pull_and_apply;
pub(super) use push::{
    LocalEmbedPolicy, local_embed_summary, push_local_oneway, unembedded_warning,
};
use round::{SyncRoundOutcome, sync_round};

/// Resolve the project slug to sync into, or halt with actionable guidance.
///
/// Precedence: an explicit `--project <slug>` overrides any configured
/// `project_id`; otherwise the configured `project_id` is used. When neither is
/// present the call **halts** — sync never auto-derives a name from the folder
/// or git remote (founder decision 2026-07-01, project-taxonomy). The returned
/// slug is sent verbatim to the server, which lazily creates the project on
/// first sync and reuses it on subsequent syncs.
fn resolve_sync_project(cli_project: Option<&str>, cfg: &Config) -> Result<String> {
    cli_project
        .map(str::to_string)
        .or_else(|| cfg.project_id.clone())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No project specified. Re-run as `spelunk sync --project <slug>` \
                 to choose the cloud project to sync into.\n\
                 (The project is created on first sync from the slug you pass; \
                 the slug is never guessed from the folder or git remote.)"
            )
        })
}

/// Resolve the cloud sync target (base URL, server-side project id, key).
///
/// Sync always speaks to an explicit `server_url`, not the inference loopback.
/// Errors with actionable guidance when the server is missing (via
/// [`capability::require_explicit_server_url`], the same guard `memory push` uses),
/// or when no project slug is available (see [`resolve_sync_project`]).
///
/// `feature` names the calling command (`"sync"` or `"memory pull"`) for the
/// error message. `cli_project` is the optional `--project <slug>` override;
/// when `None` the configured `project_id` is used.
///
/// The bearer key is resolved through [`auth_api::ensure_fresh_server_key`] so a
/// WorkOS access token that has expired since `spelunk login` is refreshed (and
/// the rotated tokens persisted) before the cloud-api call, rather than 401-ing.
async fn sync_target(
    feature: &str,
    cfg: &Config,
    cli_project: Option<&str>,
) -> Result<(String, String, Option<String>)> {
    let base_url = capability::require_explicit_server_url(feature, cfg)?;
    let project_id = resolve_sync_project(cli_project, cfg)?;
    let key = auth_api::ensure_fresh_server_key(cfg, &base_url).await?;
    Ok((base_url, project_id, key))
}

/// `spelunk memory pull` — one-way delta pull + apply.
pub async fn memory_pull(
    _args: MemoryPullArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
) -> Result<()> {
    let tier = capability::get_tier(cfg).await;
    capability::require_tier1("memory pull", tier, cfg.server_url.as_deref())?;
    let (base_url, project_id, key) = sync_target("memory pull", cfg, None).await?;

    let local = MemoryStore::open(mem_path)
        .with_context(|| format!("opening local memory at {}", mem_path.display()))?;
    let client = CloudSyncClient::new(
        &base_url,
        &project_id,
        key.as_deref(),
        cfg.server_ca.as_deref().map(std::path::Path::new),
    )?;

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
    let (base_url, project_id, key) = sync_target("sync", cfg, args.project.as_deref()).await?;

    let src_path = args.source.as_deref().unwrap_or(mem_path);
    let local = MemoryStore::open(src_path)
        .with_context(|| format!("opening local memory at {}", src_path.display()))?;
    let client = CloudSyncClient::new(
        &base_url,
        &project_id,
        key.as_deref(),
        cfg.server_ca.as_deref().map(std::path::Path::new),
    )?;

    // ── Two-phase reconciliation: pull, push, pull again off the same
    // pre-round cursor. See `sync_round` for why a plain push-then-pull (or
    // even pull-then-push) reorder is not sufficient. ───────────────────────
    let accepts_pushed_vectors = tier.caps().is_some_and(|c| c.accepts_pushed_vectors);
    let local_embed = LocalEmbedPolicy::for_push(cfg, src_path);
    let SyncRoundOutcome { pushed, pulled } = sync_round(
        &local,
        &client,
        args.include_archived,
        accepts_pushed_vectors,
        &local_embed,
    )
    .await?;
    if pushed.without_local_vector > 0 {
        eprintln!("{}", unembedded_warning(pushed.without_local_vector));
    }
    // Appended to the success summaries so relates_to propagation is visible
    // without cluttering the (unchanged) failure/interrupted framing.
    let edges_note = if pushed.edges_pushed > 0 {
        format!(" Linked {} relationship edge(s).", pushed.edges_pushed)
    } else {
        String::new()
    };

    if pushed.attempted == 0 {
        println!(
            "Nothing to push — {} entries already synced. Applied {} new remote entries.",
            pushed.already_synced, pulled
        );
    } else if let Some(reason) = pushed.interrupted.as_deref() {
        // A chunk failed mid-push. The pull half already ran (it is independently
        // useful, so there is no reason to withhold remote changes), but the
        // command must fail loud and tell the user how to resume: the chunks that
        // landed are durably stamped, so a re-run pushes only the remainder and
        // already-pushed entries come back as 207 `skipped`.
        anyhow::bail!(
            "Pushed {} of {} entries, then stopped: {reason}. \
             Re-run to resume (already-pushed entries are skipped). \
             Pull applied {} new remote entries.",
            pushed.created + pushed.skipped,
            pushed.attempted,
            pulled
        );
    } else if pushed.created == 0 && pushed.skipped == 0 {
        // Total push failure: nothing durably landed cloud-side. The pull
        // already ran (it's an independent, unconditionally useful step —
        // there's no reason to withhold remote changes just because the push
        // half failed), but the command must still fail loud: a caller
        // skimming for "Sync complete" or checking only the exit code must
        // not read this as success.
        anyhow::bail!(
            "Sync failed: 0 of {} push entries reached the server ({} failed); \
             pull still applied {} new remote entries.",
            pushed.attempted,
            pushed.failed,
            pulled
        );
    } else if pushed.failed > 0 {
        println!(
            "Sync complete. Pushed {} entries (created {}, skipped {}, {} failed), applied {} new remote entries.{}{}",
            pushed.attempted,
            pushed.created,
            pushed.skipped,
            pushed.failed,
            pulled,
            local_embed_summary(&pushed),
            edges_note
        );
    } else {
        println!(
            "Sync complete. Pushed {} entries (created {}, skipped {}), applied {} new remote entries.{}{}",
            pushed.attempted,
            pushed.created,
            pushed.skipped,
            pulled,
            local_embed_summary(&pushed),
            edges_note
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── resolve_sync_project ───────────────────────────────────────────────
    // Sync must never invent a project name. With neither `--project` nor a
    // configured `project_id`, the call halts with a message pointing the user
    // at `--project <slug>`; with an explicit slug (or configured id), that slug
    // is threaded through verbatim so it reaches the outbound request.

    fn cfg_with_project(id: Option<&str>) -> Config {
        Config {
            project_id: id.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_sync_project_halts_when_nothing_configured_or_passed() {
        let cfg = cfg_with_project(None);
        let err = resolve_sync_project(None, &cfg).unwrap_err();
        let msg = err.to_string();
        // Actionable: names the exact re-run and refuses to guess.
        assert!(msg.contains("--project <slug>"), "msg: {msg}");
        assert!(
            msg.contains("never guessed") || msg.contains("git remote"),
            "must state it won't auto-derive: {msg}"
        );
    }

    #[test]
    fn resolve_sync_project_uses_cli_flag_when_passed() {
        let cfg = cfg_with_project(None);
        let slug = resolve_sync_project(Some("acme/app"), &cfg).unwrap();
        assert_eq!(slug, "acme/app");
    }

    #[test]
    fn resolve_sync_project_falls_back_to_configured_id() {
        let cfg = cfg_with_project(Some("team/proj"));
        let slug = resolve_sync_project(None, &cfg).unwrap();
        assert_eq!(slug, "team/proj");
    }

    #[test]
    fn resolve_sync_project_cli_flag_overrides_configured_id() {
        let cfg = cfg_with_project(Some("team/proj"));
        let slug = resolve_sync_project(Some("other/slug"), &cfg).unwrap();
        assert_eq!(slug, "other/slug");
    }

    #[test]
    fn resolve_sync_project_treats_blank_slug_as_absent() {
        // A whitespace-only `--project ""` must not silently pass an empty slug.
        let cfg = cfg_with_project(None);
        assert!(resolve_sync_project(Some("   "), &cfg).is_err());
    }

    // ── end-to-end first-run path ──────────────────────────────────────────
    // The story's target path: a first-run user has a non-loopback team
    // `server_url`, NO configured `project_id`, and passes `--project <slug>`.
    // Before the fix this was rejected at dispatch by `cfg.validate()` before
    // `resolve_sync_project` ever ran. This test walks the same two gates the
    // dispatcher + `memory_sync` cross — (1) config validation must accept the
    // `--project`-only config, (2) the resolved slug must reach the outbound
    // request — proving the path is live end to end (minus the auth/tier
    // machinery, which is orthogonal to this story).
    #[tokio::test]
    async fn first_run_project_flag_only_passes_dispatch_and_reaches_wire() {
        use crate::storage::{BatchPushItem, CloudSyncClient};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // A non-loopback team server_url with NO project_id — a genuine first run.
        let cfg = Config {
            server_url: Some("http://spelunk.internal:7777".to_string()),
            project_id: None,
            ..Default::default()
        };
        let cli_project = Some("acme/app");

        // Gate 1 — dispatch: `--project` makes a project available, so the
        // non-loopback server_url no longer blocks (regression under test).
        let project_available = cli_project.is_some() || cfg.project_id.is_some();
        cfg.validate_with_project(project_available)
            .expect("first-run --project must pass dispatch validation");

        // Gate 2 — resolution: the explicit slug wins and is what sync targets.
        let slug = resolve_sync_project(cli_project, &cfg).unwrap();
        assert_eq!(slug, "acme/app");

        // Wire: the resolved slug must land, percent-encoded, in the request
        // path so the server can lazily create/reuse that project.
        Mock::given(method("POST"))
            .and(path("/v1/projects/acme%2Fapp/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": "e1", "id": "cloud-1"}]
            })))
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), &slug, None, None).unwrap();
        let res = client
            .push_batch(vec![BatchPushItem {
                kind: "decision".into(),
                title: "T".into(),
                body: Some("B".into()),
                external_id: "e1".into(),
                source_commit: None,
                vector: None,
                vector_model: None,
                vector_precision: None,
            }])
            .await
            .expect("push to the lazily-created project must succeed");
        assert_eq!(res.created, 1);
    }
}
