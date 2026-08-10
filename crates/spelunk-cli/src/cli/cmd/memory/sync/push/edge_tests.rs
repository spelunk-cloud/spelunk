// `push_local` must propagate local `relates_to` edges to the cloud once both
// endpoints have synced, via an edge-only `POST /memory/batch`, and must not
// re-post them on a later no-op sync.

use super::super::test_support::register_sqlite_vec;
use super::*;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

// Stand-in for the cloud batch route: echoes each pushed entry back as
// `created` (with a cloud id, so the row is stamped and enters `just_synced`),
// and acknowledges an edge-only batch as one `created` edge per element.
struct BatchEcho;

impl Respond for BatchEcho {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
        let entries = body["entries"].as_array().cloned().unwrap_or_default();
        if !entries.is_empty() {
            let results: Vec<Value> = entries
                .iter()
                .map(|e| {
                    let ext = e["external_id"].as_str().unwrap_or_default();
                    json!({"status": "created", "external_id": ext, "id": format!("cloud-{ext}")})
                })
                .collect();
            return ResponseTemplate::new(207).set_body_json(json!({
                "created": results.len(), "skipped": 0, "failed": 0, "results": results
            }));
        }
        let edges = body["edges"].as_array().cloned().unwrap_or_default();
        let acks: Vec<Value> = edges.iter().map(|_| json!({"status": "created"})).collect();
        ResponseTemplate::new(207).set_body_json(json!({ "edges": acks }))
    }
}

// Every edge-only `/memory/batch` body the server received (entries empty, at
// least one edge).
fn edge_batch_bodies(reqs: &[Request]) -> Vec<Value> {
    reqs.iter()
        .filter_map(|r| {
            let body: Value = serde_json::from_slice(&r.body).ok()?;
            let edges = body.get("edges")?.as_array()?;
            (!edges.is_empty()).then_some(body)
        })
        .collect()
}

#[tokio::test]
async fn push_local_propagates_a_relates_to_edge_keyed_by_external_id() {
    use tempfile::TempDir;

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    // `memory add --relates-to <target>` records a directed edge linker ->
    // target; mirror that shape here.
    let (target, _) = store
        .add_note("note", "Target", "target body", &[], &[], None, None)
        .unwrap();
    let (linker, _) = store
        .add_note("note", "Linker", "linker body", &[], &[], None, None)
        .unwrap();
    store.add_edge(linker, target, "relates_to").unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(BatchEcho)
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let summary = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!(summary.attempted, 2, "both entries must be pushed");
    assert_eq!(
        summary.edges_pushed, 1,
        "the relates_to edge must land once both endpoints synced in the same round"
    );

    // The push minted a stable uuid (the cloud external_id) for each endpoint.
    let from_ext = store.uuid_for(linker).unwrap().unwrap();
    let to_ext = store.uuid_for(target).unwrap().unwrap();

    let reqs = server.received_requests().await.unwrap();
    let bodies = edge_batch_bodies(&reqs);
    assert_eq!(
        bodies.len(),
        1,
        "exactly one edge-only batch must be posted"
    );
    assert_eq!(
        bodies[0]["entries"].as_array().map(Vec::len),
        Some(0),
        "an edge batch carries no entries"
    );
    let edge = &bodies[0]["edges"][0];
    assert_eq!(edge["kind"], "relates_to");
    assert_eq!(edge["from_external_id"], Value::String(from_ext));
    assert_eq!(edge["to_external_id"], Value::String(to_ext));
}

#[tokio::test]
async fn a_second_push_with_nothing_new_does_not_repost_the_edge() {
    use tempfile::TempDir;

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    let (target, _) = store
        .add_note("note", "Target", "target body", &[], &[], None, None)
        .unwrap();
    let (linker, _) = store
        .add_note("note", "Linker", "linker body", &[], &[], None, None)
        .unwrap();
    store.add_edge(linker, target, "relates_to").unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(BatchEcho)
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let first = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!(first.edges_pushed, 1);

    // Nothing new to push: no entry landed this round, so no edge is (re-)posted.
    let second = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!(second.attempted, 0, "no live entries remain");
    assert_eq!(
        second.edges_pushed, 0,
        "a settled edge must not be re-posted"
    );

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        edge_batch_bodies(&reqs).len(),
        1,
        "the edge must be posted exactly once across both pushes"
    );
}
