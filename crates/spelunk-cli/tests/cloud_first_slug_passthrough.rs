// `cloud_first` against a self-hosted team server, driven through the real
// binary against a real TLS peer.
//
// The configured `project_id` must reach the server as the project path
// segment exactly as written, and the open path must contact the server for
// nothing else first. The one unacceptable outcome is a silent fall back to
// local data.
//
// A non-loopback `server_url` must be `https://`, so this drives a real rustls
// listener, as `tls_trust.rs` does, and addresses it via the non-loopback
// `0.0.0.0` alias of the same socket.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const LOCAL_TITLE: &str = "local only entry";
const SERVER_TITLE: &str = "entry that only exists on the server";
const PROJECT_SLUG: &str = "github.com/owner/repo";

// ── cert generation (SAN 0.0.0.0, the non-loopback alias) ────────────────────

struct TestCa {
    cert_pem: String,
    issuer: Issuer<'static, KeyPair>,
}

fn new_ca() -> TestCa {
    let mut params = CertificateParams::new(vec!["0.0.0.0".to_string()]).expect("valid CA SAN");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "spelunk-slug-passthrough-test CA");
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    params.key_usages.push(KeyUsagePurpose::CrlSign);

    let key_pair = KeyPair::generate().expect("generate CA key");
    let cert = params.clone().self_signed(&key_pair).expect("self-sign CA");
    let cert_pem = cert.pem();
    let issuer = Issuer::new(params, key_pair);
    TestCa { cert_pem, issuer }
}

fn new_leaf(issuer: &Issuer<'static, KeyPair>) -> (String, String) {
    let mut params = CertificateParams::new(vec!["0.0.0.0".to_string()]).expect("valid leaf SAN");
    params
        .distinguished_name
        .push(DnType::CommonName, "0.0.0.0");
    params.use_authority_key_identifier_extension = true;
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ServerAuth);

    let key_pair = KeyPair::generate().expect("generate leaf key");
    let cert = params
        .signed_by(&key_pair, issuer)
        .expect("sign leaf with CA");
    (cert.pem(), key_pair.serialize_pem())
}

// ── TLS listener ─────────────────────────────────────────────────────────────

// `paths` holds every request the listener saw, so a reintroduced pre-flight
// would show up rather than pass silently. `memory_segments` holds the project
// path segment as the server decoded it, which is what proves the configured
// `project_id` travelled verbatim.
#[derive(Default)]
struct Seen {
    paths: Mutex<Vec<String>>,
    memory_segments: Mutex<Vec<String>>,
}

impl Seen {
    fn paths(&self) -> Vec<String> {
        self.paths.lock().expect("seen lock").clone()
    }

    fn memory_segments(&self) -> Vec<String> {
        self.memory_segments.lock().expect("seen lock").clone()
    }
}

// Spawn a TLS listener on 127.0.0.1 (reachable as 0.0.0.0) answering `memory`
// on the OSS team server's project-scoped memory list route, and recording
// anything else it is asked for. Detached thread, no separate process: it dies
// with the test binary.
//
// The port is taken from a listener that is already bound and listening before
// this returns, and that same socket is handed to the server, so nothing can
// claim the port in between and the sleep below is a courtesy rather than a
// correctness requirement: the kernel accepts connections into the backlog from
// the moment of bind.
fn spawn_tls_server(
    cert_pem: String,
    key_pem: String,
    memory: serde_json::Value,
    seen: Arc<Seen>,
) -> u16 {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind tls listener");
    let port = std_listener.local_addr().expect("local_addr").port();
    std_listener
        .set_nonblocking(true)
        .expect("set listener nonblocking");

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime for tls test server");
        rt.block_on(async move {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let config = axum_server::tls_rustls::RustlsConfig::from_pem(
                cert_pem.into_bytes(),
                key_pem.into_bytes(),
            )
            .await
            .expect("build rustls config from generated cert/key");

            let memory_seen = Arc::clone(&seen);
            let fallback_seen = Arc::clone(&seen);
            let app = axum::Router::new()
                .route(
                    "/v1/projects/{project_id}/memory",
                    axum::routing::get(
                        move |axum::extract::Path(project_id): axum::extract::Path<String>,
                              uri: axum::http::Uri| {
                            let seen = Arc::clone(&memory_seen);
                            let body = memory.clone();
                            async move {
                                seen.paths
                                    .lock()
                                    .expect("seen lock")
                                    .push(uri.path().to_string());
                                seen.memory_segments
                                    .lock()
                                    .expect("seen lock")
                                    .push(project_id);
                                axum::Json(body)
                            }
                        },
                    ),
                )
                .fallback(move |uri: axum::http::Uri| {
                    let seen = Arc::clone(&fallback_seen);
                    async move {
                        seen.paths
                            .lock()
                            .expect("seen lock")
                            .push(uri.path().to_string());
                        axum::Json(serde_json::json!({}))
                    }
                });
            axum_server::from_tcp_rustls(std_listener, config)
                .expect("adopt std listener for tls")
                .serve(app.into_make_service())
                .await
                .expect("serve tls listener");
        });
    });

    std::thread::sleep(std::time::Duration::from_millis(150));
    port
}

// The OSS team server's memory list body: a bare array of i64-keyed entries.
fn oss_memory_list() -> serde_json::Value {
    serde_json::json!([{
        "id": 42,
        "kind": "note",
        "title": SERVER_TITLE,
        "body": "b",
        "tags": [],
        "linked_files": [],
        "created_at": 1_700_000_000_i64,
        "status": "active",
        "superseded_by": null,
    }])
}

// ── project setup ────────────────────────────────────────────────────────────

fn write_cfg(dir: &Path, name: &str, db_path: &Path, extra: &str) -> PathBuf {
    let cfg = format!(
        "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1\"\n\
         llm_model = \"test-chat\"\n{extra}",
        db_path
    );
    let path = dir.join(name);
    std::fs::write(&path, cfg).expect("write config");
    path
}

// Seed one entry into the local store, so a silent local fallback would be
// visible on stdout rather than indistinguishable from an empty result.
fn seeded_project() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let mem_path = db_path.with_file_name("memory.db");
    let cfg = write_cfg(tmp.path(), "config-seed.toml", &db_path, "");
    let out = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args([
            "add",
            "--kind",
            "note",
            "--title",
            LOCAL_TITLE,
            "--body",
            "b",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    (tmp, mem_path)
}

// Build the `cloud_first` project: `mode` is read only from the global config,
// `server_url` / `project_id` only from the project-level `.spelunk/config.toml`.
fn cloud_first_project(ca_pem: &str, port: u16, project_id: &str) -> (TempDir, PathBuf, PathBuf) {
    let (tmp, mem_path) = seeded_project();
    let ca_path = tmp.path().join("ca.pem");
    std::fs::write(&ca_path, ca_pem).expect("write ca pem");

    let cfg = write_cfg(
        tmp.path(),
        "config-cloud-first.toml",
        &tmp.path().join("spelunk.db"),
        &format!(
            "mode = \"cloud_first\"\nserver_ca = {:?}\n",
            ca_path.display().to_string()
        ),
    );
    let server_url = format!("https://0.0.0.0:{port}");
    // Without this the test would keep passing if `0.0.0.0` were ever
    // reclassified as loopback, while silently no longer covering the
    // non-loopback peer it exists to prove.
    assert!(
        !spelunk_core::config::is_loopback_url(&server_url),
        "test seam precondition: {server_url} must be classified non-loopback"
    );
    plumbing_helpers::write_project_server_config(tmp.path(), &server_url, project_id);
    (tmp, mem_path, cfg)
}

fn memory_list(tmp: &TempDir, cfg: &Path, mem_path: &Path) -> std::process::Output {
    spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(cfg)
        .args(["memory", "--db"])
        .arg(mem_path)
        .args(["list", "--format", "json"])
        .output()
        .unwrap()
}

// End to end through the binary: a self-hosted OSS-shaped server opens under
// `cloud_first`, reads come from it rather than from the local store, and the
// configured slug arrives as the project path segment exactly as written. A
// slug containing `/` is used deliberately: it must survive as one
// percent-encoded segment and decode back to the original on the server.
//
// Connecting to `0.0.0.0` raises `WSAEADDRNOTAVAIL` (os error 10049) on Windows,
// which is why every `0.0.0.0`-addressed test in `spelunk-core` carries the same
// attribute. CI runs this suite on windows-latest, so it is required here too.
#[test]
#[cfg_attr(windows, ignore)]
fn cloud_first_reads_remotely_with_the_configured_slug_verbatim() {
    let ca = new_ca();
    let (leaf_pem, leaf_key) = new_leaf(&ca.issuer);
    let seen = Arc::new(Seen::default());
    let port = spawn_tls_server(leaf_pem, leaf_key, oss_memory_list(), Arc::clone(&seen));

    let (tmp, mem_path, cfg) = cloud_first_project(&ca.cert_pem, port, PROJECT_SLUG);
    let out = memory_list(&tmp, &cfg, &mem_path);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "the documented self-hosted cloud_first config must work; stderr: {stderr}"
    );
    assert!(
        stdout.contains(SERVER_TITLE),
        "reads must come from the server in cloud_first: {stdout}"
    );
    assert!(
        !stdout.contains(LOCAL_TITLE),
        "the local store must not be read in cloud_first: {stdout}"
    );
    assert_eq!(
        seen.memory_segments(),
        vec![PROJECT_SLUG.to_string()],
        "the configured project_id must reach the server verbatim, in one segment"
    );
    // `/v1/health` is the peer probe that picks the memory dialect; it is
    // issued on every open by design. What this pins is that no *project*
    // lookup happens and the read itself is a single request.
    assert_eq!(
        seen.paths()
            .into_iter()
            .filter(|p| p != "/v1/health")
            .collect::<Vec<_>>(),
        vec!["/v1/projects/github.com%2Fowner%2Frepo/memory".to_string()],
        "the memory read must be the only memory request the mode makes"
    );
}
