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

// ── Cloud-api slug → UUID resolution (ADR-005) ────────────────────────────────

use tempfile::TempDir;

const UUID_A: &str = "018f4e2a-1234-7abc-8def-000000000001";
const UUID_B: &str = "018f4e2a-1234-7abc-8def-000000000002";

fn write_cache(dir: &std::path::Path, slug: &str, uuid: &str) {
    std::fs::write(
        dir.join(CLOUD_PROJECT_CACHE_FILE),
        format!("slug = \"{slug}\"\nuuid = \"{uuid}\"\n"),
    )
    .unwrap();
}

/// D5: a raw UUID in config is used directly — no lookup, no cache write.
#[tokio::test]
async fn resolve_raw_uuid_passes_through_without_lookup() {
    let tmp = TempDir::new().unwrap();
    // server_url points nowhere reachable; if a lookup were attempted this
    // would fail. A raw UUID must short-circuit before any network call.
    let got =
        resolve_cloud_project_uuid(UUID_A, "https://api.example.com", Some("key"), tmp.path())
            .await
            .unwrap();
    assert_eq!(got.to_string(), UUID_A);
    // No cache should have been written for the raw-UUID path.
    assert!(!tmp.path().join(CLOUD_PROJECT_CACHE_FILE).exists());
}

/// D6: loopback / unset server_url with a slug is a misuse — clear error, no
/// network call.
#[tokio::test]
async fn resolve_loopback_with_slug_errors() {
    let tmp = TempDir::new().unwrap();
    let err = resolve_cloud_project_uuid("spelunk", "http://127.0.0.1:7777", None, tmp.path())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("loopback"), "got: {err}");

    let err2 = resolve_cloud_project_uuid("spelunk", "", None, tmp.path())
        .await
        .unwrap_err();
    assert!(err2.to_string().contains("loopback"), "got: {err2}");
}

/// D4: a cached entry whose stored slug matches the current project_id is used
/// without any network call.
#[tokio::test]
#[serial_test::serial]
async fn resolve_uses_cache_when_slug_matches() {
    unsafe { std::env::remove_var("SPELUNK_NO_SLUG_CACHE") };
    let tmp = TempDir::new().unwrap();
    write_cache(tmp.path(), "spelunk", UUID_A);

    // Unreachable server_url: a cache hit must avoid the network entirely.
    let got = resolve_cloud_project_uuid(
        "spelunk",
        "https://unreachable.invalid",
        Some("key"),
        tmp.path(),
    )
    .await
    .unwrap();
    assert_eq!(got.to_string(), UUID_A);
}

/// D4 invalidation: a cache whose stored slug differs from the current
/// project_id is discarded and re-resolved via GET /v1/projects.
#[tokio::test]
#[serial_test::serial]
async fn resolve_discards_cache_on_slug_mismatch() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    unsafe { std::env::remove_var("SPELUNK_NO_SLUG_CACHE") };
    let tmp = TempDir::new().unwrap();
    // Cache holds an entry for a *different* slug than we resolve.
    write_cache(tmp.path(), "old-slug", UUID_A);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "projects": [
                { "id": UUID_B, "slug": "new-slug" }
            ]
        })))
        .mount(&server)
        .await;

    let got = resolve_cloud_project_uuid_inner("new-slug", &server.uri(), Some("key"), tmp.path())
        .await
        .unwrap();
    assert_eq!(
        got.to_string(),
        UUID_B,
        "stale-slug cache must be discarded and re-resolved"
    );

    // The cache should now be rewritten to the new slug → uuid mapping.
    let rewritten = std::fs::read_to_string(tmp.path().join(CLOUD_PROJECT_CACHE_FILE)).unwrap();
    assert!(rewritten.contains("new-slug"), "cache: {rewritten}");
    assert!(rewritten.contains(UUID_B), "cache: {rewritten}");
}

/// Happy path: slug resolves via GET /v1/projects and the result is cached.
#[tokio::test]
#[serial_test::serial]
async fn resolve_via_list_endpoint_and_caches() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    unsafe { std::env::remove_var("SPELUNK_NO_SLUG_CACHE") };
    let tmp = TempDir::new().unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects"))
        .and(header("Authorization", "Bearer sekret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "projects": [
                { "id": UUID_A, "slug": "other" },
                { "id": UUID_B, "slug": "spelunk" }
            ]
        })))
        .mount(&server)
        .await;

    let got =
        resolve_cloud_project_uuid_inner("spelunk", &server.uri(), Some("sekret"), tmp.path())
            .await
            .unwrap();
    assert_eq!(got.to_string(), UUID_B);

    // Cached for next time.
    let cached = read_cloud_project_cache(tmp.path(), "spelunk").unwrap();
    assert_eq!(cached.to_string(), UUID_B);
}

/// D6: a slug not present in the list yields a fatal, actionable error.
#[tokio::test]
#[serial_test::serial]
async fn resolve_slug_not_found_errors() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    unsafe { std::env::remove_var("SPELUNK_NO_SLUG_CACHE") };
    let tmp = TempDir::new().unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "projects": [ { "id": UUID_A, "slug": "other" } ]
        })))
        .mount(&server)
        .await;

    let err = resolve_cloud_project_uuid_inner("missing", &server.uri(), Some("k"), tmp.path())
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("missing"), "got: {msg}");
    assert!(msg.contains("not found"), "got: {msg}");
    // D2/D6: the error must be *actionable* — point the user at the recovery
    // steps (list projects / inspect config), not just say "not found".
    assert!(
        msg.contains("spelunk projects list") && msg.contains("config.toml"),
        "slug-not-found error must include the actionable recovery hint; got: {msg}"
    );
    // A "not found" must NOT have poisoned the cache with a bogus entry.
    assert!(
        !tmp.path().join(CLOUD_PROJECT_CACHE_FILE).exists(),
        "no cache file should be written when the slug is not found"
    );
}

/// D6: GET /v1/projects returning a 401 (auth) surfaces a fatal error mentioning
/// the URL/status, and does not write a cache entry.
#[tokio::test]
#[serial_test::serial]
async fn resolve_surfaces_401_error() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    unsafe { std::env::remove_var("SPELUNK_NO_SLUG_CACHE") };
    let tmp = TempDir::new().unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let err = resolve_cloud_project_uuid_inner("spelunk", &server.uri(), Some("bad"), tmp.path())
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("/v1/projects"),
        "error should name the endpoint; got: {msg}"
    );
    assert!(
        msg.contains("401") || msg.to_lowercase().contains("unauthorized"),
        "error should surface the 401 status; got: {msg}"
    );
    assert!(
        !tmp.path().join(CLOUD_PROJECT_CACHE_FILE).exists(),
        "no cache should be written on an error response"
    );
}

/// D6: GET /v1/projects returning a 5xx surfaces a fatal error with the status.
#[tokio::test]
#[serial_test::serial]
async fn resolve_surfaces_5xx_error() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    unsafe { std::env::remove_var("SPELUNK_NO_SLUG_CACHE") };
    let tmp = TempDir::new().unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let err = resolve_cloud_project_uuid_inner("spelunk", &server.uri(), Some("k"), tmp.path())
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("503") || msg.to_lowercase().contains("server error"),
        "error should surface the 5xx status; got: {msg}"
    );
}

/// D6: a connection failure (server unreachable) surfaces a fatal error that
/// names the endpoint being resolved, rather than panicking or hanging.
#[tokio::test]
#[serial_test::serial]
async fn resolve_surfaces_connection_error() {
    unsafe { std::env::remove_var("SPELUNK_NO_SLUG_CACHE") };
    let tmp = TempDir::new().unwrap();

    // Bind then immediately drop a listener to obtain a port nothing listens on.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let dead_url = format!("http://127.0.0.1:{port}");

    let err = resolve_cloud_project_uuid_inner("spelunk", &dead_url, Some("k"), tmp.path())
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("/v1/projects") && msg.contains("spelunk"),
        "connection error should name the endpoint and slug being resolved; got: {msg}"
    );
}

/// SPELUNK_NO_SLUG_CACHE=1 forces a fresh lookup, ignoring an existing cache.
#[tokio::test]
#[serial_test::serial]
async fn resolve_no_cache_env_forces_fresh_lookup() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let tmp = TempDir::new().unwrap();
    // Cache says UUID_A, but a forced fresh lookup must return the server value.
    write_cache(tmp.path(), "spelunk", UUID_A);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "projects": [ { "id": UUID_B, "slug": "spelunk" } ]
        })))
        .mount(&server)
        .await;

    unsafe { std::env::set_var("SPELUNK_NO_SLUG_CACHE", "1") };
    let got =
        resolve_cloud_project_uuid_inner("spelunk", &server.uri(), Some("k"), tmp.path()).await;
    unsafe { std::env::remove_var("SPELUNK_NO_SLUG_CACHE") };

    assert_eq!(got.unwrap().to_string(), UUID_B);
}
