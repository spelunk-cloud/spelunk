// End-to-end coverage for the `search --mode semantic|hybrid` no-server gate.
//
// The bug: with no reachable server, an explicit `spelunk search --mode
// semantic` (and `--mode hybrid`) silently fell back to FTS text search and, on
// an empty match, printed "No results found." + exit 0. To an agent or a script
// that reads as "no such code exists" rather than "the feature is unavailable" —
// unlike every other inference-gated command (`memory search`, `explore`,
// `memory timeline`), which fail closed with the actionable
// "requires spelunk-server" locked-feature error.
//
// The fix routes the explicit inference modes through the same
// `require_server_client` gate the other commands use, so they exit non-zero
// with the shared message. `auto` (the default) is deliberately left to degrade
// gracefully to ast-grep with a visible notice — that is its contract and must
// not regress.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin_in;

use std::path::Path;
use tempfile::TempDir;

// A distinctive symbol the auto-mode ast-grep fallback can match, so the auto
// regression test proves a real fallback ran rather than an empty error path.
const PROBE_SYMBOL: &str = "spelunk_gate_probe_marker";

// Build a populated-but-unembedded project offline: `<proj>/.spelunk/index.db`
// exists and holds chunks (chunk_count > 0), but zero embeddings — indexing
// under `SPELUNK_NO_SERVER=1` parses source into chunks yet has no embedder, so
// nothing is embedded. This is exactly the state the semantic/hybrid gate must
// catch: a real index with no way to embed the query.
fn init_populated_project_offline(home: &Path, proj: &Path) {
    std::fs::create_dir_all(proj.join("src")).expect("create src dir");
    std::fs::write(
        proj.join("src").join("lib.rs"),
        format!(
            "pub fn {PROBE_SYMBOL}() -> i32 {{\n    42\n}}\n\n\
             pub fn helper_add(a: i32, b: i32) -> i32 {{\n    a + b\n}}\n"
        ),
    )
    .expect("write source file");

    spelunk_bin_in(home)
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(proj)
        .args(["index", "."])
        .assert()
        .success();

    assert!(
        proj.join(".spelunk").join("index.db").exists(),
        "indexing must create the project index.db"
    );
}

// Shared assertion: the explicit-mode failure carries the actionable
// locked-feature error (never the silent empty-result path).
fn assert_locked_feature_gate(stderr: &str, stdout: &str) {
    assert!(
        stderr.contains("requires spelunk-server"),
        "must emit the shared locked-feature error; got stderr: {stderr}"
    );
    assert!(
        stderr.contains("spelunk server start"),
        "must point at the actionable resume command; got stderr: {stderr}"
    );
    assert!(
        !stdout.contains("No results found."),
        "must NOT silently report an empty result set; got stdout: {stdout}"
    );
}

// `search --mode semantic` with no server: the exact bug report. Must exit
// non-zero with the locked-feature error, not "No results found." / exit 0.
#[test]
fn search_mode_semantic_no_server_gates_with_locked_feature_error() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_populated_project_offline(home.path(), proj.path());

    let assert = spelunk_bin_in(home.path())
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(proj.path())
        .args(["search", "anything", "--mode", "semantic"])
        .assert()
        .failure();

    let out = assert.get_output();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_locked_feature_gate(&stderr, &stdout);
}

// `search --mode hybrid` shares the semantic path's embedding requirement, so it
// must be gated identically. Covered independently since it is a distinct mode
// string reaching the same branch.
#[test]
fn search_mode_hybrid_no_server_gates_with_locked_feature_error() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_populated_project_offline(home.path(), proj.path());

    let assert = spelunk_bin_in(home.path())
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(proj.path())
        .args(["search", "anything", "--mode", "hybrid"])
        .assert()
        .failure();

    let out = assert.get_output();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_locked_feature_gate(&stderr, &stdout);
}

// Regression guard: `search --mode auto` (the default) with no server must be
// UNCHANGED — it announces its degradation and falls back to ast-grep, still
// exiting 0. The silent-fallback gate is reserved for the explicit modes above.
#[test]
fn search_mode_auto_no_server_still_degrades_and_exits_zero() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_populated_project_offline(home.path(), proj.path());

    let assert = spelunk_bin_in(home.path())
        .env("SPELUNK_NO_SERVER", "1")
        .current_dir(proj.path())
        .args(["search", PROBE_SYMBOL, "--mode", "auto"])
        .assert()
        .success();

    let out = assert.get_output();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !stderr.contains("requires spelunk-server"),
        "auto mode must never be gated with the locked-feature error; got stderr: {stderr}"
    );
    assert!(
        stderr.contains("ast-grep"),
        "auto mode must announce its degradation to ast-grep; got stderr: {stderr}"
    );
}
