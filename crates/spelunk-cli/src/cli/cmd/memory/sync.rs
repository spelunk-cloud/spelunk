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
//! - **Keep-both / Add-Wins.** Pulled entries are added, never overwriting local
//!   ones; semantic-dup detection is the server's job (it flags `contradicts`).
//! - **Lifecycle propagation.** `supersedes` and archive/tombstone state travel
//!   in both directions (previously hard-coded `None`/dropped).
//! - **Text-only by default; optional pushed vector.** A push ships no vector
//!   and the server backfills the embedding (embedding-model conformance) —
//!   unless the server advertises `accepts_pushed_vectors`, in which case an
//!   entry with a local fp32/896 embedding carries it so the server stores it
//!   as-is instead of re-embedding.

use anyhow::{Context, Result};

use super::{MemoryPullArgs, MemorySyncArgs};
use crate::{
    capability,
    cli::cmd::auth_api,
    config::Config,
    storage::{BatchPushItem, CloudSyncClient, MemoryStore},
};

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
/// Sync always speaks to an explicit `server_url` — it is the cloud-convergence
/// path, not the inference loopback. Errors with actionable guidance when the
/// server is missing, or when no project slug is available (see
/// [`resolve_sync_project`]).
///
/// `cli_project` is the optional `--project <slug>` override; when `None` the
/// configured `project_id` is used.
///
/// The bearer key is resolved through [`auth_api::ensure_fresh_server_key`] so a
/// WorkOS access token that has expired since `spelunk login` is refreshed (and
/// the rotated tokens persisted) before the cloud-api call, rather than 401-ing.
async fn sync_target(
    cfg: &Config,
    cli_project: Option<&str>,
) -> Result<(String, String, Option<String>)> {
    let base_url = cfg.server_url.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "sync requires a server. Set `server_url` in your spelunk config \
             (e.g. ~/.config/spelunk/config.toml or .spelunk/config.toml)."
        )
    })?;
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
    let (base_url, project_id, key) = sync_target(cfg, None).await?;

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
    let (base_url, project_id, key) = sync_target(cfg, args.project.as_deref()).await?;

    let src_path = args.source.as_deref().unwrap_or(mem_path);
    let local = MemoryStore::open(src_path)
        .with_context(|| format!("opening local memory at {}", src_path.display()))?;
    let client = CloudSyncClient::new(
        &base_url,
        &project_id,
        key.as_deref(),
        cfg.server_ca.as_deref().map(std::path::Path::new),
    )?;

    // ── Push local → cloud (batched, idempotent on UUID). Attaches the local
    // fp32/896 vector when the server advertises `accepts_pushed_vectors`,
    // otherwise text-only and the server re-embeds. ────────────────────────
    let accepts_pushed_vectors = tier.caps().is_some_and(|c| c.accepts_pushed_vectors);
    let pushed = push_local(
        &local,
        &client,
        args.include_archived,
        accepts_pushed_vectors,
    )
    .await?;

    // ── Pull cloud → local (delta after the UUID cursor, keep-both) ─────────
    let pulled = pull_and_apply(&local, &client).await?;

    if pushed.attempted == 0 {
        println!(
            "Nothing to push — {} entries already synced. Applied {} new remote entries.",
            pushed.already_synced, pulled
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
            "Sync complete. Pushed {} entries (created {}, skipped {}, {} failed), applied {} new remote entries.",
            pushed.attempted, pushed.created, pushed.skipped, pushed.failed, pulled
        );
    } else {
        println!(
            "Sync complete. Pushed {} entries (created {}, skipped {}), applied {} new remote entries.",
            pushed.attempted, pushed.created, pushed.skipped, pulled
        );
    }
    Ok(())
}

/// Outcome of a push pass (shared by `sync` and the one-way `memory push`).
pub(super) struct PushSummary {
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
}

/// One-way push entry point reused by `spelunk memory push`.
///
/// `accepts_pushed_vectors` mirrors the destination server's `/v1/health`
/// capability: when true, each entry that has a local embedding
/// carries its fp32/896 vector so the server stores it as-is; when false the
/// push is text-only and the server re-embeds.
pub(super) async fn push_local_oneway(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
    accepts_pushed_vectors: bool,
) -> Result<PushSummary> {
    push_local(local, client, include_archived, accepts_pushed_vectors).await
}

/// Push local entries to the cloud in batches, then propagate tombstones for any
/// archived rows that exist cloud-side. Each entry is text-only unless
/// `accepts_pushed_vectors` is set and the row has a local fp32/896 embedding,
/// in which case that vector is attached (the server stores it without
/// re-embedding).
async fn push_local(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
    accepts_pushed_vectors: bool,
) -> Result<PushSummary> {
    let rows = local.rows_for_sync(include_archived)?;
    if rows.is_empty() {
        return Ok(PushSummary {
            attempted: 0,
            created: 0,
            skipped: 0,
            failed: 0,
            already_synced: 0,
        });
    }

    // Split into live entries (batch-created/upserted by external_id) and
    // archived entries already known to the cloud (tombstoned via DELETE).
    let mut created = 0u32;
    let mut skipped = 0u32;
    let mut failed = 0u32;

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
    // Map external_id (local uuid) → local_id so we can record the cloud-minted
    // id returned in the 207 result back onto the local row.
    for chunk in live.chunks(200) {
        let mut items: Vec<BatchPushItem> = Vec::with_capacity(chunk.len());
        for r in chunk {
            // Only read the local embedding when the server can accept it. The
            // stored blob is raw little-endian fp32 (`vec_to_blob`); decode it
            // and only attach a correctly-dimensioned (896) vector — a
            // wrong-length or missing embedding falls back to text-only rather
            // than poisoning the whole batch with a 4xx.
            let vector = if accepts_pushed_vectors {
                local
                    .get_embedding(r.local_id)?
                    .map(|blob| spelunk_core::embeddings::blob_to_vec(&blob))
                    .filter(|v| v.len() == spelunk_core::embeddings::EMBEDDING_DIM)
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
        let res = client.push_batch(items).await?;

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
        failed,
        already_synced,
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

    // ── push_local end-to-end: remote_id stamping + idempotent re-sync ─────
    // The local-first push path is where the server-minted
    // cross-machine id is PERSISTED — stamped onto `notes.remote_id` from the
    // 207 batch result — not the `RemoteMemoryBackend::add` debug-log path
    // (which is the cloud-first, remote-is-store-of-record case with no local
    // row). Locks in that a push stamps `remote_id` and a re-push sends nothing
    // (no duplicate cloud writes, no local dupes).

    fn register_sqlite_vec() {
        use std::sync::OnceLock;
        // `MemoryStore::open` creates a vec0 table, so the extension must be
        // registered before any connection opens.
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

    #[tokio::test]
    async fn push_local_stamps_remote_id_and_repush_is_idempotent() {
        use tempfile::TempDir;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        register_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
        store
            .add_note("decision", "One", "first", &[], &[], None, None)
            .unwrap();
        store
            .add_note("note", "Two", "second", &[], &[], None, None)
            .unwrap();

        // Learn the lazily-minted external_ids up front so the mock can echo
        // them back with distinct cloud ids; `ensure_uuid` is idempotent, so the
        // push below re-derives the same uuids.
        let rows = store.rows_for_sync(false).unwrap();
        assert_eq!(rows.len(), 2);
        let (ext_a, ext_b) = (rows[0].uuid.clone(), rows[1].uuid.clone());
        let cloud_a = "01890000-0000-7000-8000-0000000000a1";
        let cloud_b = "01890000-0000-7000-8000-0000000000a2";

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 2, "skipped": 0, "failed": 0,
                "results": [
                    {"status": "created", "external_id": ext_a, "id": cloud_a},
                    {"status": "created", "external_id": ext_b, "id": cloud_b},
                ]
            })))
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

        // First push: creates both, persists the server-minted id on each row.
        let s1 = push_local(&store, &client, false, false).await.unwrap();
        assert_eq!((s1.attempted, s1.created, s1.skipped), (2, 2, 0));
        assert_eq!(
            store.note_id_for_remote_id(cloud_a).unwrap(),
            Some(rows[0].local_id)
        );
        assert_eq!(
            store.note_id_for_remote_id(cloud_b).unwrap(),
            Some(rows[1].local_id)
        );
        // The pull cursor is now the newest stamped id.
        assert_eq!(store.max_remote_id().unwrap().as_deref(), Some(cloud_b));

        // Second push: every row carries a `remote_id`, so the live set is empty
        // and no batch request is sent — the re-sync is a no-op. `attempted` must
        // reflect that (not the raw row count), so callers never report "Pushed
        // N" when nothing was sent.
        let s2 = push_local(&store, &client, false, false).await.unwrap();
        assert_eq!((s2.attempted, s2.created, s2.already_synced), (0, 0, 2));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "re-push must not hit the batch endpoint again"
        );
        // No duplicate local rows introduced by the round trip.
        assert_eq!(store.count().unwrap(), 2);
    }

    // ── pushed-vector fast path ─────────────────────────────────────────────
    // A note with a local fp32/896 embedding carries that vector (+ model tag
    // + precision "fp32") to a server advertising `accepts_pushed_vectors`, so
    // the server stores it as-is; against a server without the capability the
    // same note is pushed text-only even though the vector is available. This
    // exercises the full `push_local` wiring: it reads the local embedding and
    // consults the gate, which the `maybe_attach_vector` unit test cannot.

    /// Insert an active note plus a valid L2-normalised fp32/896 embedding,
    /// returning its local id + external uuid.
    fn note_with_embedding(store: &MemoryStore) -> (i64, String) {
        store
            .add_note("decision", "One", "first", &[], &[], None, None)
            .unwrap();
        let dim = spelunk_core::embeddings::EMBEDDING_DIM;
        let vec: Vec<f32> = vec![1.0 / (dim as f32).sqrt(); dim];
        let blob = spelunk_core::embeddings::vec_to_blob(&vec);
        let rows = store.rows_for_sync(false).unwrap();
        assert_eq!(rows.len(), 1);
        store.insert_embedding(rows[0].local_id, &blob).unwrap();
        (rows[0].local_id, rows[0].uuid.clone())
    }

    #[tokio::test]
    async fn push_local_attaches_vector_when_server_accepts() {
        use tempfile::TempDir;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        register_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
        let (_id, uuid) = note_with_embedding(&store);

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": uuid, "id": "cloud-1"}]
            })))
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

        // accepts_pushed_vectors = true → the fp32/896 vector reaches the wire.
        push_local(&store, &client, false, true).await.unwrap();

        let reqs = server.received_requests().await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let entry = &json["entries"][0];
        assert_eq!(
            entry["vector"].as_array().map(Vec::len),
            Some(spelunk_core::embeddings::EMBEDDING_DIM),
            "server that accepts vectors must receive the 896-dim vector: {entry}"
        );
        assert_eq!(entry["vector_model"], "F2LLM-v2-330M");
        assert_eq!(entry["vector_precision"], "fp32");
    }

    #[tokio::test]
    async fn push_local_stays_text_only_when_server_declines() {
        use tempfile::TempDir;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        register_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
        let (_id, uuid) = note_with_embedding(&store);

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": uuid, "id": "cloud-1"}]
            })))
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

        // accepts_pushed_vectors = false → text-only, despite a local vector.
        push_local(&store, &client, false, false).await.unwrap();

        let reqs = server.received_requests().await.unwrap();
        let body = String::from_utf8(reqs[0].body.clone()).unwrap();
        assert!(
            !body.contains("vector"),
            "server without the capability must get a text-only push: {body}"
        );
    }

    /// A note queued for push with NO `note_embeddings` row at all (never
    /// embedded locally, or embedding failed) must fall back to a text-only
    /// push for that row — not crash, and not send an empty/malformed
    /// `vector` field — even though the server accepts pushed vectors.
    #[tokio::test]
    async fn push_local_falls_back_to_text_only_when_local_embedding_missing() {
        use tempfile::TempDir;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        register_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
        // Deliberately no `insert_embedding` call — this note has never been
        // embedded.
        store
            .add_note("decision", "Unembedded", "first", &[], &[], None, None)
            .unwrap();
        let rows = store.rows_for_sync(false).unwrap();
        assert_eq!(rows.len(), 1);
        let uuid = rows[0].uuid.clone();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": uuid, "id": "cloud-1"}]
            })))
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

        // accepts_pushed_vectors = true, but no local embedding exists.
        let summary = push_local(&store, &client, false, true).await.unwrap();
        assert_eq!(
            (summary.attempted, summary.created, summary.failed),
            (1, 1, 0)
        );

        let reqs = server.received_requests().await.unwrap();
        let body = String::from_utf8(reqs[0].body.clone()).unwrap();
        assert!(
            !body.contains("vector"),
            "a note with no local embedding must fall back to text-only, \
             not error or send a malformed vector: {body}"
        );
    }

    /// `note_embeddings` is a `vec0` virtual table with a `FLOAT[896]` column
    /// (migration `004_memory.sql`) — sqlite-vec enforces that exact
    /// dimension AT INSERT TIME, for every write path (there is only one:
    /// `insert_embedding`). So a "leftover pre-896 768-dim row" — unlike the
    /// code-chunk `embeddings` table, which DID have a legacy 768-dim era
    /// with an explicit recreate-on-open migration in `db.rs` — can never
    /// actually be written for memory notes: there was never a 768-dim
    /// memory-embedding vintage to migrate from, and the store itself
    /// refuses the write. Confirmed here rather than assumed, since it is
    /// exactly the scenario `push_local`'s dimension guard names in its
    /// comment.
    #[tokio::test]
    async fn insert_embedding_rejects_wrong_dimension_vector() {
        use tempfile::TempDir;

        register_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
        store
            .add_note("decision", "One", "first", &[], &[], None, None)
            .unwrap();
        let rows = store.rows_for_sync(false).unwrap();

        let stale_768_blob = spelunk_core::embeddings::vec_to_blob(&vec![1.0f32; 768]);
        let err = store
            .insert_embedding(rows[0].local_id, &stale_768_blob)
            .unwrap_err();
        assert!(
            err.to_string().contains("896") && err.to_string().contains("768"),
            "the vec0 FLOAT[896] column must refuse a 768-dim insert outright \
             (this is what makes a wrong-dimension row unreachable via any \
             application write path): {err}"
        );
    }

    /// Since a wrong-dimension row can never be *written* (previous test),
    /// the only way `push_local`'s guard (`blob_to_vec` decode + a length
    /// check against `EMBEDDING_DIM` — the same two building blocks
    /// exercised inline at `sync.rs`'s vector-resolution site) could ever
    /// see a wrong-length vector is an on-disk blob corrupted or torn
    /// independently of any insert (disk fault, a crash mid-write). This
    /// pins that composed guard logic directly: it must never panic on a
    /// short/truncated/empty blob, and must always filter such a decode out
    /// rather than accept a spurious length or garbage-padded 896 vector.
    #[test]
    fn dim_guard_logic_rejects_short_truncated_and_empty_blobs() {
        use spelunk_core::embeddings::{EMBEDDING_DIM, blob_to_vec, vec_to_blob};

        let guarded = |blob: &[u8]| -> Option<Vec<f32>> {
            Some(blob_to_vec(blob)).filter(|v| v.len() == EMBEDDING_DIM)
        };

        // A stale 768-float blob (wrong dimension, but cleanly decodable).
        let stale_768 = vec_to_blob(&vec![1.0f32; 768]);
        assert!(
            guarded(&stale_768).is_none(),
            "a 768-float blob must be filtered out, not accepted"
        );

        // A torn write: a valid 896-dim blob with its last few bytes cut off.
        let full = vec_to_blob(&vec![1.0f32; EMBEDDING_DIM]);
        let truncated = &full[..full.len() - 10];
        assert_ne!(
            blob_to_vec(truncated).len(),
            EMBEDDING_DIM,
            "sanity: a truncated blob must decode to a non-896 length"
        );
        assert!(
            guarded(truncated).is_none(),
            "a truncated blob must never pass the dimension guard"
        );

        // A zero-length blob (e.g. corrupted read).
        assert!(
            guarded(&[]).is_none(),
            "an empty blob must be filtered out, not treated as a valid vector"
        );
    }

    // ── stamping must not trust a non-persisted status ─────────────────────
    // A server can return a per-item `id` for an entry alongside a status
    // that does not affirm durable persistence (aggregate `created: 0`).
    // Stamping `remote_id` anyway would permanently exclude the row from
    // `live` on every future push — the data could never be retried. Only
    // `created`/`skipped` may stamp; a `failed` item carrying an `id` must be
    // left unstamped.
    #[tokio::test]
    async fn push_local_does_not_stamp_remote_id_for_a_failed_status_item() {
        use tempfile::TempDir;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        register_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
        store
            .add_note("decision", "One", "first", &[], &[], None, None)
            .unwrap();

        let rows = store.rows_for_sync(false).unwrap();
        assert_eq!(rows.len(), 1);
        let ext_a = rows[0].uuid.clone();
        // The server hands back an `id` even though the entry was not
        // durably persisted (`created: 0`, status "failed").
        let cloud_a = "01890000-0000-7000-8000-0000000000b1";

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 0, "skipped": 0, "failed": 1,
                "results": [
                    {"status": "failed", "external_id": ext_a, "id": cloud_a},
                ]
            })))
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

        let s1 = push_local(&store, &client, false, false).await.unwrap();
        assert_eq!((s1.attempted, s1.created, s1.skipped), (1, 0, 0));

        // The row must NOT carry the id the server handed back — it stays
        // retryable on the next push.
        assert_eq!(store.note_id_for_remote_id(cloud_a).unwrap(), None);
        let rows_after = store.rows_for_sync(false).unwrap();
        assert_eq!(rows_after[0].remote_id, None);

        // A re-push must still consider this row live (not already-synced).
        let live_again: Vec<_> = rows_after
            .iter()
            .filter(|r| !r.archived && r.remote_id.is_none())
            .collect();
        assert_eq!(live_again.len(), 1, "unstamped row must remain retryable");
    }

    // ── counts must reconcile against results[], not the aggregate ints ────
    // `BatchPushResult`'s `created`/`skipped` ints and its `results[]` array
    // are independent wire fields — a server can send an aggregate
    // `created: 0` for a batch whose `results[]` shows every entry durably
    // persisted. A push summary built from the aggregate ints alone would
    // read as "nothing landed"; it must instead read the true outcome off
    // `results[].status`.
    #[tokio::test]
    async fn push_local_reconciles_counts_from_results_not_aggregate_ints() {
        use tempfile::TempDir;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        register_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
        store
            .add_note("decision", "One", "first", &[], &[], None, None)
            .unwrap();
        store
            .add_note("note", "Two", "second", &[], &[], None, None)
            .unwrap();

        let rows = store.rows_for_sync(false).unwrap();
        let (ext_a, ext_b) = (rows[0].uuid.clone(), rows[1].uuid.clone());
        let cloud_a = "01890000-0000-7000-8000-0000000000c1";
        let cloud_b = "01890000-0000-7000-8000-0000000000c2";

        let server = MockServer::start().await;
        // The aggregate ints understate what happened (`created: 0, skipped:
        // 0`), but `results[]` shows both entries durably persisted.
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 0, "skipped": 0, "failed": 0,
                "results": [
                    {"status": "created", "external_id": ext_a, "id": cloud_a},
                    {"status": "skipped", "external_id": ext_b, "id": cloud_b},
                ]
            })))
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

        let s1 = push_local(&store, &client, false, false).await.unwrap();
        // Reconciled from `results[]`, not the misleading aggregate zeros.
        assert_eq!(
            (s1.attempted, s1.created, s1.skipped, s1.failed),
            (2, 1, 1, 0)
        );
        assert_eq!(
            store.note_id_for_remote_id(cloud_a).unwrap(),
            Some(rows[0].local_id)
        );
        assert_eq!(
            store.note_id_for_remote_id(cloud_b).unwrap(),
            Some(rows[1].local_id)
        );
    }

    // ── a failed item must not mask other successes in the same batch ─────
    // Mixed outcome: one entry lands, one doesn't. The failed item must stay
    // unstamped (retryable) while the successful one is recorded — and the
    // summary must show the real partial success, not a false "nothing
    // happened" (which is what reading only the aggregate `created` count
    // for a batch containing any failure could produce).
    #[tokio::test]
    async fn push_local_partial_failure_reports_the_real_successes() {
        use tempfile::TempDir;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        register_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
        store
            .add_note("decision", "One", "first", &[], &[], None, None)
            .unwrap();
        store
            .add_note("note", "Two", "second", &[], &[], None, None)
            .unwrap();

        let rows = store.rows_for_sync(false).unwrap();
        let (ext_a, ext_b) = (rows[0].uuid.clone(), rows[1].uuid.clone());
        let cloud_a = "01890000-0000-7000-8000-0000000000d1";
        let cloud_b = "01890000-0000-7000-8000-0000000000d2";

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 1,
                "results": [
                    {"status": "created", "external_id": ext_a, "id": cloud_a},
                    {"status": "failed", "external_id": ext_b, "id": cloud_b},
                ]
            })))
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

        let s1 = push_local(&store, &client, false, false).await.unwrap();
        assert_eq!(
            (s1.attempted, s1.created, s1.skipped, s1.failed),
            (2, 1, 0, 1),
            "attempted must stay 2 (not read as nothing-to-push) and the \
             genuine success must be visible alongside the failure"
        );
        // The successful row is stamped...
        assert_eq!(
            store.note_id_for_remote_id(cloud_a).unwrap(),
            Some(rows[0].local_id)
        );
        // ...the failed one is not, and remains retryable.
        assert_eq!(store.note_id_for_remote_id(cloud_b).unwrap(), None);
        let rows_after = store.rows_for_sync(false).unwrap();
        let live_again: Vec<_> = rows_after
            .iter()
            .filter(|r| !r.archived && r.remote_id.is_none())
            .collect();
        assert_eq!(live_again.len(), 1, "failed row must remain retryable");
    }

    // ── push_local's counting stays honest on a total-failure batch ────────
    // `push_local` itself just reports honest counts (Bug 1/3's fix); it is
    // the command layer (`memory_push` / `memory_sync`) that decides whether
    // those counts mean "Done"/"Sync complete" or a hard failure, and that
    // command-layer decision (the `bail!` that gives the CLI its non-zero
    // exit) is covered end to end by the subprocess tests in
    // `crates/spelunk-cli/tests/memory_push_sync_total_failure.rs`, not here.
    // This test only pins `push_local`'s own return value for the all-failed
    // shape those command-layer tests depend on.
    #[tokio::test]
    async fn push_local_total_failure_reports_zero_created_and_skipped() {
        use tempfile::TempDir;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        register_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
        store
            .add_note("decision", "One", "first", &[], &[], None, None)
            .unwrap();
        store
            .add_note("note", "Two", "second", &[], &[], None, None)
            .unwrap();

        let rows = store.rows_for_sync(false).unwrap();
        let (ext_a, ext_b) = (rows[0].uuid.clone(), rows[1].uuid.clone());

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 0, "skipped": 0, "failed": 2,
                "results": [
                    {"status": "failed", "external_id": ext_a, "id": serde_json::Value::Null},
                    {"status": "failed", "external_id": ext_b, "id": serde_json::Value::Null},
                ]
            })))
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

        let s1 = push_local(&store, &client, false, false).await.unwrap();
        assert_eq!(
            (s1.attempted, s1.created, s1.skipped, s1.failed),
            (2, 0, 0, 2),
            "total failure: attempted > 0 but nothing durably landed"
        );
        // Neither row is stamped — both remain retryable.
        let rows_after = store.rows_for_sync(false).unwrap();
        assert!(rows_after.iter().all(|r| r.remote_id.is_none()));
    }
}
