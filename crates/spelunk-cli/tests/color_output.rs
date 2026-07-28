//! Regression coverage for the "ANSI color leaks onto piped/non-tty stdout,
//! and NO_COLOR is ignored" bug.
//!
//! `spelunk memory list` (default text format) is the lightweight target here
//! (no index or server needed, see `memory_list_format.rs`), but the fix
//! lives in a shared helper so this doubles as coverage for every text-mode
//! command that prints `\x1b[...m` escapes.

mod plumbing_helpers;
use plumbing_helpers::{spelunk_bin, write_config};

use assert_cmd::Command;
use tempfile::TempDir;

/// Create a temp project with a single memory note and return
/// `(TempDir, mem_path, config_path)`. The `TempDir` must be kept alive for
/// the duration of the test.
fn project_with_memory_note() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let mem_path = db_path.with_file_name("memory.db");

    let config_path = write_config(tmp.path(), &db_path, "http://127.0.0.1:1");

    spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_path)
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("color output test note")
        .arg("--body")
        .arg("body content here")
        .assert()
        .success();

    (tmp, mem_path, config_path)
}

fn memory_list_cmd(mem_path: &std::path::Path, config_path: &std::path::Path) -> Command {
    let mut cmd = spelunk_bin();
    cmd.arg("--config")
        .arg(config_path)
        .arg("memory")
        .arg("--db")
        .arg(mem_path)
        .arg("list");
    cmd
}

/// `assert_cmd::Command` always captures stdout through a pipe, so the child
/// process's stdout is never a tty. That's exactly the "piped" case in the
/// bug report: the raw `\x1b` (0x1b) control byte must never appear in
/// output that isn't going to a terminal.
fn assert_no_ansi(stdout: &[u8]) {
    assert!(
        !stdout.contains(&0x1b),
        "expected no ANSI escape bytes in non-tty stdout, got: {:?}",
        String::from_utf8_lossy(stdout)
    );
}

fn assert_has_ansi(stdout: &[u8]) {
    assert!(
        stdout.contains(&0x1b),
        "expected ANSI escape bytes (forced via --color=always), got: {:?}",
        String::from_utf8_lossy(stdout)
    );
}

// ── (a) non-tty stdout defaults to no color ─────────────────────────────────

#[test]
fn memory_list_default_has_no_ansi_on_non_tty_stdout() {
    let (_tmp, mem_path, config_path) = project_with_memory_note();
    let out = memory_list_cmd(&mem_path, &config_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_no_ansi(&out);
}

// ── (b) NO_COLOR forces color off regardless of tty state ──────────────────

#[test]
fn no_color_env_suppresses_color() {
    let (_tmp, mem_path, config_path) = project_with_memory_note();
    let out = memory_list_cmd(&mem_path, &config_path)
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_no_ansi(&out);
}

// ── (c) --color=always overrides both the non-tty default and NO_COLOR ─────

#[test]
fn color_always_flag_overrides_non_tty_default() {
    let (_tmp, mem_path, config_path) = project_with_memory_note();
    let mut cmd = spelunk_bin();
    let out = cmd
        .arg("--color")
        .arg("always")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_path)
        .arg("list")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_has_ansi(&out);
}

#[test]
fn color_always_flag_overrides_no_color_env() {
    let (_tmp, mem_path, config_path) = project_with_memory_note();
    let mut cmd = spelunk_bin();
    let out = cmd
        .arg("--color")
        .arg("always")
        .env("NO_COLOR", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_path)
        .arg("list")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_has_ansi(&out);
}

// ── --color=never is an explicit, unconditional off-switch ─────────────────

#[test]
fn color_never_flag_suppresses_color() {
    let (_tmp, mem_path, config_path) = project_with_memory_note();
    let mut cmd = spelunk_bin();
    let out = cmd
        .arg("--color")
        .arg("never")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_path)
        .arg("list")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_no_ansi(&out);
}

// ── spot-checks on other converted call sites ───────────────────────────────
//
// `memory list` above exercises `memory/mod.rs::print_note_summary` through
// the shared `cprintln!` macro. The macro itself is one code path, but each
// command still has to actually call through it instead of a leftover raw
// `println!` with a hand-written `\x1b[...m` escape. These spot-check two of
// the other converted sites named in the bug report (`graph`, `context`) so a
// regression that reverts one call site back to `println!` fails here even
// if `memory list` still passes.

/// `spelunk graph <symbol> --live` needs no index or config: it runs an
/// in-process ast-grep scan over the given directory (see
/// `crates/spelunk-cli/src/cli/cmd/graph.rs::graph_live`). Its header line
/// (`\x1b[1m...\x1b[0m`) and the `calls`/location fields go through
/// `cprintln!` independently of `memory list`'s call site.
fn graph_live_project() -> TempDir {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("a.rs"),
        "fn helper_fn() {}\nfn caller() { helper_fn(); }\n",
    )
    .unwrap();
    tmp
}

#[test]
fn graph_live_default_has_no_ansi_on_non_tty_stdout() {
    let tmp = graph_live_project();
    let out = spelunk_bin()
        .current_dir(tmp.path())
        .arg("graph")
        .arg("helper_fn")
        .arg("--live")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_no_ansi(&out);
}

#[test]
fn graph_live_color_always_has_ansi() {
    let tmp = graph_live_project();
    let out = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--color")
        .arg("always")
        .arg("graph")
        .arg("helper_fn")
        .arg("--live")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_has_ansi(&out);
}

/// `spelunk context`'s section header (`print_section_header` in
/// `crates/spelunk-cli/src/cli/cmd/context.rs`) emits a multi-parameter SGR
/// code (`\x1b[1;34m`), the exact form the original bug report called out as
/// a risk for a naive strip regex. `--no-conventions --local-only` keeps this
/// to the plain memory-list path (no index DB, no cross-project lookup, no
/// embedding call needed).
fn context_project_with_decision() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let mem_path = db_path.with_file_name("memory.db");
    let config_path = write_config(tmp.path(), &db_path, "http://127.0.0.1:1");

    spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_path)
        .arg("add")
        .arg("--kind")
        .arg("decision")
        .arg("--title")
        .arg("color output test decision")
        .arg("--body")
        .arg("why we made this call")
        .assert()
        .success();

    (tmp, mem_path, config_path)
}

fn context_cmd(mem_path: &std::path::Path, config_path: &std::path::Path) -> Command {
    let mut cmd = spelunk_bin();
    cmd.arg("--config")
        .arg(config_path)
        .arg("context")
        .arg("--db")
        .arg(mem_path)
        .arg("--no-conventions")
        .arg("--local-only");
    cmd
}

#[test]
fn context_section_header_has_no_ansi_on_non_tty_stdout() {
    let (_tmp, mem_path, config_path) = context_project_with_decision();
    let out = context_cmd(&mem_path, &config_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_no_ansi(&out);
}

#[test]
fn context_section_header_no_color_env_suppresses_color() {
    let (_tmp, mem_path, config_path) = context_project_with_decision();
    let out = context_cmd(&mem_path, &config_path)
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_no_ansi(&out);
}

#[test]
fn context_section_header_color_always_has_ansi() {
    let (_tmp, mem_path, config_path) = context_project_with_decision();
    let mut cmd = context_cmd(&mem_path, &config_path);
    let out = cmd
        .arg("--color")
        .arg("always")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_has_ansi(&out);
}
