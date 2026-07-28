// What the pre-batch local-embedding repair deliberately does NOT touch: rows
// outside the push set, and content handling it must leave alone.

use super::super::test_support::{fresh_store, spawn_loopback_embedder};
use super::*;
use crate::config::Config;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn team_cfg(server_url: String) -> Config {
    Config {
        server_url: Some(server_url),
        project_id: Some("proj".to_string()),
        mode: None,
        ..Default::default()
    }
}

async fn mount_batch_ok(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 0, "skipped": 0, "failed": 0, "results": []
        })))
        .mount(server)
        .await;
}

fn embed_count(reqs: &[wiremock::Request]) -> usize {
    reqs.iter()
        .filter(|r| r.url.path().ends_with("/index/embed"))
        .count()
}

#[tokio::test]
#[serial_test::serial]
async fn an_empty_push_set_makes_no_embed_calls() {
    let loopback = spawn_loopback_embedder("proj", None).await;
    let (tmp, store) = fresh_store();

    let team = MockServer::start().await;
    mount_batch_ok(&team).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = team_cfg(team.uri());
    let summary = push_local(
        &store,
        &client,
        false,
        false,
        &LocalEmbedPolicy::for_push(&cfg, &tmp.path().join("memory.db")),
    )
    .await
    .unwrap();

    assert_eq!(
        (summary.attempted, summary.already_synced),
        (0, 0),
        "today's nothing-to-push shape is unchanged"
    );
    assert_eq!(
        (summary.embedded_locally, summary.without_local_vector),
        (0, 0)
    );
    // Stronger than "no embed call": the discovery probe (`GET /v1/health`)
    // that resolving an embedder performs must not happen either, or an empty
    // push would pay for an embedder it never needed.
    assert!(
        loopback
            .server
            .received_requests()
            .await
            .unwrap()
            .is_empty(),
        "an empty push set must not even probe for an embedder"
    );
    assert!(
        team.received_requests().await.unwrap().is_empty(),
        "an empty push set sends nothing at all"
    );
    drop(loopback);
}

#[tokio::test]
#[serial_test::serial]
async fn already_synced_rows_are_left_unembedded() {
    let loopback = spawn_loopback_embedder("proj", None).await;
    let (tmp, store) = fresh_store();
    store
        .add_note("decision", "Synced", "body", &[], &[], None, None)
        .unwrap();
    let rows = store.rows_for_sync(false).unwrap();
    let id = rows[0].local_id;
    // Outside the push set: the cloud already has it. Repairing these rows is
    // the pull-side follow-up, deliberately not this change.
    store.set_remote_id(id, "cloud-1").unwrap();

    let team = MockServer::start().await;
    mount_batch_ok(&team).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = team_cfg(team.uri());
    let summary = push_local(
        &store,
        &client,
        false,
        false,
        &LocalEmbedPolicy::for_push(&cfg, &tmp.path().join("memory.db")),
    )
    .await
    .unwrap();

    assert_eq!((summary.attempted, summary.already_synced), (0, 1));
    assert_eq!(summary.embedded_locally, 0);
    assert_eq!(
        embed_count(&loopback.server.received_requests().await.unwrap()),
        0
    );
    assert!(store.get_embedding(id).unwrap().is_none());
    drop(loopback);
}

#[tokio::test]
#[serial_test::serial]
async fn archived_rows_are_not_embedded_but_still_tombstone() {
    let loopback = spawn_loopback_embedder("proj", None).await;
    let (tmp, store) = fresh_store();
    store
        .add_note("decision", "Gone", "body", &[], &[], None, None)
        .unwrap();
    let rows = store.rows_for_sync(false).unwrap();
    let id = rows[0].local_id;
    store.set_remote_id(id, "cloud-1").unwrap();
    store.archive(id).unwrap();

    let team = MockServer::start().await;
    mount_batch_ok(&team).await;
    Mock::given(method("DELETE"))
        .and(path("/v1/projects/proj/memory/cloud-1"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&team)
        .await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = team_cfg(team.uri());
    push_local(
        &store,
        &client,
        true,
        false,
        &LocalEmbedPolicy::for_push(&cfg, &tmp.path().join("memory.db")),
    )
    .await
    .unwrap();

    assert_eq!(
        embed_count(&loopback.server.received_requests().await.unwrap()),
        0,
        "an archived row is on its way out; embedding it is wasted work"
    );
    let team_paths: Vec<String> = team
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect();
    assert!(
        team_paths
            .iter()
            .any(|p| p == "DELETE /v1/projects/proj/memory/cloud-1"),
        "the tombstone must still propagate: {team_paths:?}"
    );
    assert!(store.get_embedding(id).unwrap().is_none());
    drop(loopback);
}

#[tokio::test]
#[serial_test::serial]
async fn vectors_land_in_the_store_that_was_pushed_not_the_project_default() {
    let loopback = spawn_loopback_embedder("proj", None).await;
    // Two stores in separate directories: only the one handed to the push (what
    // `--source <path>` selects) may be written to.
    let (source_tmp, source) = fresh_store();
    let (other_tmp, other) = fresh_store();
    source
        .add_note("decision", "Sourced", "body", &[], &[], None, None)
        .unwrap();
    other
        .add_note("decision", "Untouched", "body", &[], &[], None, None)
        .unwrap();
    let source_id = source.rows_for_sync(false).unwrap()[0].local_id;
    let other_id = other.rows_for_sync(false).unwrap()[0].local_id;

    let team = MockServer::start().await;
    mount_batch_ok(&team).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = team_cfg(team.uri());
    push_local(
        &source,
        &client,
        false,
        false,
        &LocalEmbedPolicy::for_push(&cfg, &source_tmp.path().join("memory.db")),
    )
    .await
    .unwrap();

    assert!(source.get_embedding(source_id).unwrap().is_some());
    assert!(
        other.get_embedding(other_id).unwrap().is_none(),
        "only the pushed store may be repaired"
    );
    drop(other_tmp);
    drop(loopback);
}

#[tokio::test]
#[serial_test::serial]
async fn the_repair_does_not_alter_or_re_screen_entry_content() {
    // The secret gate lives at `memory add` time. This repair reads the stored
    // title/body to build an embed document and writes back only a vector, so
    // the bytes on the wire are exactly what a pre-repair push sent. Regression
    // guard: no new pre-persistence scan requirement is introduced here.
    let loopback = spawn_loopback_embedder("proj", None).await;
    let (tmp, store) = fresh_store();
    let body = "token AKIAIOSFODNN7EXAMPLE stored by the user";
    store
        .add_note("decision", "Creds", body, &[], &[], None, None)
        .unwrap();

    let team = MockServer::start().await;
    mount_batch_ok(&team).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = team_cfg(team.uri());
    push_local(
        &store,
        &client,
        false,
        false,
        &LocalEmbedPolicy::for_push(&cfg, &tmp.path().join("memory.db")),
    )
    .await
    .unwrap();

    let reqs = team.received_requests().await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(
        json["entries"][0]["body"].as_str(),
        Some(body),
        "the repair must not redact, rewrite, or drop the entry it embeds"
    );
    drop(loopback);
}
