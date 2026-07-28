//! Regression coverage: `memory add --db` must scope the git-notes
//! write-through carrier to the `--db` target's own project, not to
//! whatever git repo the process happens to be run from.
//!
//! Before the fix, `append_to_git_notes`/`append_state_update`/
//! `GitNotesBackend::new()` all resolved the repo from the process CWD
//! regardless of `--db`, so `cargo test`-style fixture seeding (which points
//! `--db` at a tmpdir but inherits the developer's repo as CWD) silently
//! appended fixture entries to the developer's real `refs/notes/spelunk`.

mod plumbing_helpers;
use plumbing_helpers::{init_git_repo, spelunk_bin_in};

use assert_cmd::Command;
use std::path::Path;
use tempfile::TempDir;

fn git_out(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git")
}

/// The spelunk records currently on `HEAD`'s `refs/notes/spelunk` note in
/// `dir`, or `None` when that repo holds no such note at all.
fn spelunk_note_lines(dir: &Path) -> Option<Vec<String>> {
    let out = git_out(dir, &["notes", "--ref=spelunk", "show", "HEAD"]);
    if !out.status.success() {
        return None;
    }
    let blob = String::from_utf8_lossy(&out.stdout);
    Some(
        blob.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

fn record_field(line: &str, key: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(line).expect("record parses as JSON");
    v.get(key)
        .unwrap_or_else(|| panic!("record has no {key:?}: {line}"))
        .to_string()
        .trim_matches('"')
        .to_string()
}

/// A `spelunk` command with an isolated HOME and no server contact, run in `cwd`.
fn bin(home: &Path, cwd: &Path) -> Command {
    let mut cmd = spelunk_bin_in(home);
    cmd.current_dir(cwd)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL");
    cmd
}

/// The regression guard itself: a `--db` target with no git repo of its own
/// must never fall back to writing the carrier into the CWD repo. This is
/// exactly the shape `cargo test` fixture seeding relies on (DB in a bare
/// tmpdir, CWD the developer's checkout).
#[test]
fn db_target_outside_any_repo_never_writes_cwd_repos_notes() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    let host_repo = tmp.path().join("host_repo");
    std::fs::create_dir_all(&host_repo).unwrap();
    init_git_repo(&host_repo);

    let db_target = tmp.path().join("db_target");
    std::fs::create_dir_all(&db_target).unwrap();

    bin(home.path(), &host_repo)
        .arg("memory")
        .arg("--db")
        .arg(db_target.join("memory.db"))
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("should-not-pollute-host")
        .arg("--body")
        .arg("b")
        .assert()
        .success();

    assert!(
        spelunk_note_lines(&host_repo).is_none(),
        "the host repo's refs/notes/spelunk must stay untouched when --db \
         points outside it"
    );
}

/// Positive control: when `--db`'s parent directory IS its own git repo,
/// separate from CWD's, the carrier writes there and not into CWD's repo.
#[test]
fn db_target_inside_its_own_repo_writes_there_not_cwd_repo() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    let host_repo = tmp.path().join("host_repo");
    std::fs::create_dir_all(&host_repo).unwrap();
    init_git_repo(&host_repo);

    let project_repo = tmp.path().join("project_repo");
    std::fs::create_dir_all(&project_repo).unwrap();
    init_git_repo(&project_repo);

    bin(home.path(), &host_repo)
        .arg("memory")
        .arg("--db")
        .arg(project_repo.join("memory.db"))
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("goes-to-project-repo")
        .arg("--body")
        .arg("b")
        .assert()
        .success();

    assert!(
        spelunk_note_lines(&host_repo).is_none(),
        "the CWD repo must not receive the note"
    );
    let project_lines = spelunk_note_lines(&project_repo)
        .expect("the --db target's own repo must receive the note");
    assert_eq!(project_lines.len(), 1);
    assert_eq!(
        record_field(&project_lines[0], "title"),
        "goes-to-project-repo"
    );
}

/// The `--supersedes` state-update carry for the OLD entity (a separate call
/// site from the main record write) must also be scoped to the `--db` target,
/// not the CWD repo.
#[test]
fn supersedes_state_update_also_scoped_to_db_target_repo() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    let host_repo = tmp.path().join("host_repo");
    std::fs::create_dir_all(&host_repo).unwrap();
    init_git_repo(&host_repo);

    let project_repo = tmp.path().join("project_repo");
    std::fs::create_dir_all(&project_repo).unwrap();
    init_git_repo(&project_repo);
    let db_path = project_repo.join("memory.db");

    // Both adds run from host_repo's CWD: only --db should decide where the
    // carrier (new record AND old-entity state-update) lands.
    bin(home.path(), &host_repo)
        .arg("memory")
        .arg("--db")
        .arg(&db_path)
        .arg("add")
        .arg("--kind")
        .arg("decision")
        .arg("--title")
        .arg("old-entry")
        .arg("--body")
        .arg("b1")
        .assert()
        .success();
    let old_lines = spelunk_note_lines(&project_repo).expect("first add wrote a note");
    assert_eq!(old_lines.len(), 1);
    let old_id = record_field(&old_lines[0], "id");
    let old_entity_id = record_field(&old_lines[0], "entity_id");

    bin(home.path(), &host_repo)
        .arg("memory")
        .arg("--db")
        .arg(&db_path)
        .arg("add")
        .arg("--kind")
        .arg("decision")
        .arg("--title")
        .arg("new-entry")
        .arg("--body")
        .arg("b2")
        .arg("--supersedes")
        .arg(&old_id)
        .assert()
        .success();

    assert!(
        spelunk_note_lines(&host_repo).is_none(),
        "the CWD repo must not receive the new record or the supersede \
         state-update"
    );
    let project_lines = spelunk_note_lines(&project_repo)
        .expect("the --db target's own repo must hold every record");
    assert_eq!(
        project_lines.len(),
        3,
        "expected OLD's original active record, NEW's record, and OLD's \
         archived state-update, got:\n{project_lines:?}"
    );
    assert!(
        project_lines
            .iter()
            .any(|l| record_field(l, "entity_id") == old_entity_id
                && record_field(l, "status") == "archived"),
        "OLD's state-update (status=archived, same entity_id) must be present, got:\n{project_lines:?}"
    );
}

/// No local `.spelunk` project and no `--db`: `add` rides the git-notes
/// carrier straight into CWD's own repo (ADR-068 D3). This path's project
/// root is legitimately CWD-derived, so this confirms the redirect fix left
/// it alone rather than guarding a regression.
#[test]
fn pre_init_add_with_no_local_project_uses_cwd_repo() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    bin(home.path(), &repo)
        .arg("memory")
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("pre-init-entry")
        .arg("--body")
        .arg("b")
        .assert()
        .success();

    let lines = spelunk_note_lines(&repo).expect("pre-init add must write to CWD's repo");
    assert_eq!(lines.len(), 1);
    assert_eq!(record_field(&lines[0], "title"), "pre-init-entry");
}

/// `--supersedes` on the pre-init (no local project, no `--db`) path exercises
/// the third call site: the E4 pre-flight read of OLD via
/// `GitNotesBackend::with_root(project_root)` at `add.rs`'s `pre_init_notes`
/// branch. None of the other tests in this file combine `pre_init_notes` with
/// `--supersedes`, so without this test that read path (and the state-update
/// carry that follows it) runs unexercised by anything in this suite.
#[test]
fn pre_init_supersedes_reads_and_writes_cwd_repo() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    bin(home.path(), &repo)
        .arg("memory")
        .arg("add")
        .arg("--kind")
        .arg("decision")
        .arg("--title")
        .arg("pre-init-old")
        .arg("--body")
        .arg("b1")
        .assert()
        .success();
    let old_lines = spelunk_note_lines(&repo).expect("first pre-init add wrote a note");
    assert_eq!(old_lines.len(), 1);
    let old_id = record_field(&old_lines[0], "id");
    let old_entity_id = record_field(&old_lines[0], "entity_id");

    bin(home.path(), &repo)
        .arg("memory")
        .arg("add")
        .arg("--kind")
        .arg("decision")
        .arg("--title")
        .arg("pre-init-new")
        .arg("--body")
        .arg("b2")
        .arg("--supersedes")
        .arg(&old_id)
        .assert()
        .success();

    let lines = spelunk_note_lines(&repo).expect("pre-init add must write to CWD's repo");
    assert_eq!(
        lines.len(),
        3,
        "expected OLD's original record, NEW's record, and OLD's archived \
         state-update, got:\n{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| record_field(l, "entity_id") == old_entity_id
                && record_field(l, "status") == "archived"),
        "OLD's state-update (status=archived, same entity_id), written via the \
         pre-init GitNotesBackend::with_root preflight read, must be present, \
         got:\n{lines:?}"
    );
}

/// Adversarial nesting case: the `--db` target's own repo is not a sibling of
/// the CWD repo but lives *inside* it (a second `.git` nested a few
/// directories below the CWD repo's root, as e.g. a vendored checkout would
/// look, without going through an actual submodule). `git -C <dir>` discovery
/// walks upward from `<dir>` and must stop at the nearest `.git` (the nested
/// repo), not continue past it to the outer CWD repo. This is a stricter
/// version of `db_target_inside_its_own_repo_writes_there_not_cwd_repo`: that
/// test's two repos are unrelated siblings, so it can't tell "found the right
/// repo" apart from "found *some* repo containing a `.git` in its ancestry
/// that happens not to be CWD's". Nesting the target repo inside the CWD repo
/// closes that gap.
#[test]
fn db_target_nested_inside_cwd_repo_writes_there_not_cwd_repo() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    let host_repo = tmp.path().join("host_repo");
    std::fs::create_dir_all(&host_repo).unwrap();
    init_git_repo(&host_repo);

    let nested_repo = host_repo.join("vendor").join("nested_repo");
    std::fs::create_dir_all(&nested_repo).unwrap();
    init_git_repo(&nested_repo);

    bin(home.path(), &host_repo)
        .arg("memory")
        .arg("--db")
        .arg(nested_repo.join("memory.db"))
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("goes-to-nested-repo")
        .arg("--body")
        .arg("b")
        .assert()
        .success();

    assert!(
        spelunk_note_lines(&host_repo).is_none(),
        "the outer host repo must not receive the note even though the --db \
         target's repo is nested inside it"
    );
    let nested_lines = spelunk_note_lines(&nested_repo)
        .expect("the --db target's own nested repo must receive the note");
    assert_eq!(nested_lines.len(), 1);
    assert_eq!(
        record_field(&nested_lines[0], "title"),
        "goes-to-nested-repo"
    );
}
