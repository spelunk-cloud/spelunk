//! Component tests for `spelunk context` (#206).
//!
//! Tests the porcelain `context` command which serves as the agent session
//! entry point, printing handoffs, open questions, decisions, and requirements
//! in one shot.

use assert_cmd::Command;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

/// Set up a temp project with a mock embedding server, indexed fixture, and
/// pre-seeded memory entries of multiple kinds.
///
/// Returns `(TempDir, db_path, config_path)`.  The TempDir must stay alive
/// for the duration of the test.
fn setup_context_project() -> (TempDir, PathBuf, PathBuf) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let tmp = TempDir::new().expect("create temp dir");
    let db_path = tmp.path().join("spelunk.db");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock_server = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "embedding": vec![0.1f32; 896], "index": 0 }],
                "model": "test-model",
                "object": "list",
                "usage": { "prompt_tokens": 5, "total_tokens": 5 }
            })))
            .mount(&server)
            .await;
        server
    });

    let mock_url = mock_server.uri();
    let config_path = write_config_for_context(tmp.path(), &db_path, &mock_url);

    // The memory DB lives next to the main DB.
    let mem_path = db_path.with_file_name("memory.db");

    // Seed memory entries of different kinds.
    let entries: &[(&str, &str, &str)] = &[
        (
            "handoff",
            "Handoff: session #1",
            "Implemented the context command. Next: tests and docs.",
        ),
        (
            "handoff",
            "Handoff: session #2",
            "Reviewed PR #134. Fixed 5 CI blockers.",
        ),
        (
            "decision",
            "Use sqlite for memory backend",
            "Chose sqlite over git-notes for performance.",
        ),
        (
            "decision",
            "JSONL for plumbing output",
            "All plumbing commands emit JSONL, one object per line.",
        ),
        (
            "decision",
            "Context command design",
            "spelunk context replaces three separate memory list calls for agent workflow.",
        ),
        (
            "question",
            "Should we support remote backends?",
            "Need architect input on remote memory sync priority.",
        ),
        (
            "question",
            "What about pagination?",
            "If we have 1000+ decisions, should context paginate?",
        ),
        (
            "requirement",
            "All commands must support --format json",
            "Porcelain commands need machine-readable output mode.",
        ),
        (
            "requirement",
            "Exit codes follow protocol",
            "0=ok, 1=no-results, 2=error for all plumbing commands.",
        ),
        (
            "note",
            "GitHub Actions CI is flaky on macOS",
            "Intermittent timeouts on macos-14 runner.",
        ),
    ];

    for (kind, title, body) in entries {
        Command::cargo_bin("spelunk")
            .unwrap()
            .arg("--config")
            .arg(&config_path)
            .arg("memory")
            .arg("--db")
            .arg(&mem_path)
            .arg("add")
            .arg("--kind")
            .arg(kind)
            .arg("--title")
            .arg(title)
            .arg("--body")
            .arg(body)
            .assert()
            .success();
    }

    (tmp, db_path, config_path)
}

/// Write a minimal config.toml for the context tests.
fn write_config_for_context(dir: &Path, db_path: &Path, api_base: &str) -> PathBuf {
    let cfg = format!(
        "db_path = {:?}\napi_base_url = {:?}\nembedding_model = \"test-model\"\nllm_model = \"test-chat\"\n",
        db_path, api_base
    );
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, cfg).expect("write config");
    config_path
}

/// Helper to invoke `spelunk context` with the given args and config.
///
/// Does NOT pass `--db`; the command derives `memory.db` from `db_path` in
/// the config, which matches where `setup_context_project` seeds entries.
fn context_cmd(_db_path: &Path, config_path: &Path) -> Command {
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    // Run from the temp dir so find_project_db doesn't walk up and discover
    // the real .spelunk/index.db in the project root.
    if let Some(dir) = config_path.parent() {
        cmd.current_dir(dir);
    }
    cmd.arg("--config").arg(config_path).arg("context");
    cmd
}

// ── happy path: default text output ───────────────────────────────────────────

#[test]
fn context_outputs_all_four_sections_by_default() {
    let (_tmp, db_path, config_path) = setup_context_project();

    let output = context_cmd(&db_path, &config_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);

    // All four default section headers should appear.
    assert!(stdout.contains("Handoffs"), "expected 'Handoffs' header");
    assert!(
        stdout.contains("Open questions"),
        "expected 'Open questions' header"
    );
    assert!(stdout.contains("Decisions"), "expected 'Decisions' header");
    assert!(
        stdout.contains("Requirements"),
        "expected 'Requirements' header"
    );
}

// ── happy path: JSON output ───────────────────────────────────────────────────

#[test]
fn context_json_output_is_valid_object() {
    let (_tmp, db_path, config_path) = setup_context_project();

    let output = context_cmd(&db_path, &config_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);

    // Should be valid JSON object: {"sections": [[kind, notes], ...], "conventions": [...]}
    let obj: serde_json::Value =
        serde_json::from_str(&stdout).expect("--format json should produce valid JSON");
    let parsed = obj["sections"]
        .as_array()
        .expect("sections should be array");

    assert!(!parsed.is_empty(), "expected at least one section");
    for item in parsed {
        let arr = item.as_array().expect("each item should be [kind, notes]");
        assert_eq!(arr.len(), 2, "each item should be a [kind, notes] pair");
        assert!(arr[0].is_string(), "first element should be kind string");
        assert!(arr[1].is_array(), "second element should be notes array");
    }
}

#[test]
fn context_json_includes_all_kinds() {
    let (_tmp, db_path, config_path) = setup_context_project();

    let output = context_cmd(&db_path, &config_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);
    let obj: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let parsed = obj["sections"].as_array().expect("sections array");

    let kinds: Vec<&str> = parsed
        .iter()
        .map(|item| item[0].as_str().unwrap_or(""))
        .collect();

    assert!(kinds.contains(&"handoff"), "should include handoff section");
    assert!(
        kinds.contains(&"decision"),
        "should include decision section"
    );
    assert!(
        kinds.contains(&"question"),
        "should include question section"
    );
    assert!(
        kinds.contains(&"requirement"),
        "should include requirement section"
    );
}

// ── kind filter ───────────────────────────────────────────────────────────────

#[test]
fn context_kind_filter_shows_only_requested_kind() {
    let (_tmp, db_path, config_path) = setup_context_project();

    let output = context_cmd(&db_path, &config_path)
        .arg("--kind")
        .arg("decision")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);

    // Should show Decisions header but not Handoffs.
    assert!(
        stdout.contains("Decisions"),
        "should show Decisions when --kind decision"
    );
    assert!(
        !stdout.contains("Handoffs"),
        "should NOT show Handoffs when --kind decision"
    );
    assert!(
        !stdout.contains("Open questions"),
        "should NOT show questions"
    );
    assert!(
        !stdout.contains("Requirements"),
        "should NOT show requirements"
    );
}

#[test]
fn context_kind_filter_json_returns_single_section() {
    let (_tmp, db_path, config_path) = setup_context_project();

    let output = context_cmd(&db_path, &config_path)
        .arg("--kind")
        .arg("question")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);
    let obj: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let parsed = obj["sections"].as_array().expect("sections array");

    assert_eq!(parsed.len(), 1, "--kind should return exactly one section");
    assert_eq!(parsed[0][0].as_str().unwrap(), "question");
}

// ── limit flag ────────────────────────────────────────────────────────────────

#[test]
fn context_limit_flag_respects_count() {
    let (_tmp, db_path, config_path) = setup_context_project();

    let output = context_cmd(&db_path, &config_path)
        .arg("--kind")
        .arg("decision")
        .arg("--limit")
        .arg("2")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);
    let obj: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let parsed = obj["sections"].as_array().expect("sections array");

    let notes = parsed[0][1].as_array().expect("notes should be array");
    assert_eq!(
        notes.len(),
        2,
        "--limit 2 should return exactly 2 entries, got {}",
        notes.len()
    );
}

// ── default limits ────────────────────────────────────────────────────────────

#[test]
fn context_default_limits_respected() {
    let (_tmp, db_path, config_path) = setup_context_project();

    // We have 2 handoffs. Default handoff limit is 3, so both should appear.
    let output = context_cmd(&db_path, &config_path)
        .arg("--kind")
        .arg("handoff")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);
    let obj: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let parsed = obj["sections"].as_array().expect("sections array");

    let notes = parsed[0][1].as_array().expect("notes should be array");
    assert_eq!(
        notes.len(),
        2,
        "default handoff limit of 3 should include both entries"
    );
}

// ── empty memory / no results ─────────────────────────────────────────────────

#[test]
fn context_empty_memory_exits_zero_with_no_output() {
    // Fresh project with no memory entries at all.
    let tmp = TempDir::new().expect("create temp dir");
    let db_path = tmp.path().join("spelunk.db");
    let config_path = write_config_for_context(tmp.path(), &db_path, "http://127.0.0.1:19999");

    // Write a valid but empty memory.db so the backend can open.
    // MemoryStore::open creates the DB on demand, but context doesn't
    // create the DB if it doesn't exist.  `spelunk memory add` will
    // create it, then we delete the entries — but that's complex.
    //
    // Instead, just create a minimal config and let the backend handle
    // the empty case.  The command should exit 0 with minimal output.
    let output = context_cmd(&db_path, &config_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);
    let obj: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let parsed = obj["sections"].as_array().expect("sections array");

    // All sections should be present but empty.
    for item in parsed {
        let notes = item[1].as_array().expect("notes should be array");
        assert!(
            notes.is_empty(),
            "section with no entries should have empty array"
        );
    }
}

// ── exit codes ────────────────────────────────────────────────────────────────

#[test]
fn context_exits_zero_on_success() {
    let (_tmp, db_path, config_path) = setup_context_project();

    context_cmd(&db_path, &config_path).assert().code(0);
}

#[test]
fn context_exits_zero_with_kind_filter() {
    let (_tmp, db_path, config_path) = setup_context_project();

    context_cmd(&db_path, &config_path)
        .arg("--kind")
        .arg("decision")
        .assert()
        .code(0);
}

#[test]
fn context_exits_zero_when_kind_has_no_entries() {
    // We have no "intent" entries in the seed data, but the command
    // should still exit 0 — empty results are not an error for porcelain.
    let (_tmp, db_path, config_path) = setup_context_project();

    context_cmd(&db_path, &config_path)
        .arg("--kind")
        .arg("intent")
        .assert()
        .code(0);
}

// ── backend flag ──────────────────────────────────────────────────────────────

#[test]
fn context_explicit_sqlite_backend_works() {
    let (_tmp, db_path, config_path) = setup_context_project();

    context_cmd(&db_path, &config_path)
        .arg("--backend")
        .arg("sqlite")
        .arg("--format")
        .arg("json")
        .assert()
        .success();
}

// ── JSON output structural assertions ─────────────────────────────────────────

#[test]
fn context_json_notes_have_required_fields() {
    let (_tmp, db_path, config_path) = setup_context_project();

    let output = context_cmd(&db_path, &config_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);
    let obj: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let parsed = obj["sections"].as_array().expect("sections array");

    for item in parsed {
        let notes = item[1].as_array().expect("notes should be array");
        for note in notes {
            assert!(note.get("id").is_some(), "note missing 'id': {note}");
            assert!(note.get("kind").is_some(), "note missing 'kind': {note}");
            assert!(note.get("title").is_some(), "note missing 'title': {note}");
            assert!(note.get("body").is_some(), "note missing 'body': {note}");
        }
    }
}

// ── error path: bad config ────────────────────────────────────────────────────

#[test]
fn context_exits_nonzero_when_config_invalid() {
    // A config with a db_path inside a non-existent directory that cannot be
    // created (deeply nested under a non-existent root) will cause MemoryStore
    // to fail its create_dir_all, producing a non-zero exit.
    let tmp = TempDir::new().expect("create temp dir");
    let config_path = tmp.path().join("config.toml");
    // Use /dev/null/impossible/path — /dev/null is a file, so mkdir fails.
    let db_path = std::path::Path::new("/dev/null/impossible/path/spelunk.db");

    std::fs::write(
        &config_path,
        format!("db_path = {:?}\napi_base_url = \"http://127.0.0.1:19999\"\nembedding_model = \"test\"\nllm_model = \"test\"\n", db_path),
    )
    .expect("write config");

    Command::cargo_bin("spelunk")
        .unwrap()
        .current_dir(&tmp)
        .arg("--config")
        .arg(&config_path)
        .arg("context")
        .assert()
        .failure();
}

// ── format flag validation ────────────────────────────────────────────────────

#[test]
fn context_unknown_format_falls_back_to_text() {
    // Unknown formats should fall back to text output (not crash).
    let (_tmp, db_path, config_path) = setup_context_project();

    context_cmd(&db_path, &config_path)
        .arg("--format")
        .arg("yaml")
        .assert()
        .success();
}
