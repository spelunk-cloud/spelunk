use super::*;

fn backend(project_id: &str) -> RemoteMemoryBackend {
    RemoteMemoryBackend {
        client: reqwest::Client::new(),
        base_url: "http://127.0.0.1:7777".to_string(),
        project_id: project_id.to_string(),
        api_key: None,
    }
}

/// `derive_local_fallback` / `normalise_git_url` slugs contain `/`; the
/// segment must be percent-encoded so axum routes the whole slug into
/// `{project_id}` instead of splitting on `/` (→ 404). See spelunk
/// decision #106 (mirrors IMP-1's fix in spelunk-cli/server_client.rs).
#[test]
fn url_percent_encodes_local_fallback_slug() {
    let b = backend("local/9f2a8b3c4d5e6f70");
    assert_eq!(
        b.url("memory/search"),
        "http://127.0.0.1:7777/v1/projects/local%2F9f2a8b3c4d5e6f70/memory/search"
    );
}

#[test]
fn url_percent_encodes_github_remote_slug() {
    let b = backend("github.com/spelunk-cloud/spelunk");
    assert_eq!(
        b.url("memory"),
        "http://127.0.0.1:7777/v1/projects/github.com%2Fspelunk-cloud%2Fspelunk/memory"
    );
}

/// Round-trip: percent-decoding the encoded segment must yield the
/// original slug, since the slug is the persistence key
/// (`projects.slug` UNIQUE) and must reach `require_project`/
/// `upsert_project` exactly as `derive_project_id` produced it.
#[test]
fn encode_project_id_round_trips_through_percent_decode() {
    for slug in ["local/9f2a8b3c4d5e6f70", "github.com/spelunk-cloud/spelunk"] {
        let encoded = encode_project_id(slug);
        let decoded = percent_encoding::percent_decode_str(&encoded)
            .decode_utf8()
            .expect("valid UTF-8 after percent-decoding");
        assert_eq!(decoded, slug, "round-trip mismatch for slug {slug:?}");
    }
}

#[test]
fn url_leaves_simple_slug_unchanged() {
    let b = backend("my-project");
    assert_eq!(
        b.url("memory"),
        "http://127.0.0.1:7777/v1/projects/my-project/memory"
    );
}

/// Regression test for the v0.8.0 IMP-3 retest sweep (spelunk-cloud/spelunk
/// agent-comms/handoffs/qa-v080-test-plan.md, Fix 3).
///
/// `POST /v1/projects/{id}/memory/search` on the real server expects
/// `{"query": <text>, "limit": <n>}` — the server embeds the query itself
/// (see `spelunk_server::handlers::SearchRequest` /
/// `spelunk-server/src/handlers.rs::search_notes`, which calls
/// `body.query` through its embedder).
///
/// `RemoteMemoryBackend::search` instead serialises a pre-computed
/// `{"embedding": [f32...], "limit": <n>}` body (see `SearchRequest` in
/// this file). Because the server's `query` field is a required `String`
/// (no `#[serde(default)]`), axum's `Json<SearchRequest>` extractor
/// rejects the mismatched body with `422 Unprocessable Entity` *before*
/// `search_notes` ever runs — so `memory search` / `memory timeline`
/// (which both funnel through `RemoteMemoryBackend::search`) always fail
/// with a 422 against a real spelunk-server, never returning results.
///
/// This was masked pre-IMP-3 because `memory search`/`timeline` short-
/// circuited on `cfg.server_url.is_none()` with a "requires
/// spelunk-server" error before ever issuing the HTTP request — IMP-3
/// fixed that gating (so loopback auto-discovered servers are honoured),
/// which is what newly exposes this pre-existing client/server payload
/// mismatch end-to-end.
///
/// This test asserts the wire body sent by the client is shaped the way
/// the real server's `SearchRequest` requires (`query` + `limit`, no
/// `embedding` field). It currently FAILS — the client sends `embedding`
/// instead of `query` — capturing the bug for the implementer to fix
/// (either by changing `RemoteMemoryBackend::SearchRequest` to send
/// `{query, limit}` and dropping the local KNN step, or by adding an
/// `embedding`-accepting variant server-side; that decision belongs to
/// the implementer / architect, not this test).
#[tokio::test]
async fn search_sends_query_text_not_precomputed_embedding() {
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // Mirrors the real server's contract: a body containing a `query`
    // string field (and NOT requiring `embedding`) is what
    // `spelunk-server::handlers::search_notes` actually accepts.
    Mock::given(method("POST"))
        .and(path("/v1/projects/local%2Fabc123/memory/search"))
        .and(body_partial_json(
            serde_json::json!({ "query": "timezone" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let backend = RemoteMemoryBackend {
        client: reqwest::Client::new(),
        base_url: server.uri(),
        project_id: "local/abc123".to_string(),
        api_key: None,
    };

    // `MemoryBackend::search` takes both a pre-computed query embedding
    // blob (used by local backends for KNN) *and* the raw query text
    // (used by the remote backend, which has no local embedder and must
    // let the server embed server-side — see spelunk#359). The remote
    // backend ignores `query_blob` and sends `query` on the wire.
    let query_blob = crate::embeddings::vec_to_blob(&[0.1_f32, 0.2, 0.3]);
    let result = backend.search(&query_blob, "timezone", 3, None).await;

    assert!(
        result.is_ok(),
        "expected the server to accept the request body and return results, \
         got: {:?}\n\n\
         If this failed with a 422-shaped error, the client is still \
         sending `{{\"embedding\": [...], \"limit\": N}}` instead of the \
         `{{\"query\": \"<text>\", \"limit\": N}}` shape the real \
         spelunk-server requires — see spelunk-cloud/spelunk issue for \
         'memory search returns 422 against a real server (query/embedding \
         payload mismatch)'.",
        result.err().map(|e| e.to_string())
    );
}

// ── CLI to peer: query parameters this server does not accept ────────────────
//
// Pins live drift rather than desired behaviour. `spelunk_server::handlers::
// ListQuery` deserialises exactly three names from `GET /memory`: `kind`,
// `limit`, `archived`. Axum's `Query` extractor ignores anything else, so the
// two parameters below are accepted by the transport, dropped by the handler,
// and never reported to the caller.
//
// The `source_ref` case is the one with teeth: `has_source_ref` decides whether
// a commit has already been harvested purely from whether the filtered list
// came back non-empty. With the filter dropped, the server answers with the
// project's newest entries regardless of the sha asked about, so the answer is
// "yes" for every commit as soon as the project holds any memory at all.
//
// When the server grows these parameters (or the client stops sending them),
// this test is the thing that has to change, and its failure is the reminder
// that `has_source_ref` was reading a filtered list that was never filtered.
#[tokio::test]
async fn list_sends_query_parameters_the_oss_server_silently_drops() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/local%2Fabc123/memory"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let backend = RemoteMemoryBackend {
        client: reqwest::Client::new(),
        base_url: server.uri(),
        project_id: "local/abc123".to_string(),
        api_key: None,
    };

    backend
        .list(None, 5, false, Some(1_700_000_000))
        .await
        .expect("list must reach the mock");
    backend
        .list_by_source_ref("deadbeefcafe", 1, true, None)
        .await
        .expect("list_by_source_ref must reach the mock");

    let queries: Vec<String> = server
        .received_requests()
        .await
        .expect("mock server records requests")
        .iter()
        .map(|r| r.url.query().unwrap_or_default().to_string())
        .collect();

    let accepted_by_the_server = ["kind", "limit", "archived"];
    let sent: Vec<&str> = queries
        .iter()
        .flat_map(|q| q.split('&'))
        .filter_map(|pair| pair.split('=').next())
        .filter(|name| !accepted_by_the_server.contains(name))
        .collect();

    assert!(
        sent.contains(&"as_of"),
        "expected `list` to still be sending the unsupported `as_of` parameter; \
         if it stopped, delete this test. Sent: {sent:?}"
    );
    assert!(
        sent.contains(&"source_ref"),
        "expected `list_by_source_ref` to still be sending the unsupported \
         `source_ref` parameter; if it stopped, delete this test. Sent: {sent:?}"
    );
}

// ── Wire-shape tolerance ─────────────────────────────────────────────────────
//
// The read endpoints must accept both shapes a team server can send: the
// object envelope a server at or after the ADR-076 wire-contract fix returns
// (`{entries, total}` / `{shas}`), and the bare array an older server still in
// the version-skew support window returns. Accepting both is what keeps a
// newer CLI working against an older team server. See docs/version-skew.md.

fn note_json(title: &str) -> serde_json::Value {
    serde_json::json!({
        "id": 1,
        "kind": "decision",
        "title": title,
        "body": "b",
        "tags": [],
        "linked_files": [],
        "created_at": 0,
        "status": "active",
        "superseded_by": null,
    })
}

fn backend_at(uri: String) -> RemoteMemoryBackend {
    RemoteMemoryBackend {
        client: reqwest::Client::new(),
        base_url: uri,
        project_id: "proj".to_string(),
        api_key: None,
    }
}

#[tokio::test]
async fn list_accepts_object_envelope_from_newer_server() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [note_json("A")],
            "total": 1,
        })))
        .mount(&server)
        .await;

    let notes = backend_at(server.uri())
        .list(None, 10, false, None)
        .await
        .expect("list must parse the object envelope");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "A");
}

#[tokio::test]
async fn list_accepts_bare_array_from_older_server() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([note_json("A")])))
        .mount(&server)
        .await;

    let notes = backend_at(server.uri())
        .list(None, 10, false, None)
        .await
        .expect("list must still parse a legacy bare array");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "A");
}

#[tokio::test]
async fn search_accepts_object_envelope_from_newer_server() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [note_json("hit")],
            "total": 1,
        })))
        .mount(&server)
        .await;

    let query_blob = crate::embeddings::vec_to_blob(&[0.1_f32, 0.2, 0.3]);
    let notes = backend_at(server.uri())
        .search(&query_blob, "q", 5, None)
        .await
        .expect("search must parse the object envelope");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "hit");
}

#[tokio::test]
async fn harvested_shas_accepts_both_shapes() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let enveloped = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/harvested-shas"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "shas": ["abc"] })),
        )
        .mount(&enveloped)
        .await;
    let shas = backend_at(enveloped.uri())
        .harvested_shas()
        .await
        .expect("harvested_shas must parse the object envelope");
    assert!(shas.contains("abc"), "got: {shas:?}");

    let bare = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/harvested-shas"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!(["def"])))
        .mount(&bare)
        .await;
    let shas = backend_at(bare.uri())
        .harvested_shas()
        .await
        .expect("harvested_shas must still parse a legacy bare array");
    assert!(shas.contains("def"), "got: {shas:?}");
}
