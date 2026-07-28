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
