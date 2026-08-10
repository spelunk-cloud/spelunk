// Conformance tests for the plumbing JSONL stability contract (docs/stability.md).
//
// Each test runs a real plumbing command and checks its emitted JSONL against
// the committed golden schema. Field presence and types only: a removal,
// rename, or retype fails here; an added field passes.
//
// The checker's own accept/reject behaviour is pinned separately, in
// `schema_contract_checker.rs`.

mod plumbing_helpers;
mod schema_contract;

use plumbing_helpers::{
    FIXTURE_PROJECT_ID, IndexEmbedResponder, index_fixture_project, parse_jsonl, spelunk_bin,
    spelunk_cmd, write_config, write_project_server_config,
};
use schema_contract::{CommandSchema, assert_conforms, check_rows, load_golden};

use std::collections::BTreeSet;
use std::path::Path;
use tempfile::TempDir;

// The dimension the mock embedder in `plumbing_helpers` returns, and therefore
// the width of every vector in a fixture-backed index.
const FIXTURE_EMBEDDING_DIM: usize = 896;

fn schema_for(command: &str) -> CommandSchema {
    load_golden()
        .remove(command)
        .unwrap_or_else(|| panic!("golden schema has no entry for `{command}`"))
}

fn check(command: &str, stdout: &[u8]) {
    let schema = schema_for(command);
    let rows = parse_jsonl(stdout);
    assert_conforms(command, &schema, &rows);
    assert_every_declared_field_is_load_bearing(command, &schema, &rows);
}

fn sorted_keys(row: &serde_json::Value) -> BTreeSet<String> {
    row.as_object()
        .expect("row is a JSON object")
        .keys()
        .cloned()
        .collect()
}

// `assert_conforms` proves the checker accepts what the command emits today. It
// cannot prove the check would fail if the command stopped emitting a field,
// and that is the guarantee the contract rests on. So replay the command's real
// rows with one declared field broken at a time and require an objection each
// time. Verifying this by hand on one field of one command says nothing about
// the rest, which is why it runs per command, per field, on live output.
fn assert_every_declared_field_is_load_bearing(
    command: &str,
    schema: &CommandSchema,
    rows: &[serde_json::Value],
) {
    let mutate = |field: &str, replacement: Option<&serde_json::Value>| -> Vec<serde_json::Value> {
        rows.iter()
            .map(|row| {
                let mut row = row.clone();
                let obj = row.as_object_mut().expect("row is a JSON object");
                match replacement {
                    Some(value) => obj.insert(field.to_string(), value.clone()),
                    None => obj.remove(field),
                };
                row
            })
            .collect()
    };

    for field in schema.required.keys() {
        assert!(
            !check_rows(schema, &mutate(field, None)).is_empty(),
            "`{command}`: the contract declares `{field}` required, but dropping it from real \
             output raised nothing, so nothing would catch the code dropping it"
        );
    }

    // Optional fields are exempt from presence, never from type, and only the
    // ones this run actually produced can be mutated.
    let declared = schema.required.iter().chain(schema.optional.iter());
    for (field, ty) in declared {
        if !rows.iter().any(|row| row.get(field).is_some()) {
            continue;
        }
        let wrong = ty.counterexample().unwrap_or_else(|| {
            panic!(
                "`{command}`: `{field}` is declared `{}`, which accepts every value and so \
                    checks nothing",
                ty.spelling()
            )
        });
        assert!(
            !check_rows(schema, &mutate(field, Some(&wrong))).is_empty(),
            "`{command}`: retyping `{field}` to {wrong} in real output raised nothing"
        );
    }
}

// ── the contract covers every command that exists ────────────────────────────

// A new plumbing command that ships without a golden entry is an unguarded
// stable surface, which is the failure this whole suite exists to prevent. The
// command list comes from clap's own help rather than a second hand-maintained
// list, so it cannot drift from the binary.
#[test]
fn golden_schema_covers_every_plumbing_subcommand() {
    let help = spelunk_bin()
        .args(["plumbing", "--help"])
        .output()
        .expect("run plumbing --help");
    let help = String::from_utf8(help.stdout).expect("help is utf-8");

    let commands_section = help
        .split_once("Commands:")
        .expect("plumbing --help lists a Commands: section")
        .1;

    let mut shipped: Vec<String> = Vec::new();
    for line in commands_section.lines() {
        // Subcommand rows are indented and start with the command name. The
        // section ends at the blank line before `Options:`, but it also *opens*
        // with one, so an empty line only terminates once a row has been seen.
        if line.trim().is_empty() {
            if shipped.is_empty() {
                continue;
            }
            break;
        }
        let Some(name) = line.split_whitespace().next() else {
            continue;
        };
        if name == "help" {
            continue;
        }
        shipped.push(name.to_string());
    }
    shipped.sort();
    assert!(
        !shipped.is_empty(),
        "parsed no subcommands out of `plumbing --help`; the parser needs updating"
    );

    let mut declared: Vec<String> = load_golden().keys().cloned().collect();
    declared.sort();

    assert_eq!(
        shipped,
        declared,
        "every plumbing command must declare a JSONL schema in {}",
        schema_contract::GOLDEN_RELATIVE_PATH
    );
}

// ── index-backed commands ────────────────────────────────────────────────────

#[test]
fn cat_chunks_output_matches_the_contract() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let out = spelunk_cmd(&db_path, &config_path)
        .args(["cat-chunks", "src/lib.rs"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    check("cat-chunks", &out);
}

#[test]
fn ls_files_output_matches_the_contract() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let out = spelunk_cmd(&db_path, &config_path)
        .arg("ls-files")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    check("ls-files", &out);
}

#[test]
fn hash_file_output_matches_the_contract() {
    let (_tmp, db_path, config_path) = index_fixture_project();
    let file = plumbing_helpers::fixture_path().join("src/lib.rs");

    let out = spelunk_cmd(&db_path, &config_path)
        .arg("hash-file")
        .arg(&file)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    check("hash-file", &out);
}

#[test]
fn graph_edges_output_matches_the_contract() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    // `main.rs` calls into `lib.rs`, so the edge table is non-empty for it.
    // Asserting success (not "success or exit 1") keeps this from degrading
    // into a test that passes by never checking anything.
    let out = spelunk_cmd(&db_path, &config_path)
        .args(["graph-edges", "--file", "src/main.rs"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    check("graph-edges", &out);
}

#[test]
fn knn_output_matches_the_contract() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    // The mock embedder gives every chunk the same vector, so any query of the
    // right width matches everything. Ordering is meaningless here; the schema
    // is not.
    let payload = serde_json::json!({
        "model": "test-model",
        "dimensions": FIXTURE_EMBEDDING_DIM,
        "vector": vec![0.1f32; FIXTURE_EMBEDDING_DIM],
    });

    let out = spelunk_cmd(&db_path, &config_path)
        .arg("knn")
        .write_stdin(payload.to_string())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    check("knn", &out);
}

// `knn` has no Rust type matching its wire shape: it serialises `SearchResult`
// to a `Value` and splices `score` into the map, so the compiler cannot keep it
// in step with `cat-chunks`, which serialises the same struct directly. Two
// hand-written golden entries can drift apart while both stay green. Comparing
// the live key sets pins the relationship the golden only states in a comment,
// and asserting the derivation gives the spliced field a meaning beyond "some
// number".
#[test]
fn knn_is_the_cat_chunks_shape_plus_a_derived_score() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let payload = serde_json::json!({
        "model": "test-model",
        "dimensions": FIXTURE_EMBEDDING_DIM,
        "vector": vec![0.1f32; FIXTURE_EMBEDDING_DIM],
    });
    let knn_out = spelunk_cmd(&db_path, &config_path)
        .arg("knn")
        .write_stdin(payload.to_string())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let cat_out = spelunk_cmd(&db_path, &config_path)
        .args(["cat-chunks", "src/lib.rs"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let knn_rows = parse_jsonl(&knn_out);
    let cat_rows = parse_jsonl(&cat_out);
    assert!(!knn_rows.is_empty() && !cat_rows.is_empty());

    let mut expected = sorted_keys(&cat_rows[0]);
    expected.insert("score".to_string());
    assert_eq!(
        sorted_keys(&knn_rows[0]),
        expected,
        "`knn` must emit exactly the `cat-chunks` fields plus `score`; a divergence means the \
         two SearchResult surfaces have drifted"
    );

    for row in &knn_rows {
        let distance = row["distance"].as_f64().expect("distance is a number");
        let score = row["score"].as_f64().expect("score is a number");
        assert!(
            (score - (1.0 - distance)).abs() < 1e-6,
            "score must be 1 - distance as the contract states, got score {score} against \
             distance {distance}"
        );
    }
}

// ── commands that need no index ──────────────────────────────────────────────

#[test]
fn parse_file_output_matches_the_contract() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("index.db");
    let config_path = write_config(tmp.path(), &db_path, "http://127.0.0.1:1");
    let file = plumbing_helpers::fixture_path().join("src/lib.rs");

    // `parse-file` returns before the index-exists check, so an absent DB here
    // is deliberate: it proves the command really is index-free.
    let out = spelunk_cmd(&db_path, &config_path)
        .arg("parse-file")
        .arg(&file)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    check("parse-file", &out);
}

#[tokio::test]
async fn embed_output_matches_the_contract() {
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["index.embed", "search.semantic"],
        })))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock)
        .await;

    let tmp = TempDir::new().unwrap();
    let config = tmp.path().join("config.toml");
    std::fs::write(&config, "mode = \"cloud_first\"\n").unwrap();
    write_project_server_config(tmp.path(), &mock.uri(), FIXTURE_PROJECT_ID);

    let out = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&config)
        .args(["plumbing", "embed"])
        .write_stdin("fn greet(name: &str) -> String\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    check("embed", &out);
}

#[test]
fn read_memory_output_matches_the_contract() {
    // `read-memory` derives the memory path from `--db`, and the index-exists
    // check in the plumbing dispatcher runs first, so a real index has to sit
    // next to the memory store even though this command never reads it.
    let (tmp, db_path, config_path) = index_fixture_project();
    let mem_path = db_path.with_file_name("memory.db");

    // The git-notes carrier follows the process CWD and ignores `--db`, so this
    // runs in the temp dir rather than the repo under test.
    spelunk_bin()
        .current_dir(tmp.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_path)
        .args([
            "add",
            "--kind",
            "decision",
            "--title",
            "Plumbing JSONL fields are semver-bound",
            "--body",
            "Removing a field is a breaking change.",
        ])
        .assert()
        .success();

    let out = spelunk_cmd(&db_path, &config_path)
        .arg("read-memory")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    check("read-memory", &out);
}

// `publish-notes` emits three untyped `json!` shapes, so like `knn` it has no
// Rust type the compiler can hold to the contract. Each shape is produced from
// a real repository state rather than hand-built, and each returns its stdout.

fn seed_a_note(repo: &Path, title: &str) {
    spelunk_bin()
        .current_dir(repo)
        .env("SPELUNK_NO_SERVER", "1")
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            title,
            "--body",
            "The pre-push hook drops stdout.",
        ])
        .assert()
        .success();
}

fn publish_notes_stdout(repo: &Path, extra: &[&str]) -> Vec<u8> {
    let mut cmd = spelunk_bin();
    cmd.current_dir(repo).env("SPELUNK_NO_SERVER", "1").args([
        "plumbing",
        "publish-notes",
        "origin",
    ]);
    cmd.args(extra)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone()
}

// No `refs/notes/spelunk` in a fresh repo, so this takes the skip branch, which
// is the outcome shape reachable without a remote.
fn skip_shape(tmp: &Path) -> Vec<u8> {
    let repo = tmp.join("skip-repo");
    std::fs::create_dir_all(&repo).unwrap();
    plumbing_helpers::init_git_repo(&repo);
    publish_notes_stdout(&repo, &[])
}

// A real `memory add` writes the notes ref, so the published shape is reached
// through the same path a user takes.
fn published_shape(tmp: &Path) -> Vec<u8> {
    let repo = tmp.join("published-repo");
    let remote = tmp.join("published-remote.git");
    std::fs::create_dir_all(&repo).unwrap();
    plumbing_helpers::init_git_repo(&repo);
    init_bare_remote(&remote);
    git_in(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    seed_a_note(&repo, "Publish-notes emits its outcome as JSONL");
    publish_notes_stdout(&repo, &[])
}

// A remote that resolves but cannot be pushed to, tolerated with
// `--best-effort`: the only route to the error shape that still exits 0.
fn best_effort_error_shape(tmp: &Path) -> Vec<u8> {
    let repo = tmp.join("error-repo");
    let broken = tmp.join("error-not-a-repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&broken).unwrap();
    plumbing_helpers::init_git_repo(&repo);
    git_in(
        &repo,
        &["remote", "add", "origin", broken.to_str().unwrap()],
    );
    seed_a_note(&repo, "A tolerated publish failure is still reported");
    publish_notes_stdout(&repo, &["--best-effort"])
}

#[test]
fn publish_notes_skip_output_matches_the_contract() {
    let tmp = TempDir::new().unwrap();
    let out = skip_shape(tmp.path());

    let rows = parse_jsonl(&out);
    assert_eq!(rows.len(), 1, "publish-notes emits exactly one object");
    assert_eq!(
        rows[0].get("published").and_then(|v| v.as_bool()),
        Some(false),
        "a repo with no notes ref has nothing to publish"
    );
    assert!(
        rows[0].get("skipped").is_some(),
        "the skip shape carries a machine-readable reason: {}",
        rows[0]
    );
    check("publish-notes", &out);
}

#[test]
fn publish_notes_published_output_matches_the_contract() {
    let tmp = TempDir::new().unwrap();
    let out = published_shape(tmp.path());

    let rows = parse_jsonl(&out);
    assert_eq!(
        rows[0].get("published").and_then(|v| v.as_bool()),
        Some(true),
        "expected a successful publish, got {}",
        rows[0]
    );
    check("publish-notes", &out);
}

#[test]
fn publish_notes_best_effort_error_output_matches_the_contract() {
    let tmp = TempDir::new().unwrap();
    let out = best_effort_error_shape(tmp.path());

    let rows = parse_jsonl(&out);
    assert_eq!(rows.len(), 1, "publish-notes emits exactly one object");
    assert_eq!(
        rows[0].get("published").and_then(|v| v.as_bool()),
        Some(false),
        "a tolerated failure published nothing, got {}",
        rows[0]
    );
    assert!(
        rows[0].get("error").is_some(),
        "the failure has to reach the payload, since the exit code deliberately hides it: {}",
        rows[0]
    );
    check("publish-notes", &out);
}

// A field the contract declares but no code path emits is a promise about
// nothing, and the type-only checker cannot notice one: optional fields are
// only inspected when present. `publish-notes` is the command where that
// matters most, because its shapes are untyped `json!` literals and its three
// outcomes are enumerable.
#[test]
fn every_declared_publish_notes_field_is_emitted_by_some_outcome() {
    let tmp = TempDir::new().unwrap();
    let shapes = [
        ("skipped", skip_shape(tmp.path())),
        ("published", published_shape(tmp.path())),
        ("best-effort error", best_effort_error_shape(tmp.path())),
    ];

    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (label, out) in &shapes {
        let rows = parse_jsonl(out);
        assert_eq!(rows.len(), 1, "{label}: one object per invocation");
        seen.extend(sorted_keys(&rows[0]));
    }

    let schema = schema_for("publish-notes");
    for field in schema.required.keys().chain(schema.optional.keys()) {
        assert!(
            seen.contains(field),
            "the contract declares `{field}` for publish-notes, but none of its three outcomes \
             emits it: {seen:?}"
        );
    }
}

fn init_bare_remote(path: &Path) {
    plumbing_helpers::isolate_git_config();
    let status = std::process::Command::new("git")
        .args(["init", "--bare", "-q"])
        .arg(path)
        .status()
        .expect("run git init --bare");
    assert!(status.success(), "git init --bare failed");
}

fn git_in(dir: &Path, args: &[&str]) {
    plumbing_helpers::isolate_git_config();
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}
