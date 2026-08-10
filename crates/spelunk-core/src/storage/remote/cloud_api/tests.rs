use serde_json::{Value, json};
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;

const UUID: &str = "9f1c2d3e-4a5b-6c7d-8e9f-0a1b2c3d4e5f";
const UUID_2: &str = "1a2b3c4d-5e6f-7a8b-9c0d-1e2f3a4b5c6d";
// Shared fixture project id; `slug_backend` below exercises the slug case
// separately for every route, including the two that once rejected it.
const PROJECT: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";

fn backend(server: &MockServer) -> CloudApiMemoryBackend {
    CloudApiMemoryBackend {
        client: reqwest::Client::builder().build().unwrap(),
        base_url: server.uri(),
        project_id: PROJECT.to_string(),
        api_key: Some("token".to_string()),
    }
}

fn entry(id: &str, title: &str) -> Value {
    json!({
        "id": id,
        "kind": "decision",
        "title": title,
        "body": "b",
        "external_id": format!("ext-{id}"),
        "created_at": "2026-06-19T01:00:00Z",
    })
}

fn note_input(title: &str) -> NoteInput {
    NoteInput {
        kind: "decision".to_string(),
        title: title.to_string(),
        body: "b".to_string(),
        tags: vec![],
        linked_files: vec![],
        embedding: None,
        source_ref: None,
        valid_at: None,
        supersedes: None,
    }
}

// ── add ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn add_posts_to_the_memory_route_and_returns_the_server_uuid() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT}/memory")))
        .respond_with(ResponseTemplate::new(201).set_body_json(entry(UUID, "t")))
        .mount(&server)
        .await;

    let (id, created) = backend(&server).add(note_input("t")).await.unwrap();
    assert_eq!(id, UUID.parse::<NoteId>().unwrap());
    assert!(created);
}

// The batch edge route addresses entries only by `external_id`, and the key
// cannot be assigned after the fact, so every add must carry one.
#[tokio::test]
async fn add_always_mints_an_external_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT}/memory")))
        .respond_with(ResponseTemplate::new(201).set_body_json(entry(UUID, "t")))
        .mount(&server)
        .await;

    backend(&server).add(note_input("t")).await.unwrap();

    let req = &server.received_requests().await.unwrap()[0];
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    let ext = body["external_id"].as_str().expect("external_id sent");
    assert!(
        uuid::Uuid::parse_str(ext).is_ok(),
        "external_id must be a UUID, got {ext}"
    );
}

#[tokio::test]
async fn a_conflict_on_add_is_a_warning_not_a_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT}/memory")))
        .respond_with(ResponseTemplate::new(409).set_body_json(entry(UUID, "t")))
        .mount(&server)
        .await;

    let (id, _) = backend(&server).add(note_input("t")).await.unwrap();
    assert_eq!(id, UUID.parse::<NoteId>().unwrap());
}

// ── list / search / count ────────────────────────────────────────────────────

#[tokio::test]
async fn list_reads_the_memory_route_without_a_query() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/{PROJECT}/memory")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"entries": [entry(UUID, "one")], "total": 1})),
        )
        .mount(&server)
        .await;

    let notes = backend(&server).list(None, 10, false, None).await.unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "one");
    assert_eq!(notes[0].id, UUID.parse::<NoteId>().unwrap());

    let req = &server.received_requests().await.unwrap()[0];
    assert!(
        !req.url.query_pairs().any(|(k, _)| k == "q"),
        "list must not send a search query"
    );
}

#[tokio::test]
async fn an_empty_project_lists_cleanly() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/{PROJECT}/memory")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"entries": [], "total": 0})))
        .mount(&server)
        .await;

    assert!(
        backend(&server)
            .list(None, 10, false, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(backend(&server).count().await.unwrap(), 0);
}

// Search is the list route with `q`, not the team server's `POST memory/search`.
#[tokio::test]
async fn search_uses_the_query_parameter_on_the_list_route() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/{PROJECT}/memory")))
        .and(query_param("q", "sqlite"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"entries": [entry(UUID, "hit")], "total": 1})),
        )
        .mount(&server)
        .await;

    let notes = backend(&server)
        .search(&[], "sqlite", 5, None)
        .await
        .unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "hit");
}

#[tokio::test]
async fn bm25_text_search_keeps_the_shared_remote_message() {
    let server = MockServer::start().await;
    let err = backend(&server)
        .search_text("q", 5, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("not supported by the remote memory backend"),
        "got: {err}"
    );
}

#[tokio::test]
async fn count_reads_the_server_computed_total() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/{PROJECT}/memory")))
        .and(query_param("limit", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"entries": [entry(UUID, "one")], "total": 37})),
        )
        .mount(&server)
        .await;

    assert_eq!(backend(&server).count().await.unwrap(), 37);
}

// ── get ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_fetches_by_uuid_path_segment() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/{PROJECT}/memory/{UUID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(entry(UUID, "found")))
        .mount(&server)
        .await;

    let note = backend(&server)
        .get(UUID.parse().unwrap())
        .await
        .unwrap()
        .expect("entry present");
    assert_eq!(note.title, "found");
    assert_eq!(
        note.created_at, 1781830800,
        "RFC 3339 becomes epoch seconds"
    );
}

#[tokio::test]
async fn a_missing_entry_is_none_not_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/{PROJECT}/memory/{UUID}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    assert!(
        backend(&server)
            .get(UUID.parse().unwrap())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn an_archived_tombstone_becomes_archived_status() {
    let server = MockServer::start().await;
    let mut body = entry(UUID, "gone");
    body["archived_at"] = json!("2026-06-20T01:00:00Z");
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/{PROJECT}/memory/{UUID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let note = backend(&server)
        .get(UUID.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(note.status, "archived");
    assert_eq!(note.invalid_at, Some(1781917200));
}

// ── archive ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn archive_deletes_rather_than_posting_an_archive_subroute() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/projects/{PROJECT}/memory/{UUID}")))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    assert!(
        backend(&server)
            .archive(UUID.parse().unwrap())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn archiving_an_already_gone_entry_is_not_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/projects/{PROJECT}/memory/{UUID}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    assert!(
        !backend(&server)
            .archive(UUID.parse().unwrap())
            .await
            .unwrap(),
        "a 404 reports nothing changed, and must not surface as an error"
    );
}

// ── supersede ────────────────────────────────────────────────────────────────

async fn mount_two_live_entries(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/{PROJECT}/memory/{UUID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(entry(UUID, "old")))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/{PROJECT}/memory/{UUID_2}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(entry(UUID_2, "new")))
        .mount(server)
        .await;
}

#[tokio::test]
async fn supersede_posts_a_batch_edge_keyed_by_external_id() {
    let server = MockServer::start().await;
    mount_two_live_entries(&server).await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT}/memory/batch")))
        .and(body_json(json!({
            "entries": [],
            "edges": [{
                "from_external_id": format!("ext-{UUID}"),
                "to_external_id": format!("ext-{UUID_2}"),
                "kind": "supersedes",
            }],
        })))
        .respond_with(
            ResponseTemplate::new(207).set_body_json(json!({"edges": [{"status": "created"}]})),
        )
        .mount(&server)
        .await;

    assert!(
        backend(&server)
            .supersede(UUID.parse().unwrap(), UUID_2.parse().unwrap())
            .await
            .unwrap()
    );
}

// An edge naming an already-archived predecessor comes back unresolved.
// Reporting that as success would claim a link that does not exist.
#[tokio::test]
async fn an_unresolved_edge_reports_no_change_rather_than_success() {
    let server = MockServer::start().await;
    mount_two_live_entries(&server).await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT}/memory/batch")))
        .respond_with(
            ResponseTemplate::new(207).set_body_json(json!({"edges": [{"status": "unresolved"}]})),
        )
        .mount(&server)
        .await;

    assert!(
        !backend(&server)
            .supersede(UUID.parse().unwrap(), UUID_2.parse().unwrap())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn superseding_a_missing_entry_names_which_side_is_missing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/{PROJECT}/memory/{UUID}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let err = backend(&server)
        .supersede(UUID.parse().unwrap(), UUID_2.parse().unwrap())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("(old)"), "got: {err}");
}

// ── harvest dedupe (client-side source_commit filtering) ─────────────────────

// The cloud API has no server-side source_commit filter, so the client pages
// through and filters locally. The page boundary is where that goes wrong.
#[tokio::test]
async fn harvested_shas_pages_past_the_first_full_page() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/{PROJECT}/memory")))
        .respond_with(|req: &Request| {
            let offset: usize = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "offset")
                .and_then(|(_, v)| v.parse().ok())
                .unwrap_or(0);
            if offset == 0 {
                let entries: Vec<Value> = (0..PAGE_SIZE)
                    .map(|i| {
                        let mut e = entry(UUID, "t");
                        e["source_commit"] = json!(format!("sha{i:04}"));
                        e
                    })
                    .collect();
                ResponseTemplate::new(200).set_body_json(json!({"entries": entries, "total": 201}))
            } else {
                let mut e = entry(UUID_2, "tail");
                e["source_commit"] = json!("deadbeef");
                ResponseTemplate::new(200).set_body_json(json!({"entries": [e], "total": 201}))
            }
        })
        .mount(&server)
        .await;

    let shas = backend(&server).harvested_shas().await.unwrap();
    assert_eq!(shas.len(), PAGE_SIZE + 1);
    assert!(
        shas.contains("deadbeef"),
        "the entry past the page boundary must be seen"
    );
}

#[tokio::test]
async fn has_source_ref_matches_on_source_commit() {
    let server = MockServer::start().await;
    let mut e = entry(UUID, "harvested");
    e["source_commit"] = json!("deadbeefdeadbeef");
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/{PROJECT}/memory")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"entries": [e], "total": 1})))
        .mount(&server)
        .await;

    let be = backend(&server);
    assert!(be.has_source_ref("deadbeefdeadbeef").await.unwrap());
    assert!(!be.has_source_ref("cafebabe").await.unwrap());
}

// ── edges stay unsupported ───────────────────────────────────────────────────

#[tokio::test]
async fn edge_queries_stay_empty_as_on_every_remote_backend() {
    let server = MockServer::start().await;
    let be = backend(&server);
    assert!(be.add_edge(1, 2, "relates_to").await.is_ok());
    let (outgoing, incoming) = be.get_edges(1).await.unwrap();
    assert!(outgoing.is_empty() && incoming.is_empty());
}

#[tokio::test]
async fn every_request_carries_the_bearer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/{PROJECT}/memory")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"entries": [], "total": 0})))
        .mount(&server)
        .await;

    backend(&server).list(None, 10, false, None).await.unwrap();
    let req = &server.received_requests().await.unwrap()[0];
    assert_eq!(
        req.headers.get("authorization").unwrap(),
        "Bearer token",
        "CRUD calls are authenticated even though the peer probe is not"
    );
}

// ── all six routes accept a project slug, not only a UUID ────────────────────

// `GET`/`DELETE /memory/{entry_id}` used to be typed `Path<(Uuid, Uuid)>`
// server-side, unlike their four sibling routes, so a slug `project_id`
// worked for list/search/add but not for get/archive/supersede. Both routes
// now take `Path<(String, Uuid)>` like their siblings, so a slug works
// identically everywhere.
fn slug_backend(server: &MockServer) -> CloudApiMemoryBackend {
    CloudApiMemoryBackend {
        client: reqwest::Client::builder().build().unwrap(),
        base_url: server.uri(),
        project_id: "my-project".to_string(),
        api_key: None,
    }
}

#[tokio::test]
async fn a_slug_project_gets_an_entry() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/my-project/memory/{UUID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(entry(UUID, "t")))
        .mount(&server)
        .await;

    let note = slug_backend(&server)
        .get(UUID.parse().unwrap())
        .await
        .unwrap();
    assert!(note.is_some());
}

#[tokio::test]
async fn a_slug_project_archives_an_entry() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/projects/my-project/memory/{UUID}")))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    assert!(
        slug_backend(&server)
            .archive(UUID.parse().unwrap())
            .await
            .unwrap()
    );
}

// Listing and adding already took `Path<String>` before this change; keep
// them covered alongside get/archive so the whole set is exercised in one
// place.
#[tokio::test]
async fn a_slug_project_still_lists_and_adds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/my-project/memory"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"entries": [], "total": 0})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/my-project/memory"))
        .respond_with(ResponseTemplate::new(201).set_body_json(entry(UUID, "t")))
        .mount(&server)
        .await;

    let be = slug_backend(&server);
    assert!(be.list(None, 10, false, None).await.unwrap().is_empty());
    assert!(be.add(note_input("t")).await.is_ok());
}
