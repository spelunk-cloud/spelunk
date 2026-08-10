// Backend selection under the resolved sync mode: which store `open_memory_backend`
// picks, what reaches the wire, and what it refuses to route at.

use super::open_memory_backend;
use crate::config::{Config, SyncMode};
use crate::storage::memory::NoteId;
use anyhow::Result;
use std::sync::OnceLock;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn register_sqlite_vec() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        #[allow(clippy::missing_transmute_annotations)]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

fn clear_env() {
    unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };
}

#[tokio::test]
#[serial_test::serial]
async fn offline_mode_routes_local_even_with_server_url() {
    clear_env();
    register_sqlite_vec();
    let cfg = Config {
        server_url: Some("http://team.example.com:7777".to_string()),
        project_id: Some("team/proj".to_string()),
        mode: Some(SyncMode::Offline),
        ..Default::default()
    };
    let be = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None)
        .await
        .unwrap();
    assert_eq!(
        be.backend_kind(),
        "sqlite",
        "offline must keep memory local even when server_url is set"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn local_first_mode_routes_local() {
    clear_env();
    register_sqlite_vec();
    let cfg = Config {
        server_url: Some("http://team.example.com:7777".to_string()),
        project_id: Some("team/proj".to_string()),
        mode: Some(SyncMode::LocalFirst),
        ..Default::default()
    };
    let be = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None)
        .await
        .unwrap();
    assert_eq!(be.backend_kind(), "sqlite");
}

// The configuration this suite exists to pin is a non-loopback team server,
// and loopback-ness is the axis every `cloud_first` transport rule keys on,
// so these tests must not be satisfied by a `127.0.0.1` peer. wiremock binds
// `127.0.0.1`; the same listener addressed as `0.0.0.0` is classified
// non-loopback while the OS still routes to it, so the real branch is driven
// with no live network or DNS.
//
// Connecting to `0.0.0.0` raises `WSAEADDRNOTAVAIL` on Windows, hence the
// `cfg_attr(windows, ignore)` on every test that uses it.
fn non_loopback_alias(server: &MockServer) -> String {
    let url = server.uri().replace("127.0.0.1", "0.0.0.0");
    assert!(
        !crate::config::is_loopback_url(&url),
        "test seam precondition: {url} must be classified non-loopback"
    );
    url
}

async fn mount_stats(server: &MockServer, project_segment: &str, count: i64) {
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/{project_segment}/stats")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "count": count })),
        )
        .mount(server)
        .await;
}

fn cloud_first_cfg(url: &str, project_id: &str) -> Config {
    Config {
        server_url: Some(url.to_string()),
        project_id: Some(project_id.to_string()),
        mode: Some(SyncMode::CloudFirst),
        ..Default::default()
    }
}

// Production reaches this seam only after `open_memory_backend` has
// enforced `validate_transport_url`, which the `0.0.0.0` plaintext alias
// would not clear. The injected store keeps bearer resolution off the
// developer's real secret store.
async fn open_seam(cfg: &Config, url: &str) -> Result<Box<dyn super::MemoryBackend + Send>> {
    let store = crate::config::secret_store::MemoryStore::default();
    super::open_remote_memory_backend_with_store(cfg, url, &store).await
}

// Paths requested, minus the peer probe: it is issued on every open by design
// and is not what these assertions are pinning.
async fn memory_paths(server: &MockServer) -> Vec<String> {
    requested_paths(server)
        .await
        .into_iter()
        .filter(|p| p != "/v1/health")
        .collect()
}

async fn requested_paths(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect()
}

// The configured `project_id` is the project key, so opening the backend never
// looks one up. `GET /v1/projects` is the retired slug-to-UUID resolver: a
// self-hosted server answers it in a shape the resolver could not deserialize,
// so reintroducing it would break the documented `cloud_first` configuration at
// open and take every memory command with it. That is the harm this names.
//
// A `GET /v1/health` peer probe IS issued, to pick the memory dialect. It
// cannot cause the same harm, because every failure to answer it resolves to
// the self-hosted dialect: the mocks below mount no `/v1/health`, so each of
// these tests exercises that fallback and proves the self-hosted path is
// reached exactly as it was before the probe existed.
async fn assert_no_project_lookup(server: &MockServer) {
    let paths = requested_paths(server).await;
    assert!(
        !paths.iter().any(|p| p == "/v1/projects"),
        "opening the backend must not resolve the project; saw {paths:?}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn cloud_first_mode_routes_remote() {
    clear_env();
    // This goes through the PUBLIC `open_memory_backend`, which resolves
    // the bearer via `Config::bearer_for`: the host's *default* secret
    // store. Isolate `HOME` + force the file backend so this never reads
    // or writes the developer's real `~/.config/spelunk`.
    let home = tempfile::TempDir::new().unwrap();
    let original_home = std::env::var("HOME").ok();
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("SPELUNK_SECRET_STORE", "file");
    }

    // The backend is built without contacting the server at all, so nothing
    // needs to listen on this port for the routing decision to be observable.
    let cfg = cloud_first_cfg("http://127.0.0.1:7777", "team/proj");
    let be = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None)
        .await
        .unwrap();

    unsafe {
        match original_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        std::env::remove_var("SPELUNK_SECRET_STORE");
    }

    assert_eq!(
        be.backend_kind(),
        "remote",
        "cloud_first (server-authoritative) routes memory CRUD to the cloud"
    );
}

// ── Project id reaches the server exactly as configured ───────────────────

#[tokio::test]
#[serial_test::serial]
#[cfg_attr(windows, ignore)]
async fn cloud_first_slug_reaches_server_verbatim() {
    clear_env();
    let server = MockServer::start().await;
    mount_stats(&server, "my-awesome-app", 7).await;

    let url = non_loopback_alias(&server);
    let be = open_seam(&cloud_first_cfg(&url, "my-awesome-app"), &url)
        .await
        .unwrap();

    assert_eq!(be.count().await.unwrap(), 7);
    assert_no_project_lookup(&server).await;
}

#[tokio::test]
#[serial_test::serial]
#[cfg_attr(windows, ignore)]
async fn cloud_first_raw_uuid_reaches_server_verbatim() {
    clear_env();
    const RAW_UUID: &str = "018f4e2a-1234-7abc-8def-0000000000aa";

    let server = MockServer::start().await;
    mount_stats(&server, RAW_UUID, 3).await;

    let url = non_loopback_alias(&server);
    let be = open_seam(&cloud_first_cfg(&url, RAW_UUID), &url)
        .await
        .unwrap();

    assert_eq!(be.count().await.unwrap(), 3);
    assert_no_project_lookup(&server).await;
}

// `derive_project_id` slugs contain `/`. The whole slug must occupy one
// captured path segment, which the server percent-decodes back to the
// original string it keys on.
#[tokio::test]
#[serial_test::serial]
#[cfg_attr(windows, ignore)]
async fn cloud_first_git_remote_slug_is_one_encoded_segment() {
    clear_env();
    let server = MockServer::start().await;
    mount_stats(&server, "github.com%2Fowner%2Frepo", 11).await;

    let url = non_loopback_alias(&server);
    let be = open_seam(&cloud_first_cfg(&url, "github.com/owner/repo"), &url)
        .await
        .unwrap();

    assert_eq!(be.count().await.unwrap(), 11);
    assert_no_project_lookup(&server).await;
    let decoded = percent_encoding::percent_decode_str("github.com%2Fowner%2Frepo")
        .decode_utf8()
        .unwrap();
    assert_eq!(decoded, "github.com/owner/repo");
}

#[tokio::test]
#[serial_test::serial]
#[cfg_attr(windows, ignore)]
async fn cloud_first_local_derived_slug_is_one_encoded_segment() {
    clear_env();
    let server = MockServer::start().await;
    mount_stats(&server, "local%2F9f2a8b3c4d5e6f70", 5).await;

    let url = non_loopback_alias(&server);
    let be = open_seam(&cloud_first_cfg(&url, "local/9f2a8b3c4d5e6f70"), &url)
        .await
        .unwrap();

    assert_eq!(be.count().await.unwrap(), 5);
    assert_no_project_lookup(&server).await;
    let decoded = percent_encoding::percent_decode_str("local%2F9f2a8b3c4d5e6f70")
        .decode_utf8()
        .unwrap();
    assert_eq!(decoded, "local/9f2a8b3c4d5e6f70");
}

// `project_id` is opaque to the CLI and the peer's slug key is case-sensitive,
// so nothing on the way out may normalise its case. Every other fixture in this
// file is already lowercase, which would hide a case-folding transform.
#[tokio::test]
#[serial_test::serial]
#[cfg_attr(windows, ignore)]
async fn cloud_first_mixed_case_project_id_is_not_normalised() {
    clear_env();
    let server = MockServer::start().await;
    mount_stats(&server, "GitHub.com%2FOwner%2FRepo-CamelCase", 13).await;

    let url = non_loopback_alias(&server);
    let be = open_seam(
        &cloud_first_cfg(&url, "GitHub.com/Owner/Repo-CamelCase"),
        &url,
    )
    .await
    .unwrap();

    assert_eq!(be.count().await.unwrap(), 13);
    assert_no_project_lookup(&server).await;
}

// A raw UUID is equally opaque: canonical UUIDs are case-insensitive, but the
// peer keys on the string, so an uppercase one must not be folded either.
#[tokio::test]
#[serial_test::serial]
#[cfg_attr(windows, ignore)]
async fn cloud_first_uppercase_uuid_is_not_normalised() {
    clear_env();
    const UPPER_UUID: &str = "018F4E2A-1234-7ABC-8DEF-0000000000AA";

    let server = MockServer::start().await;
    mount_stats(&server, UPPER_UUID, 2).await;

    let url = non_loopback_alias(&server);
    let be = open_seam(&cloud_first_cfg(&url, UPPER_UUID), &url)
        .await
        .unwrap();

    assert_eq!(be.count().await.unwrap(), 2);
    assert_no_project_lookup(&server).await;
}

// A repo that reached the hosted API before the passthrough still carries the
// resolver's cache file, which maps the slug to a different string. Nothing
// reads it any more: the configured `project_id` is the only project key, and
// the file is not rewritten either.
#[tokio::test]
#[serial_test::serial]
async fn a_leftover_project_id_cache_on_disk_changes_nothing() {
    clear_env();
    let home = tempfile::TempDir::new().unwrap();
    let original_home = std::env::var("HOME").ok();
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("SPELUNK_SECRET_STORE", "file");
    }

    let server = MockServer::start().await;
    mount_stats(&server, "team%2Fproj", 4).await;

    let spelunk_dir = tempfile::TempDir::new().unwrap();
    let mem_path = spelunk_dir.path().join("memory.db");
    let stale = spelunk_dir.path().join("cloud-project-id.lock");
    const STALE_BODY: &str =
        "slug = \"team/proj\"\nuuid = \"018f4e2a-1234-7abc-8def-00000000beef\"\n";
    std::fs::write(&stale, STALE_BODY).unwrap();

    let cfg = cloud_first_cfg(&server.uri(), "team/proj");
    let be = open_memory_backend(&cfg, &mem_path, None).await.unwrap();

    unsafe {
        match original_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        std::env::remove_var("SPELUNK_SECRET_STORE");
    }

    assert_eq!(be.count().await.unwrap(), 4);
    assert_eq!(
        memory_paths(&server).await,
        vec!["/v1/projects/team%2Fproj/stats".to_string()],
        "a leftover cache file must not divert the configured project id"
    );
    assert_eq!(
        std::fs::read_to_string(&stale).unwrap(),
        STALE_BODY,
        "nothing may rewrite the retired cache file either"
    );
}

// The loopback peer takes the same passthrough as any other, and nothing is
// cached beside `memory.db`.
#[tokio::test]
#[serial_test::serial]
async fn cloud_first_loopback_slug_needs_no_lookup() {
    clear_env();
    let home = tempfile::TempDir::new().unwrap();
    let original_home = std::env::var("HOME").ok();
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("SPELUNK_SECRET_STORE", "file");
    }

    let server = MockServer::start().await;
    mount_stats(&server, "team%2Fproj", 2).await;

    let spelunk_dir = tempfile::TempDir::new().unwrap();
    let mem_path = spelunk_dir.path().join("memory.db");
    let cfg = cloud_first_cfg(&server.uri(), "team/proj");
    let be = open_memory_backend(&cfg, &mem_path, None).await.unwrap();

    unsafe {
        match original_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        std::env::remove_var("SPELUNK_SECRET_STORE");
    }

    assert_eq!(be.count().await.unwrap(), 2);
    assert_eq!(
        memory_paths(&server).await,
        vec!["/v1/projects/team%2Fproj/stats".to_string()],
        "the memory call must be the only memory request the open path makes"
    );
    assert!(
        !spelunk_dir.path().join("cloud-project-id.lock").exists(),
        "nothing may cache a resolved project id beside memory.db any more"
    );
}

#[tokio::test]
#[serial_test::serial]
#[cfg_attr(windows, ignore)]
async fn local_first_non_loopback_stays_local_and_silent() {
    clear_env();
    register_sqlite_vec();
    let server = MockServer::start().await;

    let url = non_loopback_alias(&server);
    let cfg = Config {
        mode: Some(SyncMode::LocalFirst),
        ..cloud_first_cfg(&url, "my-awesome-app")
    };
    let be = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None)
        .await
        .unwrap();

    assert_eq!(be.backend_kind(), "sqlite");
    assert!(
        requested_paths(&server).await.is_empty(),
        "local_first must issue no HTTP of any kind at open"
    );
}

#[tokio::test]
#[serial_test::serial]
#[cfg_attr(windows, ignore)]
async fn offline_non_loopback_stays_local_and_silent() {
    clear_env();
    register_sqlite_vec();
    let server = MockServer::start().await;

    let url = non_loopback_alias(&server);
    let cfg = Config {
        mode: Some(SyncMode::Offline),
        ..cloud_first_cfg(&url, "my-awesome-app")
    };
    let be = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None)
        .await
        .unwrap();

    assert_eq!(be.backend_kind(), "sqlite");
    assert!(
        requested_paths(&server).await.is_empty(),
        "offline must issue no HTTP of any kind at open"
    );
}

// ── The documented self-hosted cloud_first config ─────────────────────────

// The three keys documented for a self-hosted `cloud_first` server
// (`server_url` + `project_id` + `mode`), pointed at a mock that serves the
// OSS team server's route shapes. `server_url` is the mock's non-loopback
// alias rather than the documented `https://` host, which is the only
// substitution: the branch under test keys on non-loopback-ness, not on the
// scheme.
#[tokio::test]
#[serial_test::serial]
#[cfg_attr(windows, ignore)]
async fn documented_self_hosted_cloud_first_config_round_trips() {
    clear_env();
    let server = MockServer::start().await;

    // The OSS team server's `GET /v1/projects` body: a bare array, not the
    // `{"projects": [...]}` object the deleted resolver expected. Mounted
    // so that a stray lookup would be recorded rather than 404, making the
    // "never requested" assertion below meaningful.
    Mock::given(method("GET"))
        .and(path("/v1/projects"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "id": 1, "slug": "my-awesome-app", "embedding_dim": 896 }
        ])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/projects/my-awesome-app/memory"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                "id": 42,
                "kind": "decision",
                "title": "Use sqlite-vec",
                "body": "no separate vector db",
                "tags": [],
                "linked_files": [],
                "created_at": 1_700_000_000_i64,
                "status": "active",
                "superseded_by": null,
            }])),
        )
        .mount(&server)
        .await;

    let url = non_loopback_alias(&server);
    let be = open_seam(&cloud_first_cfg(&url, "my-awesome-app"), &url)
        .await
        .expect("the documented self-hosted cloud_first config must open");

    let notes = be.list(None, 10, false, None).await.unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].id, NoteId::from_i64(42));
    assert_eq!(notes[0].title, "Use sqlite-vec");

    assert_no_project_lookup(&server).await;
}

#[tokio::test]
#[serial_test::serial]
#[cfg_attr(windows, ignore)]
async fn oss_route_shapes_are_reached_for_every_backend_call() {
    clear_env();
    let server = MockServer::start().await;
    mount_stats(&server, "my-awesome-app", 1).await;

    let note = serde_json::json!({
        "id": 42,
        "kind": "decision",
        "title": "Use sqlite-vec",
        "body": "no separate vector db",
        "tags": [],
        "linked_files": [],
        "created_at": 1_700_000_000_i64,
        "status": "active",
        "superseded_by": null,
    });
    let base = "/v1/projects/my-awesome-app";
    for (verb, route, body) in [
        (
            "POST",
            format!("{base}/memory"),
            serde_json::json!({ "id": 42 }),
        ),
        (
            "GET",
            format!("{base}/memory"),
            serde_json::json!([note.clone()]),
        ),
        ("GET", format!("{base}/memory/42"), note.clone()),
        (
            "POST",
            format!("{base}/memory/search"),
            serde_json::json!([note.clone()]),
        ),
        (
            "POST",
            format!("{base}/memory/42/archive"),
            serde_json::json!({ "changed": true }),
        ),
        (
            "POST",
            format!("{base}/memory/42/supersede"),
            serde_json::json!({ "changed": true }),
        ),
        (
            "GET",
            format!("{base}/memory/harvested-shas"),
            serde_json::json!(["deadbeef"]),
        ),
    ] {
        Mock::given(method(verb))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
    }

    let url = non_loopback_alias(&server);
    let be = open_seam(&cloud_first_cfg(&url, "my-awesome-app"), &url)
        .await
        .unwrap();

    let input = super::NoteInput {
        kind: "decision".to_string(),
        title: "Use sqlite-vec".to_string(),
        body: "no separate vector db".to_string(),
        tags: vec![],
        linked_files: vec![],
        embedding: None,
        source_ref: None,
        valid_at: None,
        supersedes: None,
    };
    assert_eq!(be.add(input).await.unwrap().0, NoteId::from_i64(42));
    assert_eq!(be.list(None, 10, false, None).await.unwrap().len(), 1);
    assert_eq!(
        be.get(NoteId::from_i64(42)).await.unwrap().unwrap().id,
        NoteId::from_i64(42)
    );
    assert_eq!(be.search(&[], "sqlite", 5, None).await.unwrap().len(), 1);
    assert!(be.archive(NoteId::from_i64(42)).await.unwrap());
    assert!(
        be.supersede(NoteId::from_i64(42), NoteId::from_i64(43))
            .await
            .unwrap()
    );
    assert_eq!(be.count().await.unwrap(), 1);
    assert!(be.harvested_shas().await.unwrap().contains("deadbeef"));
}

// `SPELUNK_NO_SLUG_CACHE` gated the deleted resolution cache. It must now
// be inert rather than change any routing decision.
#[tokio::test]
#[serial_test::serial]
#[cfg_attr(windows, ignore)]
async fn no_slug_cache_env_var_is_inert() {
    clear_env();
    let server = MockServer::start().await;
    mount_stats(&server, "my-awesome-app", 9).await;
    let url = non_loopback_alias(&server);

    let mut counts = Vec::new();
    for value in ["1", "0"] {
        unsafe { std::env::set_var("SPELUNK_NO_SLUG_CACHE", value) };
        let be = open_seam(&cloud_first_cfg(&url, "my-awesome-app"), &url)
            .await
            .unwrap();
        counts.push(be.count().await.unwrap());
    }
    unsafe { std::env::remove_var("SPELUNK_NO_SLUG_CACHE") };

    assert_eq!(counts, vec![9, 9]);
    assert!(
        !requested_paths(&server)
            .await
            .contains(&"/v1/projects".to_string())
    );
}

// ADR-071 D2: the bearer is resolved per-origin, so a cloud login must not be
// what reaches a self-hosted `server_url`. Dropping the slug resolver must not
// disturb that: the mock only answers a request carrying the key registered
// for this origin.
#[tokio::test]
#[serial_test::serial]
#[cfg_attr(windows, ignore)]
async fn memory_requests_carry_the_per_origin_bearer() {
    use wiremock::matchers::header;

    clear_env();
    let server = MockServer::start().await;
    let url = non_loopback_alias(&server);

    Mock::given(method("GET"))
        .and(path("/v1/projects/my-awesome-app/stats"))
        .and(header("Authorization", "Bearer sk-team"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "count": 6 })))
        .mount(&server)
        .await;

    let store = crate::config::secret_store::MemoryStore::default();
    crate::config::server_keys::set_key_for_origin(&url, "sk-team", &store).unwrap();
    let cfg = cloud_first_cfg(&url, "my-awesome-app");
    let be = super::open_remote_memory_backend_with_store(&cfg, &url, &store)
        .await
        .unwrap();

    assert_eq!(be.count().await.unwrap(), 6);
}

// A `cloud_first` config with a non-loopback plaintext `http://` `server_url`
// must be rejected by `open_memory_backend` before any bearer token is sent,
// over the memory REST calls (`RemoteMemoryBackend::authed`).
// Mirrors `server_client::transport_validator_rejects_non_loopback_http`.
#[tokio::test]
#[serial_test::serial]
async fn cloud_first_rejects_non_loopback_http() {
    clear_env();
    // Absent this guard nothing would fail here: the open path contacts the
    // server for nothing, so the backend would be constructed and the bearer
    // attached to every subsequent memory call over plaintext.
    let cfg = Config {
        server_url: Some("http://team-server:7777".to_string()),
        project_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
        mode: Some(SyncMode::CloudFirst),
        server_key: Some("secret".to_string()),
        ..Default::default()
    };
    // `map(|_| ())` discards the non-`Debug` `Box<dyn MemoryBackend>` so
    // `expect_err` can format the (never-taken) Ok arm.
    let err = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None)
        .await
        .map(|_| ())
        .expect_err("non-loopback http:// server_url must be rejected before any bearer is sent");
    let msg = err.to_string();
    assert!(
        msg.contains("loopback"),
        "error must name the fix; got: {msg}"
    );
    assert!(msg.contains("https"), "error must name the fix; got: {msg}");
}

// The same guard, against authorities that only look like loopback. These
// reached the remote backend before the authority was parsed rather than
// prefix-matched, which put the bearer on the wire in the clear to the host
// after the `@` or the suffix.
#[tokio::test]
#[serial_test::serial]
async fn cloud_first_rejects_spoofed_loopback_http() {
    clear_env();
    for url in [
        "http://127.0.0.1.evil.example:7777",
        "http://127.0.0.1@evil.example:7777",
        "http://127.0.0.1:1234@evil.example:7777",
    ] {
        let cfg = Config {
            server_url: Some(url.to_string()),
            project_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
            mode: Some(SyncMode::CloudFirst),
            server_key: Some("secret".to_string()),
            ..Default::default()
        };
        let err = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None)
            .await
            .map(|_| ())
            .expect_err("a host that only looks like loopback must be rejected");
        assert!(
            err.to_string().contains("loopback"),
            "{url}: error must name the fix; got: {err}"
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn no_server_kill_switch_forces_local() {
    register_sqlite_vec();
    let cfg = Config {
        server_url: Some("http://team.example.com:7777".to_string()),
        project_id: Some("team/proj".to_string()),
        mode: Some(SyncMode::CloudFirst),
        ..Default::default()
    };
    unsafe { std::env::set_var("SPELUNK_NO_SERVER", "1") };
    let be = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None)
        .await
        .unwrap();
    assert_eq!(
        be.backend_kind(),
        "sqlite",
        "SPELUNK_NO_SERVER=1 forces offline → local backend"
    );
    unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };
}

// ── the peer probe picks the dialect, and never strands the self-hosted one ──

#[tokio::test]
#[serial_test::serial]
#[cfg_attr(windows, ignore)]
async fn a_health_probe_advertising_memory_stream_selects_the_cloud_dialect() {
    clear_env();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["memory", "memory.stream"],
        })))
        .mount(&server)
        .await;

    let url = non_loopback_alias(&server);
    let be = open_seam(&cloud_first_cfg(&url, "proj"), &url)
        .await
        .unwrap();
    assert_eq!(be.backend_kind(), "cloud-api");
}

// A self-hosted server that answers health without the cloud capability, and
// one that cannot answer it at all, must both land on the dialect they used
// before the probe existed.
#[tokio::test]
#[serial_test::serial]
#[cfg_attr(windows, ignore)]
async fn a_team_server_health_probe_keeps_the_self_hosted_dialect() {
    clear_env();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["memory", "index.embed", "search.semantic"],
        })))
        .mount(&server)
        .await;

    let url = non_loopback_alias(&server);
    let be = open_seam(&cloud_first_cfg(&url, "proj"), &url)
        .await
        .unwrap();
    assert_eq!(be.backend_kind(), "remote");
}

#[tokio::test]
#[serial_test::serial]
#[cfg_attr(windows, ignore)]
async fn an_unanswerable_health_probe_keeps_the_self_hosted_dialect() {
    clear_env();
    // No `/v1/health` mounted at all: the probe 404s, which must not change
    // which backend opens.
    let server = MockServer::start().await;
    let url = non_loopback_alias(&server);
    let be = open_seam(&cloud_first_cfg(&url, "proj"), &url)
        .await
        .unwrap();
    assert_eq!(be.backend_kind(), "remote");
}
