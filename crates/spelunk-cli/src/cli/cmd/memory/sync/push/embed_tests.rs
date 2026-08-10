// Pre-batch local-embedding repair tests for `super::push_local`: a pushed
// row must not be left invisible to semantic `memory search` locally.

use super::super::test_support::{fresh_store, spawn_loopback_embedder, stub_vector};
use super::*;
use crate::config::{Config, SyncMode};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// A team `server_url` that is deliberately never mocked: any accidental
// routing of the local embed to it surfaces as a connection error rather than
// a silent pass.
fn team_cfg() -> Config {
    Config {
        server_url: Some("https://cloud.invalid.example:1".to_string()),
        project_id: Some("proj".to_string()),
        mode: None,
        ..Default::default()
    }
}

fn add_note(store: &MemoryStore, title: &str, body: &str) -> (i64, String) {
    store
        .add_note("decision", title, body, &[], &[], None, None)
        .unwrap();
    let rows = store.rows_for_sync(true).unwrap();
    let row = rows.iter().find(|r| r.title == title).expect("note added");
    (row.local_id, row.uuid.clone())
}

async fn mount_batch_created(server: &MockServer, uuids: &[&str]) {
    let results: Vec<serde_json::Value> = uuids
        .iter()
        .enumerate()
        .map(|(i, u)| {
            serde_json::json!({"status": "created", "external_id": u, "id": format!("cloud-{i}")})
        })
        .collect();
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": uuids.len(), "skipped": 0, "failed": 0, "results": results
        })))
        .mount(server)
        .await;
}

fn embed_docs(reqs: &[wiremock::Request]) -> Vec<String> {
    reqs.iter()
        .filter(|r| r.url.path().ends_with("/index/embed"))
        .map(|r| {
            let json: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            json["chunks"][0]["content"].as_str().unwrap().to_string()
        })
        .collect()
}

#[tokio::test]
#[serial_test::serial]
async fn push_embeds_and_durably_stores_a_row_with_no_local_vector() {
    let loopback = spawn_loopback_embedder("proj", None).await;
    let (tmp, store) = fresh_store();
    let (id, uuid) = add_note(&store, "Unembedded", "first");
    assert!(store.get_embedding(id).unwrap().is_none());

    let team = MockServer::start().await;
    mount_batch_created(&team, &[&uuid]).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = team_cfg();
    assert_eq!(cfg.resolve_mode(), SyncMode::LocalFirst);
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
        (summary.embedded_locally, summary.without_local_vector),
        (1, 0)
    );
    // Durable: a store reopened from the same file still has the vector.
    drop(store);
    let reopened = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    let blob = reopened
        .get_embedding(id)
        .unwrap()
        .expect("push must leave the row embedded in memory.db");
    assert_eq!(
        spelunk_core::embeddings::blob_to_vec(&blob),
        stub_vector(),
        "the vector the local embedder returned must be what is stored"
    );
    drop(loopback);
}

#[tokio::test]
#[serial_test::serial]
async fn push_embeds_the_same_document_string_reindex_does() {
    let loopback = spawn_loopback_embedder("proj", None).await;
    let (tmp, store) = fresh_store();
    let (_id, uuid) = add_note(&store, "Cache policy", "we cache for 5m");

    let team = MockServer::start().await;
    mount_batch_created(&team, &[&uuid]).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = team_cfg();
    push_local(
        &store,
        &client,
        false,
        false,
        &LocalEmbedPolicy::for_push(&cfg, &tmp.path().join("memory.db")),
    )
    .await
    .unwrap();

    let docs = embed_docs(&loopback.server.received_requests().await.unwrap());
    // `memory reindex` embeds exactly this string (reindex.rs), document-side.
    // A query-side embed would carry F2LLM's `Instruct:/Query:` prefix and put
    // the row in a different space from every other note in the store.
    assert_eq!(docs, vec!["title: Cache policy | text: we cache for 5m"]);
    drop(loopback);
}

#[tokio::test]
#[serial_test::serial]
async fn push_does_not_re_embed_a_row_that_already_has_a_valid_vector() {
    let loopback = spawn_loopback_embedder("proj", None).await;
    let (tmp, store) = fresh_store();
    let (id_a, uuid_a) = add_note(&store, "Already", "embedded");
    let (id_b, uuid_b) = add_note(&store, "Also already", "embedded");
    for id in [id_a, id_b] {
        store
            .insert_embedding(id, &spelunk_core::embeddings::vec_to_blob(&stub_vector()))
            .unwrap();
    }

    let team = MockServer::start().await;
    mount_batch_created(&team, &[&uuid_a, &uuid_b]).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = team_cfg();
    let summary = push_local(
        &store,
        &client,
        false,
        false,
        &LocalEmbedPolicy::for_push(&cfg, &tmp.path().join("memory.db")),
    )
    .await
    .unwrap();

    assert_eq!(summary.embedded_locally, 0);
    // Not just "no embed call": no traffic at all. Resolving the embedder is
    // itself a discovery probe (`GET /v1/health`), so a push set that is
    // already fully embedded must never reach the resolver in the first place.
    // Asserting only on `/index/embed` would still pass if the client were
    // resolved eagerly and then went unused.
    assert!(
        loopback
            .server
            .received_requests()
            .await
            .unwrap()
            .is_empty(),
        "a fully embedded push set must not even probe for an embedder"
    );
    drop(loopback);
}

#[tokio::test]
#[serial_test::serial]
async fn locally_embedded_row_ships_its_vector_when_the_server_accepts_vectors() {
    let loopback = spawn_loopback_embedder("proj", None).await;
    let (tmp, store) = fresh_store();
    let (_id, uuid) = add_note(&store, "Unembedded", "first");

    let team = MockServer::start().await;
    mount_batch_created(&team, &[&uuid]).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = team_cfg();
    push_local(
        &store,
        &client,
        false,
        true,
        &LocalEmbedPolicy::for_push(&cfg, &tmp.path().join("memory.db")),
    )
    .await
    .unwrap();

    let reqs = team.received_requests().await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let entry = &json["entries"][0];
    assert_eq!(
        entry["vector"].as_array().map(Vec::len),
        Some(spelunk_core::embeddings::EMBEDDING_DIM),
        "the vector minted before the batch was built must reach the wire: {entry}"
    );
    assert_eq!(entry["vector_model"], "F2LLM-v2-330M");
    assert_eq!(entry["vector_precision"], "fp32");
    drop(loopback);
}

#[tokio::test]
#[serial_test::serial]
async fn local_vector_is_persisted_even_when_the_server_declines_vectors() {
    let loopback = spawn_loopback_embedder("proj", None).await;
    let (tmp, store) = fresh_store();
    let (id, uuid) = add_note(&store, "Unembedded", "first");

    let team = MockServer::start().await;
    mount_batch_created(&team, &[&uuid]).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = team_cfg();
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
    let body = String::from_utf8(reqs[0].body.clone()).unwrap();
    assert!(
        !body.contains("vector"),
        "a server without the capability must still get a text-only push: {body}"
    );
    assert!(
        store.get_embedding(id).unwrap().is_some(),
        "the local store must be repaired regardless of what the destination accepts"
    );
    drop(loopback);
}

#[tokio::test]
#[serial_test::serial]
async fn a_row_with_an_empty_body_still_embeds_and_pushes() {
    let loopback = spawn_loopback_embedder("proj", None).await;
    let (tmp, store) = fresh_store();
    let (id, uuid) = add_note(&store, "Title only", "");

    let team = MockServer::start().await;
    mount_batch_created(&team, &[&uuid]).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = team_cfg();
    let summary = push_local(
        &store,
        &client,
        false,
        false,
        &LocalEmbedPolicy::for_push(&cfg, &tmp.path().join("memory.db")),
    )
    .await
    .unwrap();

    assert_eq!(summary.embedded_locally, 1);
    assert_eq!(
        embed_docs(&loopback.server.received_requests().await.unwrap()),
        vec!["title: Title only | text: "],
        "an empty body must still produce the well-formed document string"
    );
    assert!(store.get_embedding(id).unwrap().is_some());
    let reqs = team.received_requests().await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert!(
        json["entries"][0].get("body").is_none_or(|b| b.is_null()),
        "an empty body stays off the wire, as before: {json}"
    );
    drop(loopback);
}

#[tokio::test]
#[serial_test::serial]
async fn a_second_push_run_issues_no_embed_calls() {
    let loopback = spawn_loopback_embedder("proj", None).await;
    let (tmp, store) = fresh_store();
    let (_id, uuid) = add_note(&store, "Unembedded", "first");

    let team = MockServer::start().await;
    mount_batch_created(&team, &[&uuid]).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();
    let cfg = team_cfg();
    let mem_path = tmp.path().join("memory.db");

    push_local(
        &store,
        &client,
        false,
        false,
        &LocalEmbedPolicy::for_push(&cfg, &mem_path),
    )
    .await
    .unwrap();
    let after_first = embed_docs(&loopback.server.received_requests().await.unwrap()).len();

    let second = push_local(
        &store,
        &client,
        false,
        false,
        &LocalEmbedPolicy::for_push(&cfg, &mem_path),
    )
    .await
    .unwrap();

    assert_eq!(after_first, 1);
    assert_eq!(second.embedded_locally, 0);
    assert_eq!(
        embed_docs(&loopback.server.received_requests().await.unwrap()).len(),
        after_first,
        "a re-run must not re-embed rows the first run already repaired"
    );
    drop(loopback);
}

#[tokio::test]
#[serial_test::serial]
async fn reindex_has_nothing_pending_for_rows_a_push_just_repaired() {
    let loopback = spawn_loopback_embedder("proj", None).await;
    let (tmp, store) = fresh_store();
    let (_id, uuid) = add_note(&store, "Unembedded", "first");
    assert_eq!(store.notes_missing_embeddings(false).unwrap().len(), 1);

    let team = MockServer::start().await;
    mount_batch_created(&team, &[&uuid]).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = team_cfg();
    push_local(
        &store,
        &client,
        false,
        false,
        &LocalEmbedPolicy::for_push(&cfg, &tmp.path().join("memory.db")),
    )
    .await
    .unwrap();

    // `memory reindex --dry-run` reports exactly this set as `would_embed`.
    assert!(
        store.notes_missing_embeddings(false).unwrap().is_empty(),
        "a pushed row must no longer be pending a reindex"
    );
    drop(loopback);
}

#[tokio::test]
#[serial_test::serial]
async fn vectors_minted_before_an_interrupted_chunk_stay_durable() {
    let loopback = spawn_loopback_embedder("proj", None).await;
    let (tmp, store) = fresh_store();
    let (id, _uuid) = add_note(&store, "Unembedded", "first");

    // The batch route is never mounted, so `push_batch` fails and the push
    // reports itself interrupted after the repair has already run.
    let team = MockServer::start().await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = team_cfg();
    let summary = push_local(
        &store,
        &client,
        false,
        false,
        &LocalEmbedPolicy::for_push(&cfg, &tmp.path().join("memory.db")),
    )
    .await
    .unwrap();

    assert!(
        summary.interrupted.is_some(),
        "the push must report a failure"
    );
    assert_eq!((summary.created, summary.skipped), (0, 0));
    drop(store);
    let reopened = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    assert!(
        reopened.get_embedding(id).unwrap().is_some(),
        "a vector minted before the failing chunk must survive it"
    );
    drop(loopback);
}
