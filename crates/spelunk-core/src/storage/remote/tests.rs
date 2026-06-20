use super::*;

fn backend(project_id: &str) -> RemoteMemoryBackend {
    RemoteMemoryBackend::new(
        reqwest::Client::new(),
        "http://127.0.0.1:7777".to_string(),
        project_id.to_string(),
        None,
        None,
    )
}

fn backend_with_base(project_id: &str, base_url: &str) -> RemoteMemoryBackend {
    RemoteMemoryBackend::new(
        reqwest::Client::new(),
        base_url.to_string(),
        project_id.to_string(),
        None,
        None,
    )
}

/// `derive_local_fallback` / `normalise_git_url` slugs contain `/`; the
/// segment must be percent-encoded so axum routes the whole slug into
/// `{project_id}` instead of splitting on `/` (→ 404). See spelunk
/// decision #106 (mirrors IMP-1's fix in spelunk-cli/server_client.rs).
///
/// Note: URL construction now resolves slugs asynchronously (ADR-005), but for
/// loopback base URLs the slug is used directly (loopback guard D6) — no
/// network call, and the test is still effectively synchronous in semantics.
#[tokio::test]
async fn url_percent_encodes_local_fallback_slug() {
    let b = backend("local/9f2a8b3c4d5e6f70");
    assert_eq!(
        b.url("memory/search").await.unwrap(),
        "http://127.0.0.1:7777/v1/projects/local%2F9f2a8b3c4d5e6f70/memory/search"
    );
}

#[tokio::test]
async fn url_percent_encodes_github_remote_slug() {
    let b = backend("github.com/spelunk-cloud/spelunk");
    assert_eq!(
        b.url("memory").await.unwrap(),
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

#[tokio::test]
async fn url_leaves_simple_slug_unchanged() {
    let b = backend("my-project");
    assert_eq!(
        b.url("memory").await.unwrap(),
        "http://127.0.0.1:7777/v1/projects/my-project/memory"
    );
}

// ── ADR-005: slug→UUID resolution tests ──────────────────────────────────────

/// D5: A raw UUID in `project_id` must pass through to URLs without any
/// network call (zero-cost path).
#[tokio::test]
async fn uuid_project_id_passthrough_no_resolution() {
    let uuid = "018f4e2a-1234-7abc-8def-000000000001";
    // Use a non-loopback base URL — if resolution were attempted it would
    // fail because there's no mock server.
    let b = backend_with_base(uuid, "http://api.spelunk.cloud");
    let url = b.url("memory").await.unwrap();
    assert_eq!(
        url,
        format!("http://api.spelunk.cloud/v1/projects/{uuid}/memory")
    );
}

/// D6: Loopback servers skip resolution and use the slug directly.
#[tokio::test]
async fn loopback_server_skips_resolution() {
    // "my-slug" is not a UUID, but the base_url is loopback → use slug as-is
    let b = backend_with_base("my-slug", "http://127.0.0.1:7777");
    let url = b.url("memory").await.unwrap();
    assert_eq!(url, "http://127.0.0.1:7777/v1/projects/my-slug/memory");
}

/// D2 + D3: A slug resolves to UUID via GET /v1/projects.
/// Tests `resolve_cloud_project_uuid` directly — the loopback guard in
/// `effective_project_id` is exercised separately in `loopback_server_skips_resolution`.
#[tokio::test]
async fn slug_resolves_to_uuid_via_api() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/projects"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "projects": [
                {"id": "018f4e2a-1234-7abc-8def-000000000001", "slug": "spelunk"},
                {"id": "00000000-0000-0000-0000-000000000002", "slug": "other-project"}
            ]
        })))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let uuid = resolve_cloud_project_uuid(&client, &server.uri(), None, "spelunk", None)
        .await
        .unwrap();
    assert_eq!(uuid, "018f4e2a-1234-7abc-8def-000000000001");
}

/// D4: Cache hit with matching slug returns cached UUID without hitting the API.
#[tokio::test]
async fn cache_hit_matching_slug_returns_cached_uuid() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let spelunk_dir = tmp.path();

    write_cache(spelunk_dir, "my-slug", "018f4e2a-0000-0000-0000-000000000099");

    // The API should NOT be called (mock returns 500 to detect unwanted calls)
    Mock::given(method("GET"))
        .and(path("/v1/projects"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let uuid =
        resolve_cloud_project_uuid(&client, &server.uri(), None, "my-slug", Some(spelunk_dir))
            .await
            .unwrap();
    assert_eq!(
        uuid, "018f4e2a-0000-0000-0000-000000000099",
        "expected cached UUID, got: {uuid}"
    );
}

/// D4: Cache hit with mismatched slug discards cache and re-resolves.
#[tokio::test]
async fn cache_miss_stale_slug_triggers_resolution() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let spelunk_dir = tmp.path();

    // Write a cache with a DIFFERENT slug
    write_cache(spelunk_dir, "old-slug", "018f4e2a-0000-0000-0000-000000000099");

    // The API MUST be called because the cached slug doesn't match
    Mock::given(method("GET"))
        .and(path("/v1/projects"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "projects": [
                {"id": "018f4e2a-1234-7abc-8def-aaaaaaaaaaaa", "slug": "new-slug"}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let uuid =
        resolve_cloud_project_uuid(&client, &server.uri(), None, "new-slug", Some(spelunk_dir))
            .await
            .unwrap();
    assert_eq!(
        uuid, "018f4e2a-1234-7abc-8def-aaaaaaaaaaaa",
        "expected freshly resolved UUID, got: {uuid}"
    );
}

/// Slug not found in project list → descriptive error message.
#[tokio::test]
async fn slug_not_found_returns_descriptive_error() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/projects"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "projects": [
                {"id": "00000000-0000-0000-0000-000000000001", "slug": "other-project"}
            ]
        })))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let err =
        resolve_cloud_project_uuid(&client, &server.uri(), None, "spelunk", None)
            .await
            .unwrap_err()
            .to_string();

    assert!(
        err.contains("project slug \"spelunk\" not found"),
        "expected slug-not-found error, got: {err}"
    );
    assert!(
        err.contains("spelunk projects list") || err.contains(".spelunk/config.toml"),
        "expected actionable hint in error, got: {err}"
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

    // Use loopback base URL so the slug is used as-is (no slug resolution)
    let backend = RemoteMemoryBackend::new(
        reqwest::Client::new(),
        server.uri(),
        "local/abc123".to_string(),
        None,
        None,
    );

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
