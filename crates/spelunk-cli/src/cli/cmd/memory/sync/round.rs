//! `sync_round`: `spelunk sync`'s two-phase pull/push/pull sequence.

use anyhow::{Context, Result};

use crate::storage::{CloudSyncClient, MemoryStore};

use super::pull::pull_and_apply_since;
use super::push::{LocalEmbedPolicy, PushSummary, push_local};

/// Outcome of one [`sync_round`]: the push summary plus the total newly
/// applied entries across both pull passes.
#[derive(Debug)]
pub(super) struct SyncRoundOutcome {
    pub(super) pushed: PushSummary,
    pub(super) pulled: usize,
}

/// Run one full two-way sync round: pull, then push, then pull again off the
/// same pre-round cursor, except on a genuinely first sync, which pushes
/// first (see [`sync_round_first`]).
///
/// This is `spelunk sync`'s actual push+pull sequence, extracted into its own
/// function so it can be exercised directly in tests against a real server:
/// the command entry point (`memory_sync`) can't be unit-tested cheaply
/// because of its config/tier-probe plumbing (`get_tier`'s per-process cache
/// makes multiple differently-configured in-process probes unreliable within
/// one test binary).
///
/// Neither a plain push-then-pull nor a plain pull-then-push reorder is
/// sufficient for an established client (one with real sync history). The
/// failure mode: the cursor is always `MAX(remote_id)` over local rows
/// (decision #183, no persisted watermark), and this client's own push mints
/// `remote_id`s stamped "now", chronologically the newest thing on the
/// server. If a plain re-derived cursor were used for a second pull, this
/// round's own just-pushed rows would become the new `MAX(remote_id)`,
/// permanently shadowing (via the strict `>` comparison) any teammate entry
/// that landed between this round's first pull and its own push.
///
/// The fix for that case: capture the cursor once, before this round's own
/// pull or push touches anything (`pre_round_cursor`), pull with it, push,
/// then pull AGAIN reusing that SAME `pre_round_cursor`, not a freshly
/// re-derived one. The second pull harmlessly re-fetches this round's own
/// just-pushed rows (their `remote_id` is now `> pre_round_cursor`) alongside
/// anything a teammate pushed in the gap; both are idempotent no-ops or
/// genuine new applies via [`pull_and_apply_since`], so the combined count is
/// never inflated by double-counting.
///
/// A first sync (`pre_round_cursor` is `None`, meaning nothing has ever
/// pulled or pushed for this store) is a different case entirely: there is
/// nothing to pull yet, since the project cannot exist server-side until
/// this round's own push provisions it, and no shadowing risk, since nothing
/// local has ever synced before to be shadowed. Pulling first there sends a
/// pull request the server has no way to answer (nothing has provisioned
/// the project yet), so that case pushes first instead; see
/// [`sync_round_first`].
pub(super) async fn sync_round(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
    accepts_pushed_vectors: bool,
    local_embed: &LocalEmbedPolicy<'_>,
) -> Result<SyncRoundOutcome> {
    let pre_round_cursor = local.max_remote_id()?;

    if pre_round_cursor.is_none() {
        return sync_round_first(
            local,
            client,
            include_archived,
            accepts_pushed_vectors,
            local_embed,
        )
        .await;
    }

    let pulled_first = pull_and_apply_since(local, client, pre_round_cursor.as_deref()).await?;

    let pushed = push_local(
        local,
        client,
        include_archived,
        accepts_pushed_vectors,
        local_embed,
    )
    .await?;

    // If this second pull errors (network blip, transient 5xx), the error
    // propagates out of `sync_round` rather than being swallowed: `?`
    // surfaces it to `memory_sync`, which reports a failure and a non-zero
    // exit. That is correct (a real error must not be silently dropped), but
    // by this point `pushed` already reflects a push that may have durably
    // landed server-side (and stamped local `remote_id`s accordingly, inside
    // `push_local`, before this call even runs), so the failure is scoped to
    // the confirmation pull, not the push. Attach that context so the
    // surfaced error doesn't read as "nothing happened": a caller shouldn't
    // conclude their content was lost and try to force a re-push (harmless
    // but pointless: already-stamped rows are excluded from `live` and
    // skipped) instead of simply re-running sync, which retries the pull with
    // an unaffected, freshly-derived cursor.
    let pulled_second = pull_and_apply_since(local, client, pre_round_cursor.as_deref())
        .await
        .with_context(|| {
            format!(
                "confirmation pull failed after this round's push already reached \
                 the server ({} attempted: {} created, {} skipped, {} failed) - \
                 the push is not affected by this error; re-running sync will retry \
                 the pull without re-pushing already-landed entries",
                pushed.attempted, pushed.created, pushed.skipped, pushed.failed
            )
        })?;

    Ok(SyncRoundOutcome {
        pushed,
        pulled: pulled_first + pulled_second,
    })
}

/// The first-sync half of [`sync_round`]: a store that has never pulled or
/// pushed before (`local.max_remote_id()` is `None`).
///
/// A brand new project only comes into existence server-side via this
/// round's own push (`push_local`, which provisions it and caches its id per
/// ADR-005). Pulling before that push runs has nothing to fetch and nowhere
/// valid to fetch it from, so this pushes first, then runs a single pull
/// (there is no pre-round cursor to preserve, and no earlier round's state
/// that a freshly re-derived cursor could shadow, since nothing has ever
/// synced before this round).
async fn sync_round_first(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
    accepts_pushed_vectors: bool,
    local_embed: &LocalEmbedPolicy<'_>,
) -> Result<SyncRoundOutcome> {
    let pushed = push_local(
        local,
        client,
        include_archived,
        accepts_pushed_vectors,
        local_embed,
    )
    .await?;

    // Mirrors the established-client confirmation pull's error handling: a
    // pull failure here must not read as "nothing happened" when the push
    // already durably landed.
    let pulled = pull_and_apply_since(local, client, None)
        .await
        .with_context(|| {
            format!(
                "post-push pull failed on this project's first sync, after the push already \
                 reached the server ({} attempted: {} created, {} skipped, {} failed) - \
                 the push is not affected by this error; re-running sync will retry \
                 the pull without re-pushing already-landed entries",
                pushed.attempted, pushed.created, pushed.skipped, pushed.failed
            )
        })?;

    Ok(SyncRoundOutcome { pushed, pulled })
}

#[cfg(test)]
mod tests {
    use super::super::pull::pull_and_apply;
    use super::super::test_support::{fresh_store, spawn_spelunk_server};
    use super::*;

    // ── sync_round: two-phase reconciliation ────────────────────────────────
    // `sync_round` is `memory_sync`'s actual push+pull sequence, extracted so
    // it can be driven directly against a real spawned server. `memory_sync`
    // itself can't cheaply carry these scenarios: `capability::get_tier`
    // caches its probe result in a per-process `OnceCell`, so several
    // differently-configured in-process probes in one test binary would see
    // stale tiers from whichever test's probe ran first.

    // The primary repro, fixed: a client with local-only, never-pushed
    // content, running the actual `sync_round` sequence against a project
    // that already has a teammate's prior entry (pushed strictly before
    // this round begins),
    // ends the round with that teammate entry applied - not 0. This is
    // exactly the case the existing `two_established_clients_...` test
    // deliberately routes around (see its own comment) because, before this
    // fix, `memory_sync`'s push-then-pull order shadowed it permanently.
    #[tokio::test]
    async fn sync_round_pulls_teammates_prior_entry_on_a_first_round_with_local_content() {
        let addr = spawn_spelunk_server().await;
        let base_url = format!("http://{addr}");

        // Teammate A establishes the project first, entirely before client
        // C's own sync round begins.
        let (_tmp_a, store_a) = fresh_store();
        store_a
            .add_note(
                "decision",
                "A1",
                "teammate's prior entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let client_a = CloudSyncClient::new(&base_url, "proj-primary", None, None).unwrap();
        assert_eq!(
            push_local(&store_a, &client_a, false, false, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .created,
            1
        );

        // Client C has its own never-pushed local entry and has never synced.
        let (_tmp_c, store_c) = fresh_store();
        store_c
            .add_note(
                "decision",
                "C1",
                "client C's own new entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let client_c = CloudSyncClient::new(&base_url, "proj-primary", None, None).unwrap();

        let outcome = sync_round(&store_c, &client_c, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();
        assert_eq!(outcome.pushed.created, 1, "C's own entry must land");
        assert_eq!(
            outcome.pulled, 1,
            "C must pull A's prior entry within this same round, not 0"
        );
        let titles: Vec<String> = store_c
            .rows_for_sync(false)
            .unwrap()
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert!(titles.contains(&"A1".to_string()) && titles.contains(&"C1".to_string()));
    }

    // Idempotence + no double-counting: running `sync_round` twice back to
    // back with nothing new to push or pull is a no-op both times, and the
    // round's own just-pushed row (harmlessly re-fetched by the second,
    // pre-round-cursor pull) is never counted twice or duplicated locally.
    #[tokio::test]
    async fn sync_round_twice_with_nothing_new_is_idempotent_and_never_double_counts() {
        let addr = spawn_spelunk_server().await;
        let base_url = format!("http://{addr}");

        let (_tmp, store) = fresh_store();
        store
            .add_note("decision", "A1", "own entry", &[], &[], None, None)
            .unwrap();
        let client = CloudSyncClient::new(&base_url, "proj-idem", None, None).unwrap();

        let r1 = sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();
        assert_eq!(r1.pushed.created, 1);
        assert_eq!(
            r1.pulled, 0,
            "the second pull re-fetches this round's own just-pushed row via \
             the pre-round cursor, but it must not be double-counted"
        );
        assert_eq!(store.count().unwrap(), 1, "no duplicate local row");

        let r2 = sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();
        assert_eq!(
            (r2.pushed.attempted, r2.pushed.already_synced, r2.pulled),
            (0, 1, 0),
            "a second round with nothing new must be a full no-op"
        );
        assert_eq!(store.count().unwrap(), 1);
    }

    // The race window a plain reorder cannot close: a teammate's push
    // that lands on the server strictly between this round's own first pull
    // and its own push must still be picked up within this same round (via
    // the second pull, reusing the pre-round cursor) rather than being
    // permanently shadowed by the round's own push becoming the new
    // `MAX(remote_id)`.
    //
    // Real network concurrency can't be forced deterministically in a unit
    // test, so this composes `sync_round`'s exact same three calls
    // (`pull_and_apply_since` / `push_local` / `pull_and_apply_since`,
    // reusing one `pre_round_cursor`) with the teammate's push manually
    // interleaved at the precise point the race window occupies.
    #[tokio::test]
    async fn sync_round_catches_a_teammate_push_landing_between_its_own_pull_and_push() {
        let addr = spawn_spelunk_server().await;
        let base_url = format!("http://{addr}");

        let (_tmp, store) = fresh_store();
        store
            .add_note("decision", "Client1", "own new entry", &[], &[], None, None)
            .unwrap();
        let client = CloudSyncClient::new(&base_url, "proj-race", None, None).unwrap();

        // Step 1 of sync_round: capture the cursor, then pull. Nothing on the
        // server yet.
        let pre_round_cursor = store.max_remote_id().unwrap();
        let pulled_first = pull_and_apply_since(&store, &client, pre_round_cursor.as_deref())
            .await
            .unwrap();
        assert_eq!(pulled_first, 0);

        // The race window: a teammate pushes here, strictly between this
        // round's own pull and its own push.
        let (_tmp_b, store_b) = fresh_store();
        store_b
            .add_note(
                "decision",
                "B1",
                "teammate's race-window entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let client_b = CloudSyncClient::new(&base_url, "proj-race", None, None).unwrap();
        assert_eq!(
            push_local(&store_b, &client_b, false, false, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .created,
            1
        );

        // Step 2 of sync_round: this round's own push.
        let pushed = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();
        assert_eq!(pushed.created, 1);

        // Step 3 of sync_round: the second pull, reusing pre_round_cursor
        // (NOT a freshly re-derived max_remote_id(), which would now include
        // this round's own push and shadow B1 forever).
        let pulled_second = pull_and_apply_since(&store, &client, pre_round_cursor.as_deref())
            .await
            .unwrap();
        assert_eq!(
            pulled_second, 1,
            "the race-window teammate push must be caught by the second pull, \
             not permanently lost"
        );

        let titles: Vec<String> = store
            .rows_for_sync(false)
            .unwrap()
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert!(titles.contains(&"B1".to_string()));
    }

    // `memory pull` (one-way, no push) is unaffected by the `sync_round`
    // two-phase reconciliation added for `sync`. It keeps
    // deriving a single cursor from the store itself via `pull_and_apply`,
    // unmodified.
    #[tokio::test]
    async fn pull_and_apply_one_way_pull_still_derives_its_own_single_cursor() {
        let addr = spawn_spelunk_server().await;
        let base_url = format!("http://{addr}");

        let (_tmp_a, store_a) = fresh_store();
        store_a
            .add_note("decision", "A1", "first", &[], &[], None, None)
            .unwrap();
        let client_a = CloudSyncClient::new(&base_url, "proj-pull", None, None).unwrap();
        assert_eq!(
            push_local(&store_a, &client_a, false, false, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .created,
            1
        );
        store_a
            .add_note("decision", "A2", "second", &[], &[], None, None)
            .unwrap();
        assert_eq!(
            push_local(&store_a, &client_a, false, false, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .created,
            1
        );

        // A pull-only client with nothing local picks up both in one call.
        let (_tmp_c, store_c) = fresh_store();
        let client_c = CloudSyncClient::new(&base_url, "proj-pull", None, None).unwrap();
        let pulled = pull_and_apply(&store_c, &client_c).await.unwrap();
        assert_eq!(pulled, 2);

        // A second, immediate pull is a no-op (cursor re-derived from what
        // was just applied).
        let pulled_again = pull_and_apply(&store_c, &client_c).await.unwrap();
        assert_eq!(pulled_again, 0);
    }

    // ── first-sync regression: no pre-push pull against an unprovisioned
    // project ─────────────────────────────────────────────────────────────
    // A production regression hit on a project that had never synced to the
    // cloud before: the pre-push pull sent the project slug to an endpoint
    // that only accepts an already-resolved id, which fails until something
    // has provisioned the project. A first sync's own push is what
    // provisions it, so the pre-push pull on a first sync has nothing valid
    // to query yet.

    // Mock `/memory/since` responder that fails like the real bug (400)
    // until this project has been provisioned by a push, then succeeds:
    // models "the project does not exist yet" without depending on the real
    // cloud-api's own id-resolution details, which are out of scope for
    // this client-side fix.
    struct SinceUntilProvisioned {
        provisioned: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl wiremock::Respond for SinceUntilProvisioned {
        fn respond(&self, _request: &wiremock::Request) -> wiremock::ResponseTemplate {
            if self.provisioned.load(std::sync::atomic::Ordering::SeqCst) {
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "entries": [], "count": 0 }))
            } else {
                wiremock::ResponseTemplate::new(400)
                    .set_body_string("invalid project id: expected a UUID, got a slug")
            }
        }
    }

    // Mock `/memory/batch` responder that marks the project provisioned as
    // it lands the push, so a subsequent `/memory/since` call (via
    // `SinceUntilProvisioned`) succeeds.
    struct BatchProvisions {
        provisioned: std::sync::Arc<std::sync::atomic::AtomicBool>,
        external_id: String,
    }

    impl wiremock::Respond for BatchProvisions {
        fn respond(&self, _request: &wiremock::Request) -> wiremock::ResponseTemplate {
            self.provisioned
                .store(true, std::sync::atomic::Ordering::SeqCst);
            wiremock::ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": self.external_id, "id": "cloud-t1"}]
            }))
        }
    }

    // The exact repro: a never-pushed project's first sync must not send a
    // pre-push pull at all. `/memory/since` fails (400) for as long as the
    // project is unprovisioned, which is exactly the state a pre-push pull
    // would see; that the round still succeeds, with the entry pushed and
    // provisioned and no error surfaced, proves the pre-push pull was
    // skipped rather than happening to tolerate the error.
    #[tokio::test]
    async fn sync_round_first_sync_skips_the_pre_push_pull_and_succeeds() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let (_tmp, store) = fresh_store();
        store
            .add_note(
                "decision",
                "T1",
                "first entry, never synced",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let ext = store.rows_for_sync(false).unwrap()[0].uuid.clone();

        let provisioned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(SinceUntilProvisioned {
                provisioned: provisioned.clone(),
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(BatchProvisions {
                provisioned: provisioned.clone(),
                external_id: ext,
            })
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let outcome = sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .expect("a first sync must not surface the pre-push pull's 400 to the user");

        assert_eq!(outcome.pushed.created, 1, "the entry must be pushed");
        assert!(
            store.note_id_for_remote_id("cloud-t1").unwrap().is_some(),
            "the project must be provisioned and the entry stamped locally"
        );
    }

    // ── established-client regression: pull-before-push order unregressed ──

    // An established client (real sync history, a non-`None` cursor) must
    // keep the pull-before-push-before-pull order the first-sync branch
    // above does not use. Verified by observing the actual HTTP call
    // sequence rather than timing a real race: a first-sync round makes
    // exactly two calls (push, then one pull), while an established round
    // makes three (pull, push, pull), so seeing three calls in this exact
    // order, for a store seeded with a real cursor, proves the established
    // path was taken and its ordering is unchanged.
    #[tokio::test]
    async fn sync_round_established_client_keeps_pull_before_push_order() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let (_tmp, store) = fresh_store();
        // Seed a real cursor directly (as if an earlier sync had already
        // landed this row), without a real HTTP round trip.
        store
            .apply_remote_note(
                "01890000-0000-7000-8000-000000000001",
                "decision",
                "Seed",
                "seeds a real sync cursor",
                None,
                crate::storage::now_secs(),
                false,
            )
            .unwrap();
        assert!(
            store.max_remote_id().unwrap().is_some(),
            "the store must now be an established client"
        );
        store
            .add_note(
                "decision",
                "A2",
                "new local entry this round",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "entries": [], "count": 0 })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": "a2-ext", "id": "cloud-a2"}]
            })))
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();

        let reqs = server.received_requests().await.unwrap();
        let methods: Vec<&str> = reqs.iter().map(|r| r.method.as_str()).collect();
        assert_eq!(
            methods,
            vec!["GET", "POST", "GET"],
            "an established client must still pull, then push, then pull again: {methods:?}"
        );
    }

    // ── crash-before-record edge case: `max_remote_id()` reads only local
    // state, never the server ───────────────────────────────────────────────

    // `pre_round_cursor.is_none()` cannot distinguish "this project has never
    // been synced by anyone" from "this row was already pushed and the
    // server durably has it, but the local `remote_id` stamp never landed"
    // (a process crash between the server's 207 response and `push_local`'s
    // own `set_remote_id` write). `max_remote_id()` only ever reads the local
    // `notes` table (see its doc comment), so it cannot tell these apart. The
    // branch does not need to: a retry's push re-sends the same stable
    // `external_id`, the server dedupes and answers `skipped` rather than a
    // fresh `created`, and `push_local` stamps `remote_id` from a `skipped`
    // result exactly like a `created` one, so the store still converges to
    // established within this very round, without ever needing a pre-push
    // pull to have run.
    #[tokio::test]
    async fn sync_round_first_sync_recovers_a_push_that_landed_before_a_crash() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let (_tmp, store) = fresh_store();
        store
            .add_note(
                "decision",
                "T1",
                "pushed once, crashed before the local remote_id was recorded",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let ext = store.rows_for_sync(false).unwrap()[0].uuid.clone();
        assert!(
            store.max_remote_id().unwrap().is_none(),
            "the crash means this row's remote_id was never durably stamped locally"
        );

        let server = MockServer::start().await;
        // The project is already provisioned (the crashed run's push landed
        // server-side), so `/memory/since` succeeds unconditionally here,
        // unlike the never-provisioned case above.
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "entries": [], "count": 0 })),
            )
            .mount(&server)
            .await;
        // The retry's push re-sends the same external_id; the server already
        // has it, so it comes back `skipped` with the SAME cloud id it
        // minted the first time around, not a fresh `created`.
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 0, "skipped": 1, "failed": 0,
                "results": [{"status": "skipped", "external_id": ext, "id": "cloud-t1-preexisting"}]
            })))
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let outcome = sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .expect("a skipped-not-created push must not be treated as a failure");

        assert_eq!(
            outcome.pushed.skipped, 1,
            "the retry sees its own earlier push as already known to the server"
        );
        assert!(
            store
                .note_id_for_remote_id("cloud-t1-preexisting")
                .unwrap()
                .is_some(),
            "a skipped result must still stamp remote_id, recovering the crash-lost state"
        );
        assert!(
            store.max_remote_id().unwrap().is_some(),
            "the store must now be established, so the next sync takes the pull-push-pull path"
        );
    }

    // ── total push failure on a first sync: the follow-up pull's failure
    // must still surface cleanly, not panic or get silently swallowed ──────

    // When the push itself never lands (a transport-level failure on the
    // only chunk, not a per-item 4xx), nothing is provisioned server-side and
    // no `remote_id` is stamped. `sync_round_first` still runs its post-push
    // pull unconditionally, which then hits the exact never-provisioned 400
    // the original bug exhibited. That compound failure must come back as a
    // real `Err` naming the pull failure, not panic, not silently succeed,
    // and not leave the store looking established when nothing landed.
    #[tokio::test]
    async fn sync_round_first_sync_surfaces_pull_error_when_push_never_lands() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let (_tmp, store) = fresh_store();
        store
            .add_note("decision", "T1", "never lands", &[], &[], None, None)
            .unwrap();

        let server = MockServer::start().await;
        // The push chunk itself fails outright: nothing is provisioned and
        // no result item exists to stamp a remote_id from.
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        // The project is still unprovisioned, so the post-push pull gets the
        // same 400 a pre-push pull would have.
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string("invalid project id: expected a UUID, got a slug"),
            )
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let err = sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .expect_err(
                "a pull failure after a totally failed push must still surface as an error",
            );

        assert!(
            format!("{err:#}").contains("post-push pull failed"),
            "the error must carry the first-sync pull-failure context: {err:#}"
        );
        assert!(
            store.max_remote_id().unwrap().is_none(),
            "nothing landed, so a retry must still take the first-sync branch"
        );
    }

    // ── concurrent first syncs: two never-before-synced clients racing to
    // provision the same project must not trip the pre-push-pull bug for
    // either of them ─────────────────────────────────────────────────────

    // Mock `/memory/batch` responder that provisions the project and echoes
    // back a generated cloud id per pushed `external_id`, so two different
    // clients pushing two different entries concurrently each get a
    // coherent per-item result rather than a hardcoded single id.
    struct BatchProvisionsEcho {
        provisioned: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl wiremock::Respond for BatchProvisionsEcho {
        fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
            self.provisioned
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let entries = body["entries"].as_array().cloned().unwrap_or_default();
            let results: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    let ext = e["external_id"].as_str().unwrap_or_default();
                    serde_json::json!({
                        "status": "created", "external_id": ext, "id": format!("cloud-{ext}")
                    })
                })
                .collect();
            wiremock::ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": results.len(), "skipped": 0, "failed": 0, "results": results
            }))
        }
    }

    // Each client's own `sync_round_first` always pushes before it pulls, so
    // each one's own pull only ever runs after its own push has already set
    // `provisioned`; interleaving with the other client cannot reopen the
    // pre-push-pull window for either. This exercises that under genuine
    // concurrent execution (both rounds polled together via `tokio::join!`,
    // sharing one mock server and one `provisioned` flag) rather than
    // asserting it only holds sequentially.
    #[tokio::test]
    async fn sync_round_two_concurrent_first_syncs_both_succeed_without_pre_push_pull() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let (_tmp_x, store_x) = fresh_store();
        store_x
            .add_note(
                "decision",
                "X1",
                "client X's own new entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let (_tmp_y, store_y) = fresh_store();
        store_y
            .add_note(
                "decision",
                "Y1",
                "client Y's own new entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();

        let provisioned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj-race-first/memory/since"))
            .respond_with(SinceUntilProvisioned {
                provisioned: provisioned.clone(),
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj-race-first/memory/batch"))
            .respond_with(BatchProvisionsEcho {
                provisioned: provisioned.clone(),
            })
            .mount(&server)
            .await;

        let client_x = CloudSyncClient::new(&server.uri(), "proj-race-first", None, None).unwrap();
        let client_y = CloudSyncClient::new(&server.uri(), "proj-race-first", None, None).unwrap();

        let (outcome_x, outcome_y) = tokio::join!(
            sync_round(&store_x, &client_x, false, false, &LocalEmbedPolicy::Skip),
            sync_round(&store_y, &client_y, false, false, &LocalEmbedPolicy::Skip),
        );

        assert_eq!(
            outcome_x
                .expect("X's round must not surface the pre-push-pull 400")
                .pushed
                .created,
            1
        );
        assert_eq!(
            outcome_y
                .expect("Y's round must not surface the pre-push-pull 400")
                .pushed
                .created,
            1
        );
    }
}
