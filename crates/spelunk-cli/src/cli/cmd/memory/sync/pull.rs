//! Pull remote memory entries into the local store, paginating past the
//! server's per-page entry limit ([`CloudSyncClient::MEMORY_SINCE_PULL_LIMIT`]).

use anyhow::Result;

use crate::storage::{CloudSyncClient, MemoryStore};

/// Pull remote entries after the UUID cursor and apply them idempotently.
/// Returns the number of newly-inserted local rows.
///
/// The cursor is derived from the store itself — `MAX(remote_id)` over local
/// notes (decision #183) — so there is no persisted watermark to advance: the
/// next run re-derives the cursor from the rows just applied. This is what makes
/// the pull immune to clock drift and trivially resumable. Used as-is by the
/// one-way `memory pull`; `sync_round` below instead calls
/// [`pull_and_apply_since`] directly so it can pin an explicit cursor across
/// its two pull passes.
pub(super) async fn pull_and_apply(local: &MemoryStore, client: &CloudSyncClient) -> Result<usize> {
    let cursor = local.max_remote_id()?;
    pull_and_apply_since(local, client, cursor.as_deref()).await
}

/// Pull remote entries after an explicit `since_id` cursor and apply them
/// idempotently. Returns the number of newly-inserted local rows.
///
/// `pull_since` returns at most `CloudSyncClient::MEMORY_SINCE_PULL_LIMIT`
/// entries per call, so a backlog larger than one page requires more than
/// one request: this loops, applying each page as it arrives and advancing
/// the cursor to the last entry's `id`, until a page comes back shorter
/// than the requested limit (the definitive "nothing left" signal —
/// including the empty-page case for an already-fully-synced project).
/// Without this loop, a first sync into an established project would
/// silently apply only the first page and report success.
///
/// Applying an entry is idempotent regardless of how many times this is
/// called with overlapping ranges, or how many pages one call fetches:
/// [`MemoryStore::apply_remote_note`] dedupes on `remote_id` (or reuses a
/// matching row by `entity_id`), so re-fetching a row already known locally
/// is a harmless no-op, not a duplicate insert or a double count.
pub(super) async fn pull_and_apply_since(
    local: &MemoryStore,
    client: &CloudSyncClient,
    cursor: Option<&str>,
) -> Result<usize> {
    let mut cursor = cursor.map(str::to_string);
    let mut applied = 0usize;
    loop {
        let entries = client.pull_since(cursor.as_deref()).await?;
        let page_len = entries.len();

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

        if (page_len as i64) < CloudSyncClient::MEMORY_SINCE_PULL_LIMIT {
            break;
        }
        // A full page never proves it's the last one (the server never
        // returns more than the limit even when more remain), so always
        // follow up: entries is non-empty here (page_len == the limit, which
        // is > 0), so `last()` is always `Some`.
        cursor = entries.last().map(|e| e.id.clone());
    }
    Ok(applied)
}

/// Parse an ISO 8601 / RFC 3339 timestamp to Unix epoch seconds.
///
/// Falls back to "now" if the server sends a value we cannot parse, so a single
/// odd row never aborts the whole sync.
pub(in crate::cli::cmd::memory) fn parse_iso_to_secs(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|_| crate::storage::now_secs())
}

#[cfg(test)]
mod tests {
    use super::super::push::{LocalEmbedPolicy, push_local};
    use super::super::round::sync_round;
    use super::super::test_support::{fresh_store, register_sqlite_vec, spawn_spelunk_server};
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

    // ── real-server regression: an established client must keep pulling ────
    // A wiremock stand-in can't catch this class of bug: it lives in the real
    // server's own handler/db pairing (a batch-push ack echoing the raw row
    // id instead of the `sync_id` `/memory/since` cursors on), not in the wire
    // shape a hand-typed mock response can get right by construction. Spins
    // up the actual `spelunk-server` axum router, matching the pattern in
    // `outbox.rs`'s `spawn_spelunk_server`.

    // Reproduces the walk-the-store bug: a client that has already pushed
    // and synced once (an "established" client) never sees a teammate's
    // later entries on a subsequent sync, even though a fresh client would
    // pull the full set. Before the fix, client A's first push stamps its
    // own row's `remote_id` from the batch ack's `id` field, which the real
    // server (bug) fills with the raw autoincrement row id ("1", "2", ...)
    // instead of `sync_id`. That digit-string sorts lexically AFTER every
    // real UUIDv7 `sync_id` (which starts with a much smaller hex nibble for
    // any current timestamp), so `max_remote_id()`'s cursor becomes that row
    // id and `since_id=<cursor>` on the second pull matches nothing, even
    // though the server holds teammate B's newer entry.
    #[tokio::test]
    async fn established_client_pulls_teammates_entries_added_after_its_first_sync() {
        register_sqlite_vec();
        let addr = spawn_spelunk_server().await;
        let base_url = format!("http://{addr}");

        // Client A: first sync. Pushes its own entry, then pulls (nothing new
        // yet): this is what "establishes" the client and stamps its remote_id.
        let tmp_a = tempfile::TempDir::new().unwrap();
        let store_a = MemoryStore::open(&tmp_a.path().join("memory.db")).unwrap();
        store_a
            .add_note(
                "decision",
                "A1",
                "client A's own entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let client_a = CloudSyncClient::new(&base_url, "proj", None, None).unwrap();

        let push1 = push_local(&store_a, &client_a, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();
        assert_eq!(
            push1.created, 1,
            "client A's own entry must land on the server"
        );
        let pull1 = pull_and_apply(&store_a, &client_a).await.unwrap();
        assert_eq!(pull1, 0, "nothing new on the server yet for the first pull");

        // Teammate B: a second, independent client pushes a new entry to the
        // same server/project.
        let tmp_b = tempfile::TempDir::new().unwrap();
        let store_b = MemoryStore::open(&tmp_b.path().join("memory.db")).unwrap();
        store_b
            .add_note("decision", "B1", "teammate B's entry", &[], &[], None, None)
            .unwrap();
        let client_b = CloudSyncClient::new(&base_url, "proj", None, None).unwrap();
        let push_b = push_local(&store_b, &client_b, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();
        assert_eq!(
            push_b.created, 1,
            "teammate B's entry must land on the server"
        );

        // Client A syncs again: it is now an established client (already has a
        // remote_id-stamped row), exactly the steady-state "sync to get
        // teammates' latest" case the bug report describes.
        let pull2 = pull_and_apply(&store_a, &client_a).await.unwrap();
        assert_eq!(
            pull2, 1,
            "an established client must still pull entries a teammate pushed afterward"
        );
        let titles: Vec<String> = store_a
            .rows_for_sync(false)
            .unwrap()
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert!(
            titles.contains(&"B1".to_string()),
            "client A must now have teammate B's entry locally: {titles:?}"
        );
    }

    // Three-way steady state, each established client syncing across
    // multiple rounds: rules out an off-by-one in cursor advancement that a
    // two-client, single-extra-round test (see above) could miss (e.g. a
    // cursor that only "catches up" once and then drifts on a later round).
    //
    // Client C joins second, via a pull-only first sync (see the doc
    // comment on `store_c`'s setup below for why: joining via push+pull in
    // one round is a separate, real ordering issue, not this story's bug).
    // After that, both A and C are established clients holding a cursor
    // derived purely from pulls/pushes of their own already-caught-up
    // state, and teammate B pushes twice, in two separate rounds. Each of A
    // and C must pick up exactly the right delta on each of their own
    // subsequent pulls: never 0 (the bug this story fixes), never a
    // duplicate re-application, and never the other established client's
    // own entries re-surfacing.
    #[tokio::test]
    async fn two_established_clients_each_pull_correctly_across_multiple_rounds() {
        register_sqlite_vec();
        let addr = spawn_spelunk_server().await;
        let base_url = format!("http://{addr}");

        // Client A establishes: push A1, pull (nothing yet).
        let tmp_a = tempfile::TempDir::new().unwrap();
        let store_a = MemoryStore::open(&tmp_a.path().join("memory.db")).unwrap();
        store_a
            .add_note("decision", "A1", "client A's entry", &[], &[], None, None)
            .unwrap();
        let client_a = CloudSyncClient::new(&base_url, "proj3", None, None).unwrap();
        assert_eq!(
            push_local(&store_a, &client_a, false, false, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .created,
            1
        );
        assert_eq!(pull_and_apply(&store_a, &client_a).await.unwrap(), 0);

        // Client C joins with no local content yet, so its first sync is a
        // pull only (matching the walk-the-store "fresh client" case, which
        // is known-good). This deliberately avoids a SEPARATE, real ordering
        // issue that is out of scope for this story: `memory_sync` pushes
        // before it pulls, so a client that pushes brand-new local content
        // in the same round as older, not-yet-pulled remote content stamps
        // its own freshly-minted (and therefore chronologically newest)
        // sync_id as its cursor, permanently shadowing that older remote
        // content from every future pull. Filed separately; see this task's
        // board comment.
        let tmp_c = tempfile::TempDir::new().unwrap();
        let store_c = MemoryStore::open(&tmp_c.path().join("memory.db")).unwrap();
        let client_c = CloudSyncClient::new(&base_url, "proj3", None, None).unwrap();
        let pull_c1 = pull_and_apply(&store_c, &client_c).await.unwrap();
        assert_eq!(pull_c1, 1, "client C must pull client A's A1 on establish");

        // Now that C is caught up (nothing outstanding to miss), it can
        // safely push its own new entry in the same round it pulls.
        store_c
            .add_note("decision", "C1", "client C's entry", &[], &[], None, None)
            .unwrap();
        assert_eq!(
            push_local(&store_c, &client_c, false, false, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .created,
            1
        );
        assert_eq!(
            pull_and_apply(&store_c, &client_c).await.unwrap(),
            0,
            "nothing further for C to pull immediately after its own push"
        );

        // Teammate B pushes its first entry.
        let tmp_b = tempfile::TempDir::new().unwrap();
        let store_b = MemoryStore::open(&tmp_b.path().join("memory.db")).unwrap();
        store_b
            .add_note(
                "decision",
                "B1",
                "teammate B's first entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let client_b = CloudSyncClient::new(&base_url, "proj3", None, None).unwrap();
        assert_eq!(
            push_local(&store_b, &client_b, false, false, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .created,
            1
        );

        // Round 2: both established clients must pick up exactly the new
        // delta each is missing (A is missing C1 and B1; C is missing B1).
        let pull_a_round2 = pull_and_apply(&store_a, &client_a).await.unwrap();
        assert_eq!(
            pull_a_round2, 2,
            "client A must pull both C1 and B1 on its second sync"
        );
        let pull_c_round2 = pull_and_apply(&store_c, &client_c).await.unwrap();
        assert_eq!(
            pull_c_round2, 1,
            "client C must pull only B1 (it already has A1 and its own C1)"
        );

        // Teammate B pushes a second entry. This is the off-by-one probe:
        // a cursor that only advances correctly ONCE (round 2) but then
        // sticks or drifts would surface here as 0 or a re-applied dup on
        // this THIRD round for either established client.
        store_b
            .add_note(
                "decision",
                "B2",
                "teammate B's second entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            push_local(&store_b, &client_b, false, false, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .created,
            1
        );

        let pull_a_round3 = pull_and_apply(&store_a, &client_a).await.unwrap();
        assert_eq!(
            pull_a_round3, 1,
            "client A's cursor must advance correctly again on a third round"
        );
        let pull_c_round3 = pull_and_apply(&store_c, &client_c).await.unwrap();
        assert_eq!(
            pull_c_round3, 1,
            "client C's cursor must advance correctly again on a third round"
        );

        let titles_a: Vec<String> = store_a
            .rows_for_sync(false)
            .unwrap()
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert!(
            ["A1", "C1", "B1", "B2"]
                .iter()
                .all(|t| titles_a.contains(&t.to_string())),
            "client A must end up with all four entries exactly once each: {titles_a:?}"
        );
        let titles_c: Vec<String> = store_c
            .rows_for_sync(false)
            .unwrap()
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert!(
            ["A1", "C1", "B1", "B2"]
                .iter()
                .all(|t| titles_c.contains(&t.to_string())),
            "client C must end up with all four entries exactly once each: {titles_c:?}"
        );
    }

    // ── pull pagination: exhaust every page, not just the first ─────────────
    // Before this fix, `pull_and_apply_since` made exactly one request to
    // `pull_since` and treated whatever it returned as the whole backlog. This
    // client requests `CloudSyncClient::MEMORY_SINCE_PULL_LIMIT` entries per
    // page, so a first sync into an established project silently applied
    // only the first page and reported success. These tests drive the fix
    // directly against a mock `/memory/since`, controlling exact page sizes
    // (including a full page at that request limit) without needing that
    // many real rows in a live server.

    // Deterministic, lexically-increasing ids so `since_id` cursors compare
    // the same way real UUIDv7 cloud ids do.
    fn page_ids(start: usize, count: usize) -> Vec<String> {
        (start..start + count)
            .map(|i| format!("01890000-0000-7000-8000-{i:012x}"))
            .collect()
    }

    fn entries_json(ids: &[String]) -> serde_json::Value {
        let entries: Vec<_> = ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "id": id,
                    "kind": "note",
                    "title": format!("T-{id}"),
                    "body": "b",
                    "created_at": "2026-06-19T01:00:00Z",
                })
            })
            .collect();
        serde_json::json!({ "entries": entries, "count": entries.len() })
    }

    const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";

    // Mounts one `/memory/since` mock per page, matched by the exact
    // `since_id` it must be requested with (the prior page's last id, or the
    // nil UUID for the very first request). Each mock must be hit exactly
    // `times` times.
    async fn mount_pages_times(server: &wiremock::MockServer, pages: &[Vec<String>], times: u64) {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, ResponseTemplate};

        let mut cursor = NIL_UUID.to_string();
        for ids in pages {
            Mock::given(method("GET"))
                .and(path("/v1/projects/proj/memory/since"))
                .and(query_param("since_id", cursor.clone()))
                .respond_with(ResponseTemplate::new(200).set_body_json(entries_json(ids)))
                .expect(times)
                .mount(server)
                .await;
            if let Some(last) = ids.last() {
                cursor = last.clone();
            }
        }
    }

    async fn mount_pages(server: &wiremock::MockServer, pages: &[Vec<String>]) {
        mount_pages_times(server, pages, 1).await;
    }

    // Item 1: a backlog smaller than one page is a single request, and every
    // entry lands.
    #[tokio::test]
    async fn pull_and_apply_since_single_page_matches_prior_behavior() {
        let server = wiremock::MockServer::start().await;
        let page = page_ids(0, 40);
        mount_pages(&server, &[page]).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply_since(&store, &client, None).await.unwrap();

        assert_eq!(applied, 40);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    // Item 2: a backlog spanning exactly two pages (100 then 40, the
    // requested page limit) is fetched in two requests, the second cursor is
    // the first page's last id, and every entry is applied exactly once.
    #[tokio::test]
    async fn pull_and_apply_since_two_pages_advances_cursor_to_last_id_of_prior_page() {
        let server = wiremock::MockServer::start().await;
        let page1 = page_ids(0, 100);
        let page2 = page_ids(100, 40);
        mount_pages(&server, &[page1.clone(), page2.clone()]).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply_since(&store, &client, None).await.unwrap();

        assert_eq!(applied, 140);
        assert_eq!(store.count().unwrap(), 140, "no duplicates applied");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    // Item 3: three-plus pages (100 + 100 + 45) keep looping past two
    // iterations, not just handling the two-page case.
    #[tokio::test]
    async fn pull_and_apply_since_three_pages_loops_past_two_iterations() {
        let server = wiremock::MockServer::start().await;
        let page1 = page_ids(0, 100);
        let page2 = page_ids(100, 100);
        let page3 = page_ids(200, 45);
        mount_pages(&server, &[page1, page2, page3]).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply_since(&store, &client, None).await.unwrap();

        assert_eq!(applied, 245);
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    // Item 4: a page landing exactly on the limit is always followed by
    // exactly one more request (never assumed to be the last page), and
    // that follow-up returning short ends the loop immediately (never a
    // third, unnecessary request).
    #[tokio::test]
    async fn pull_and_apply_since_full_page_triggers_exactly_one_more_request() {
        let server = wiremock::MockServer::start().await;
        let page1 = page_ids(0, 100);
        let page2: Vec<String> = vec![];
        mount_pages(&server, &[page1, page2]).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply_since(&store, &client, None).await.unwrap();

        assert_eq!(applied, 100);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    // Item 5: an already fully-synced project (empty first page) terminates
    // after that one request instead of looping forever.
    #[tokio::test]
    async fn pull_and_apply_since_empty_first_page_terminates_after_one_request() {
        let server = wiremock::MockServer::start().await;
        mount_pages(&server, &[vec![]]).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply_since(&store, &client, None).await.unwrap();

        assert_eq!(applied, 0);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    // Item 6: `sync_round`'s reported `pulled` count (what `spelunk sync`'s
    // completion message prints) is the TRUE total across every page, not
    // just the first. `memory_sync` itself can't be driven directly in this
    // binary (see `sync_round`'s own doc comment on the per-process tier
    // cache), so this asserts on the exact value the message interpolates.
    #[tokio::test]
    async fn sync_round_pulled_count_reflects_every_page_not_just_the_first() {
        let server = wiremock::MockServer::start().await;
        // sync_round's own first pull pass sees the whole multi-page backlog
        // (nothing local to push, so the push call is empty and the second
        // pull pass, reusing the same pre-round cursor, will be a no-op
        // repeat of the same now-caught-up query).
        let page1 = page_ids(0, 100);
        let page2 = page_ids(100, 60);
        {
            use wiremock::matchers::{method, path, query_param};
            use wiremock::{Mock, ResponseTemplate};
            // No `expect(1)`: the confirmation pull re-derives from the SAME
            // pre-round cursor (nil UUID, since nothing local existed before
            // this round), so it legitimately repeats this identical
            // two-page sequence a second time.
            Mock::given(method("GET"))
                .and(path("/v1/projects/proj/memory/since"))
                .and(query_param("since_id", NIL_UUID))
                .respond_with(ResponseTemplate::new(200).set_body_json(entries_json(&page1)))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/v1/projects/proj/memory/since"))
                .and(query_param("since_id", page1.last().unwrap().clone()))
                .respond_with(ResponseTemplate::new(200).set_body_json(entries_json(&page2)))
                .mount(&server)
                .await;
        }

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let outcome = sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();

        assert_eq!(
            outcome.pulled, 160,
            "both pull passes re-fetch the same 160-entry backlog off the \
             unchanged pre-round cursor; apply_remote_note's dedupe means the \
             SECOND pass applies 0 new rows, so pulled must be exactly the \
             true total, not double-counted nor short of the second page"
        );
        assert_eq!(store.count().unwrap(), 160);
    }

    // Item 7: the one-way `spelunk memory pull` entry point (`pull_and_apply`,
    // which derives its own cursor from the store rather than being handed
    // one) also paginates fully, proven through `pull_and_apply` directly,
    // not just through `sync_round`, since both merely wrap the same shared
    // `pull_and_apply_since`.
    #[tokio::test]
    async fn pull_and_apply_one_way_also_paginates_fully() {
        let server = wiremock::MockServer::start().await;
        let page1 = page_ids(0, 100);
        let page2 = page_ids(100, 45);
        mount_pages(&server, &[page1, page2]).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply(&store, &client).await.unwrap();

        assert_eq!(applied, 145);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    // Item 8: on a first sync (nothing local has ever pushed or pulled
    // before, so `sync_round` pushes first and runs a single post-push
    // pull), that post-push pull paginates fully on its own when it turns
    // up more than one page of results (e.g. a teammate already has a
    // 130-entry backlog on the project by the time this round's push
    // provisions it and the pull runs).
    #[tokio::test]
    async fn sync_round_first_sync_post_push_pull_paginates_fully_on_its_own() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        // This round's own push (empty local store: nothing to push).
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 0, "skipped": 0, "failed": 0, "results": []
            })))
            .mount(&server)
            .await;
        // The single post-push pull starts from the nil UUID (no prior sync
        // history) and must exhaust both pages.
        let page1 = page_ids(0, 100);
        let page2 = page_ids(100, 30);
        mount_pages(&server, &[page1, page2]).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let outcome = sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();

        assert_eq!(outcome.pushed.attempted, 0);
        assert_eq!(
            outcome.pulled, 130,
            "the post-push pull on a first sync must exhaust its own pagination too"
        );
        assert_eq!(store.count().unwrap(), 130);
    }

    // Item 9: re-running a pull after an interrupted/partial prior run
    // (some entries already applied locally from an earlier page) must not
    // double-count `applied` for entries seen again: regression-guards the
    // existing dedupe-by-`remote_id` specifically under the new loop, since
    // a naive re-implementation could re-tally a page it had already
    // applied once before.
    #[tokio::test]
    async fn pull_and_apply_since_rerun_after_partial_prior_run_does_not_double_count() {
        let server = wiremock::MockServer::start().await;
        let page1 = page_ids(0, 100);
        let page2 = page_ids(100, 40);
        // Each page is legitimately re-requested once per run below.
        mount_pages_times(&server, &[page1.clone(), page2.clone()], 2).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let first_run = pull_and_apply_since(&store, &client, None).await.unwrap();
        assert_eq!(first_run, 140);

        // Re-running from the same (still `None`, un-advanced) cursor
        // re-fetches the identical two pages; every entry is already known
        // by `remote_id`, so nothing should be counted or inserted twice.
        let rerun = pull_and_apply_since(&store, &client, None).await.unwrap();
        assert_eq!(rerun, 0, "already-applied entries must not be re-counted");
        assert_eq!(store.count().unwrap(), 140, "and never re-inserted");
    }

    // Item 10a: a project not yet created on the server (404 on the very
    // first page request) still applies 0 and returns success, not an
    // error, unchanged by the new loop.
    #[tokio::test]
    async fn pull_and_apply_since_404_on_first_page_is_still_zero_not_an_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply_since(&store, &client, None).await.unwrap();

        assert_eq!(applied, 0);
    }

    // Item 10b: a `None` cursor still starts the loop's first request from
    // the nil UUID (full catch-up), unchanged by the new loop wrapping the
    // cursor for its own subsequent iterations.
    #[tokio::test]
    async fn pull_and_apply_since_none_cursor_still_starts_at_nil_uuid() {
        let server = wiremock::MockServer::start().await;
        mount_pages(&server, &[page_ids(0, 5)]).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply_since(&store, &client, None).await.unwrap();

        assert_eq!(applied, 5);
    }

    // If the post-push pull on a first sync (nothing local has ever pushed
    // or pulled before, so `sync_round` pushes first and runs a single pull
    // afterward) fails, `sync_round` must not silently swallow that error,
    // but it also must not lose or misrepresent the push that already
    // succeeded. The push already durably landed server-side and already
    // stamped this round's row with its `remote_id` (inside `push_local`,
    // which returns before the pull ever runs), so: (1) the error surfaces
    // instead of being dropped, (2) its message says the push already
    // reached the server rather than reading as "nothing happened", and
    // (3) local state is left exactly as `push_local` left it: no
    // corruption, no re-attempt needed, just a retryable pull on the next
    // sync.
    #[tokio::test]
    async fn sync_round_first_sync_post_push_pull_failure_surfaces_the_error_without_losing_the_push()
     {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let (_tmp, store) = fresh_store();
        store
            .add_note("decision", "T1", "own new entry", &[], &[], None, None)
            .unwrap();
        let ext = store.rows_for_sync(false).unwrap()[0].uuid.clone();
        let cloud_id = "01890000-0000-7000-8000-0000000000b1";

        let server = MockServer::start().await;
        // The push itself succeeds and durably lands, provisioning the
        // project.
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": ext, "id": cloud_id}]
            })))
            .mount(&server)
            .await;
        // The single post-push pull hits a transient server error.
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let err = sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .expect_err("a real post-push pull error must not be swallowed as success");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("push already reached the server") && msg.contains("1 created"),
            "error must say the push already succeeded, not read as a total \
             failure: {msg}"
        );

        // The push's own effect is untouched by the later pull error: the row
        // is stamped with its cloud id, exactly as `push_local` left it.
        assert!(
            store.note_id_for_remote_id(cloud_id).unwrap().is_some(),
            "the already-succeeded push must not be undone or left unstamped \
             just because the confirmation pull afterward failed"
        );
        assert_eq!(
            store.count().unwrap(),
            1,
            "no duplicate/corrupted local row"
        );
    }

    // A network/server failure on a LATER page (not the first) must not
    // corrupt or silently drop the pages that already succeeded: the loop
    // applies each page as it arrives, so a first-page success is durably
    // in the local store even though the overall call returns `Err`. A
    // naive rewrite (e.g. buffering all pages before applying any) would
    // instead lose the first page's entries when a later page fails.
    #[tokio::test]
    async fn pull_and_apply_since_error_on_a_later_page_keeps_earlier_pages_applied_and_is_retryable()
     {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Page 1 must be a FULL page (== MEMORY_SINCE_PULL_LIMIT): a short
        // page 1 would already be the natural last page and the loop would
        // never even attempt a second request, defeating the point of this
        // test.
        let page1 = page_ids(0, 100);
        let page2 = page_ids(100, 5);

        // ── Run 1: page 1 succeeds and applies, page 2 fails (500). ──
        let server1 = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .and(query_param("since_id", NIL_UUID))
            .respond_with(ResponseTemplate::new(200).set_body_json(entries_json(&page1)))
            .expect(1)
            .mount(&server1)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .and(query_param("since_id", page1.last().unwrap().clone()))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server1)
            .await;

        let (_tmp, store) = fresh_store();
        let client1 = CloudSyncClient::new(&server1.uri(), "proj", None, None).unwrap();
        pull_and_apply_since(&store, &client1, None)
            .await
            .expect_err("a later-page failure must surface as Err, not a silent partial success");

        assert_eq!(
            store.count().unwrap(),
            100,
            "page 1's entries must already be durably applied even though the \
             overall call failed on page 2"
        );
        assert_eq!(
            store.max_remote_id().unwrap().as_deref(),
            Some(page1.last().unwrap().as_str()),
            "the store-derived cursor reflects exactly the pages that landed, \
             so a retry resumes from the right place"
        );

        // ── Run 2: a healthy server serves the remainder from the re-derived
        // cursor; nothing from page 1 is re-applied or duplicated. ──
        let server2 = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .and(query_param("since_id", page1.last().unwrap().clone()))
            .respond_with(ResponseTemplate::new(200).set_body_json(entries_json(&page2)))
            .expect(1)
            .mount(&server2)
            .await;
        let client2 = CloudSyncClient::new(&server2.uri(), "proj", None, None).unwrap();

        let cursor = store.max_remote_id().unwrap();
        let applied = pull_and_apply_since(&store, &client2, cursor.as_deref())
            .await
            .unwrap();
        assert_eq!(applied, 5, "the retry applies exactly the remainder");
        assert_eq!(
            store.count().unwrap(),
            105,
            "no duplicates from re-fetching across the two runs"
        );
    }

    // A page whose entries fail to deserialize (here: an entry missing the
    // required `id` field, the value pagination advances the cursor from)
    // must fail the WHOLE page atomically rather than partially applying
    // the entries that happened to parse fine. `SinceBody`/`RemoteEntry`
    // deserialize the full response body before `pull_and_apply_since` ever
    // sees a single entry, so this is really a regression guard: a future
    // change that streamed/parsed entries one at a time could silently
    // apply a prefix before hitting the bad entry.
    #[tokio::test]
    async fn pull_and_apply_since_malformed_entry_missing_id_fails_the_page_atomically() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let malformed = serde_json::json!({
            "entries": [
                {
                    "kind": "note",
                    "title": "no id field at all",
                    "body": "b",
                    "created_at": "2026-06-19T01:00:00Z"
                }
            ],
            "count": 1
        });
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(ResponseTemplate::new(200).set_body_json(malformed))
            .expect(1)
            .mount(&server)
            .await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        pull_and_apply_since(&store, &client, None)
            .await
            .expect_err("a page that fails to parse must surface as Err");

        assert_eq!(
            store.count().unwrap(),
            0,
            "nothing from an unparseable page may be partially applied"
        );
    }

    // The server's `count` field is documented (see `pull_since`'s wire
    // comment) as redundant with `entries.len()`, never a "more remain"
    // signal, so the loop's termination must be driven by the actual
    // number of entries returned, not by trusting `count`. A server (or a
    // test double) that reports an inflated `count` alongside a genuinely
    // short `entries` array must still be treated as the last page.
    #[tokio::test]
    async fn pull_and_apply_since_terminates_on_actual_entries_len_not_a_lying_count_field() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let ids = page_ids(0, 50);
        let mut body = entries_json(&ids);
        // `entries.len() == 50` (well short of MEMORY_SINCE_PULL_LIMIT), but `count`
        // falsely claims far more remain. If the loop ever keyed off `count`
        // instead of the real page length, this would spin into a second,
        // unexpected request against a server with nothing left to mount.
        body["count"] = serde_json::json!(9999);
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply_since(&store, &client, None).await.unwrap();

        assert_eq!(applied, 50);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "a lying count must not trigger a second request past the real \
             short page"
        );
    }
}
