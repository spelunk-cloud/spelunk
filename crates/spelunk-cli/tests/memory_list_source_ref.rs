// Regression coverage for the reported bug: `spelunk memory list --source-ref
// <sha>` returned ZERO results for a commit that carries `refs/notes/spelunk`
// notes, even though `memory add` had written entries anchored to that commit.
//
// A `memory add` entry records which commit it belongs to only as the git-notes
// attachment (commit -> note object). Its SQLite `source_ref` column stays NULL
// (that column is harvest provenance), so a `source_ref` column query can never
// surface it. `--source-ref <sha>` must resolve the entries anchored to that
// commit from the notes ref and return them.

mod plumbing_helpers;
use plumbing_helpers::{init_git_repo, spelunk_bin_in};

use assert_cmd::Command;
use std::path::Path;
use tempfile::TempDir;

// A `spelunk` command with an isolated HOME and no server contact, run in `cwd`.
fn bin(home: &Path, cwd: &Path) -> Command {
    let mut cmd = spelunk_bin_in(home);
    cmd.current_dir(cwd)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL");
    cmd
}

fn git_out(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git")
}

fn head_sha(dir: &Path) -> String {
    let out = git_out(dir, &["rev-parse", "HEAD"]);
    assert!(out.status.success(), "git rev-parse HEAD failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// Make an empty commit so a second entry anchors to a different commit than the
// first, letting the test prove `--source-ref` filters by commit.
fn empty_commit(dir: &Path, msg: &str) {
    let out = git_out(dir, &["commit", "--allow-empty", "-q", "-m", msg]);
    assert!(out.status.success(), "git commit --allow-empty failed");
}

fn add_note(home: &Path, repo: &Path, db: &Path, title: &str, body: &str) {
    bin(home, repo)
        .arg("memory")
        .arg("--db")
        .arg(db)
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg(title)
        .arg("--body")
        .arg(body)
        .assert()
        .success();
}

fn list_source_ref(home: &Path, repo: &Path, db: &Path, sha: &str) -> String {
    let out = bin(home, repo)
        .arg("memory")
        .arg("--db")
        .arg(db)
        .arg("list")
        .arg("--source-ref")
        .arg(sha)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8_lossy(&out).into_owned()
}

// The core repro: an entry whose git note is anchored to a commit must be
// returned by `memory list --source-ref <that commit>`. Pre-fix this printed
// "No memory entries found." because the SQLite `source_ref` column is NULL.
#[test]
fn source_ref_finds_note_anchored_entry() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);
    let db = repo.join("memory.db");

    add_note(home.path(), &repo, &db, "anchored-entry", "b1");
    let sha = head_sha(&repo);

    let out = list_source_ref(home.path(), &repo, &db, &sha);
    assert!(
        out.contains("anchored-entry"),
        "entry anchored to {sha} must be returned by --source-ref {sha}, got:\n{out}"
    );
}

// A short sha prefix must match too (docs promise "exact or prefix").
#[test]
fn source_ref_matches_a_prefix() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);
    let db = repo.join("memory.db");

    add_note(home.path(), &repo, &db, "prefix-entry", "b1");
    let sha = head_sha(&repo);
    let prefix = &sha[..8];

    let out = list_source_ref(home.path(), &repo, &db, prefix);
    assert!(
        out.contains("prefix-entry"),
        "entry must be found by the 8-char prefix {prefix}, got:\n{out}"
    );
}

// No false positives: a commit that carries no entries returns nothing, and an
// entry anchored to one commit is not returned for a different commit.
#[test]
fn source_ref_does_not_match_the_wrong_commit() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);
    let db = repo.join("memory.db");

    // Entry one anchors to commit A.
    add_note(home.path(), &repo, &db, "entry-on-A", "ba");
    let sha_a = head_sha(&repo);

    // Move HEAD, then entry two anchors to commit B.
    empty_commit(&repo, "second");
    let sha_b = head_sha(&repo);
    add_note(home.path(), &repo, &db, "entry-on-B", "bb");

    assert_ne!(
        sha_a, sha_b,
        "the two entries must anchor to distinct commits"
    );

    let on_a = list_source_ref(home.path(), &repo, &db, &sha_a);
    assert!(
        on_a.contains("entry-on-A"),
        "A's entry must show for A:\n{on_a}"
    );
    assert!(
        !on_a.contains("entry-on-B"),
        "B's entry must NOT show for A (false positive):\n{on_a}"
    );

    let on_b = list_source_ref(home.path(), &repo, &db, &sha_b);
    assert!(
        on_b.contains("entry-on-B"),
        "B's entry must show for B:\n{on_b}"
    );
    assert!(
        !on_b.contains("entry-on-A"),
        "A's entry must NOT show for B (false positive):\n{on_b}"
    );

    // A commit sha that carries no note at all returns nothing.
    let unrelated = "0000000000000000000000000000000000000000";
    let none = list_source_ref(home.path(), &repo, &db, unrelated);
    assert!(
        none.contains("No memory entries found"),
        "a commit with no entries must return none, got:\n{none}"
    );
}
