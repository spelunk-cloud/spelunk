// Subprocess-level coverage for `relates_to` edge propagation on `spelunk
// sync`. `memory add --relates-to` writes a LOCAL `relates_to` edge; this pins
// that the edge now also travels UP to the cloud on sync, via an edge-only
// `POST /memory/batch` keyed by each endpoint's external_id.
//
// The real compiled `spelunk` binary (`assert_cmd`) drives `memory add
// --relates-to` to build the local edge, then `sync` against a mock team
// server. The mock echoes the entry push (so both endpoints get stamped and
// enter this round's just-synced set) and captures the edge-only batch, which
// this test asserts carries `kind: "relates_to"` and the two entries' uuids.
// Following `memory_push_sync_partial_failure.rs` for the mock-server + config
// harness, and `memory_relates_to_edge.rs` for the `memory add` driving.

mod plumbing_helpers;
use plumbing_helpers::{register_sqlite_vec, spelunk_bin_in, write_project_server_config};

use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

// A slug with no characters `encode_project_id` would percent-encode, so the
// mocked route paths match literally.
const PROJECT_SLUG: &str = "acme-widget";

// Stand-in for the cloud batch route: echoes each pushed entry as `created`
// (with a cloud id, so the local row is stamped and its edges become
// pushable), and acknowledges an edge-only batch as one `created` edge each.
struct BatchEcho;
impl Respond for BatchEcho {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value =
            serde_json::from_slice(&request.body).unwrap_or(serde_json::json!({}));
        let entries = body["entries"].as_array().cloned().unwrap_or_default();
        if !entries.is_empty() {
            let results: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    let ext = e["external_id"].as_str().unwrap_or_default();
                    serde_json::json!({
                        "status": "created", "external_id": ext, "id": format!("cloud-{ext}")
                    })
                })
                .collect();
            return ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": results.len(), "skipped": 0, "failed": 0, "results": results
            }));
        }
        let edges = body["edges"].as_array().cloned().unwrap_or_default();
        let acks: Vec<serde_json::Value> = edges
            .iter()
            .map(|_| serde_json::json!({"status": "created"}))
            .collect();
        ResponseTemplate::new(207).set_body_json(serde_json::json!({ "edges": acks }))
    }
}

async fn mount_health(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["memory"],
        })))
        .mount(server)
        .await;
}

async fn mount_batch(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT_SLUG}/memory/batch")))
        .respond_with(BatchEcho)
        .mount(server)
        .await;
}

// The post-push pull half of `sync` must have something to talk to.
async fn mount_since_empty(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path_regex(format!(
            r"^/v1/projects/{PROJECT_SLUG}/memory/since$"
        )))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "entries": [] })),
        )
        .mount(server)
        .await;
}

// Global personal config: local memory writes, git-notes carrier off.
fn write_global_config(dir: &Path) -> std::path::PathBuf {
    let db_path = dir.join(".spelunk").join("index.db");
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "db_path = {db_path:?}\n\
             llm_model = \"test-chat\"\n\
             store_in_git_notes = false\n"
        ),
    )
    .expect("write global config.toml");
    config_path
}

// Run `memory add --kind note --title <title> --body … <extra…>` against a
// local memory.db, from a neutral CWD (no project `.spelunk`, so `add` stays
// local and never sees the team server_url). Returns the printed entry id.
fn add_note(
    home: &Path,
    cwd: &Path,
    cfg: &Path,
    mem_db: &Path,
    title: &str,
    extra: &[&str],
) -> i64 {
    let mut cmd = spelunk_bin_in(home);
    cmd.current_dir(cwd)
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(cfg)
        .arg("memory")
        .arg("--db")
        .arg(mem_db)
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg(title)
        .arg("--body")
        .arg("body text with no secrets");
    for a in extra {
        cmd.arg(a);
    }
    let out = cmd.output().expect("run memory add");
    assert!(
        out.status.success(),
        "memory add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    parse_stored_id(&String::from_utf8_lossy(&out.stdout))
}

fn parse_stored_id(stdout: &str) -> i64 {
    let hash = stdout
        .find('#')
        .unwrap_or_else(|| panic!("no id marker in stored output: {stdout:?}"));
    let rest = &stdout[hash + 1..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end]
        .parse()
        .unwrap_or_else(|_| panic!("could not parse id from stored output: {stdout:?}"))
}

fn uuid_of(mem_db: &Path, id: i64) -> String {
    register_sqlite_vec();
    let conn = rusqlite::Connection::open(mem_db).expect("open memory db");
    conn.query_row("SELECT uuid FROM notes WHERE id = ?1", [id], |r| {
        r.get::<_, Option<String>>(0)
    })
    .expect("read uuid")
    .expect("uuid must be minted after sync")
}

#[tokio::test]
async fn sync_pushes_a_local_relates_to_edge_to_the_cloud() {
    let server = MockServer::start().await;
    mount_health(&server).await;
    mount_batch(&server).await;
    mount_since_empty(&server).await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let mem_dir = TempDir::new().unwrap();
    let mem_db = mem_dir.path().join("memory.db");

    let cfg = write_global_config(proj.path());
    write_project_server_config(proj.path(), &server.uri(), PROJECT_SLUG);

    // Build the local edge exactly as a user would: a target entry, then a
    // linker entry that `--relates-to` it (records linker -> target).
    let target = add_note(
        home.path(),
        mem_dir.path(),
        &cfg,
        &mem_db,
        "Original observation",
        &[],
    );
    let linker = add_note(
        home.path(),
        mem_dir.path(),
        &cfg,
        &mem_db,
        "Follow-up observation",
        &["--relates-to", &target.to_string()],
    );

    // Sync from the project dir, so `server_url` + `project_id` are discovered
    // from its `.spelunk/config.toml`.
    let assert = spelunk_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&cfg)
        .args(["sync", "--source"])
        .arg(&mem_db)
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.contains("Sync complete"),
        "sync must succeed: {stdout:?}"
    );

    // The two entries' cloud external_ids (their local uuids, minted on sync).
    let from_ext = uuid_of(&mem_db, linker);
    let to_ext = uuid_of(&mem_db, target);

    // Exactly one edge-only `/memory/batch` was posted, keyed by external_id.
    let reqs = server.received_requests().await.unwrap();
    let edge_bodies: Vec<serde_json::Value> = reqs
        .iter()
        .filter_map(|r| {
            let body: serde_json::Value = serde_json::from_slice(&r.body).ok()?;
            let edges = body.get("edges")?.as_array()?;
            (!edges.is_empty()).then_some(body)
        })
        .collect();
    assert_eq!(
        edge_bodies.len(),
        1,
        "sync must post exactly one relates_to edge batch; requests: {:?}",
        reqs.iter()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .collect::<Vec<_>>()
    );
    let edge = &edge_bodies[0]["edges"][0];
    assert_eq!(edge["kind"], "relates_to");
    assert_eq!(
        edge["from_external_id"],
        serde_json::Value::String(from_ext)
    );
    assert_eq!(edge["to_external_id"], serde_json::Value::String(to_ext));
    assert_eq!(
        edge_bodies[0]["entries"].as_array().map(Vec::len),
        Some(0),
        "an edge batch carries no entries"
    );
}
