// Coverage for `memory archive`'s git-notes write-through carrier.
//
// A single-repo test cannot tell an append from an in-place rewrite: both
// leave that one repo's `git notes show` looking archived. Only a clone that
// genuinely had to MERGE (not fast-forward) the incoming notes distinguishes
// them, so every test here runs across two real clones of a shared origin.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin_in;

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const TRACKING_REF: &str = "refs/notes/origin/spelunk";

fn git(dir: &Path, args: &[&str]) {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git")
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&git_out(dir, args).stdout).into_owned()
}

// Fetch `refs/notes/spelunk` from `origin` into this repo's tracking ref.
// Explicit rather than relying on `spelunk init`'s configured refspec, so
// each test controls exactly when a fetch happens.
fn fetch_notes(dir: &Path) {
    git(
        dir,
        &[
            "fetch",
            "-q",
            "origin",
            &format!("refs/notes/spelunk:{TRACKING_REF}"),
        ],
    );
}

fn init_repo_with_commit(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("f.txt"), "x\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

// A `spelunk` command with an isolated HOME and no server contact.
fn bin(home: &Path, cwd: &Path) -> Command {
    let mut cmd = spelunk_bin_in(home);
    cmd.current_dir(cwd)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL");
    cmd
}

// Write an empty spelunk config (init needs `--config` but no values here).
fn empty_config(dir: &Path) -> PathBuf {
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, "").unwrap();
    cfg
}

// Run `spelunk init --no-index` in `dir`, using `dir` itself as HOME (so the
// import writes `dir/.spelunk/memory.db`). Offline, non-TTY.
fn run_init(dir: &Path) -> String {
    let cfg = empty_config(dir);
    let out = spelunk_bin_in(dir)
        .current_dir(dir)
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&cfg)
        .args(["init", "--no-index"])
        .output()
        .expect("spawn spelunk init");
    assert!(
        out.status.success(),
        "spelunk init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

// The spelunk records currently in HEAD's `refs/notes/spelunk` note.
fn spelunk_note_lines(dir: &Path) -> Vec<String> {
    let blob = git_stdout(dir, &["notes", "--ref=spelunk", "show", "HEAD"]);
    blob.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn record_field(line: &str, key: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(line).expect("record parses as JSON");
    v.get(key)
        .unwrap_or_else(|| panic!("record has no {key:?}: {line}"))
        .to_string()
        .trim_matches('"')
        .to_string()
}

// The `id` spelunk assigned locally (in `dir`'s own `memory.db`) to the entry
// titled `title`. Distinct from the `id` on a git-notes record: two clones
// mint their own rowids for the same entity independently.
fn local_id_for_title(home: &Path, dir: &Path, title: &str) -> i64 {
    let out = bin(home, dir)
        .args(["memory", "list", "--format", "jsonl", "--limit", "100"])
        .output()
        .expect("spawn spelunk memory list");
    assert!(
        out.status.success(),
        "memory list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    stdout
        .lines()
        .find_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            (v.get("title")?.as_str()? == title)
                .then(|| v.get("id")?.as_i64())
                .flatten()
        })
        .unwrap_or_else(|| panic!("no local entry titled {title:?} in:\n{stdout}"))
}

// A bare origin plus two clones ("a" and "b") that both hold the same
// single-commit history. Both get a `.spelunk/` dir so a plain `memory add`
// resolves to the SQLite-primary-plus-carrier path (not the pre-init
// fallback). Returns (origin, a, b).
fn setup_origin_with_two_clones(tmp: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let origin = tmp.join("origin.git");
    git(
        tmp,
        &[
            "init",
            "--bare",
            "-q",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );

    let a = tmp.join("a");
    std::fs::create_dir_all(&a).unwrap();
    init_repo_with_commit(&a);
    git(&a, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&a, &["push", "-q", "-u", "origin", "main"]);
    std::fs::create_dir_all(a.join(".spelunk")).unwrap();

    let b = tmp.join("b");
    git(
        tmp,
        &["clone", "-q", origin.to_str().unwrap(), b.to_str().unwrap()],
    );
    git(&b, &["config", "user.email", "b@example.com"]);
    git(&b, &["config", "user.name", "B"]);
    std::fs::create_dir_all(b.join(".spelunk")).unwrap();

    (origin, a, b)
}

/// Clone A archives entry X and pushes; clone B fetches and merges. B holds a
/// divergent local note of its own (added after adopting X but before A's
/// archive lands), so the merge genuinely unions two histories rather than
/// fast-forwarding. Without that divergence, B's working ref would just be an
/// ancestor of the incoming tracking ref, and a broken in-place rewrite of X's
/// status would look correct too — this is the trap the two-clone shape
/// exists to close.
#[test]
fn two_clone_archive_travels_and_folds_to_archived_once_despite_divergent_note() {
    let tmp = TempDir::new().unwrap();
    let home_a = TempDir::new().unwrap();
    let home_b = TempDir::new().unwrap();
    let (_origin, a, b) = setup_origin_with_two_clones(tmp.path());

    bin(home_a.path(), &a)
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "clone-a-archives-me",
            "--body",
            "b1",
        ])
        .assert()
        .success();
    let seeded = spelunk_note_lines(&a);
    assert_eq!(seeded.len(), 1, "setup: A's own add");
    let x_id = record_field(&seeded[0], "id");
    git(&a, &["push", "-q", "origin", "refs/notes/spelunk"]);

    // B adopts X onto its own working ref before diverging: without a prior
    // local commit of its own, this merge is a plain fast-forward/create.
    fetch_notes(&b);
    bin(home_b.path(), &b)
        .args(["memory", "--backend", "git-notes", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("clone-a-archives-me"));

    // B's own, unrelated entry: the divergent local note.
    bin(home_b.path(), &b)
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "clone-b-local-only",
            "--body",
            "b2",
        ])
        .assert()
        .success();

    bin(home_a.path(), &a)
        .args(["memory", "archive", &x_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("Archived memory entry"));
    git(&a, &["push", "-q", "origin", "refs/notes/spelunk"]);

    fetch_notes(&b);

    let default_list = bin(home_b.path(), &b)
        .args(["memory", "--backend", "git-notes", "list"])
        .output()
        .expect("spawn spelunk memory list");
    assert!(default_list.status.success());
    let default_stdout = String::from_utf8_lossy(&default_list.stdout);
    assert!(
        !default_stdout.contains("clone-a-archives-me"),
        "archived X must not appear in a default list, got:\n{default_stdout}"
    );
    assert!(
        default_stdout.contains("clone-b-local-only"),
        "the union must not drop B's own divergent entry, got:\n{default_stdout}"
    );

    // Exactly once: the archived state-update must have FOLDED onto X's
    // original active copy, not merely sit next to it as a second entry.
    let archived_list = bin(home_b.path(), &b)
        .args(["memory", "--backend", "git-notes", "list", "--archived"])
        .output()
        .expect("spawn spelunk memory list --archived");
    let archived_stdout = String::from_utf8_lossy(&archived_list.stdout);
    assert_eq!(
        archived_stdout.matches("clone-a-archives-me").count(),
        1,
        "X must fold to exactly one entry, got:\n{archived_stdout}"
    );
    assert!(
        archived_stdout.contains("[archived]"),
        "X's single folded copy must be marked archived, got:\n{archived_stdout}"
    );

    // Assert on the raw git ref too, not just the CLI's folded view: the
    // fold could hide either a rewrite (one line, wrongly convincing) or an
    // over-eager append (three-plus lines). The archive state-update is a new
    // line alongside X's original, never a replacement of it, so exactly two
    // raw lines must carry X's entity_id: the untouched original (still
    // "active") and the appended state-update ("archived").
    let raw_lines = spelunk_note_lines(&b);
    let x_entity_id = record_field(&seeded[0], "entity_id");
    let x_lines: Vec<&String> = raw_lines
        .iter()
        .filter(|l| record_field(l, "entity_id") == x_entity_id)
        .collect();
    assert_eq!(
        x_lines.len(),
        2,
        "X must carry exactly two raw lines after the merge (original + \
         appended state-update); a rewrite collapses to one, an unbounded \
         re-append would exceed two, got:\n{raw_lines:?}"
    );
    assert!(
        x_lines
            .iter()
            .any(|l| record_field(l, "status") == "active"),
        "the original active line must survive byte-for-byte on the raw ref, got:\n{raw_lines:?}"
    );
    assert!(
        x_lines
            .iter()
            .any(|l| record_field(l, "status") == "archived"),
        "the appended state-update line must be present on the raw ref, got:\n{raw_lines:?}"
    );
}

/// Two clones each archive the same entity independently, neither aware of
/// the other, before either fetches the other's update. The read-side fold
/// (ADR-068 A6) must converge the two archived-state-update copies to one
/// entry rather than surfacing a duplicate or a conflict.
#[test]
fn concurrent_archives_from_two_clones_fold_to_one_archived_entry() {
    let tmp = TempDir::new().unwrap();
    let home_a = TempDir::new().unwrap();
    let (_origin, a, b) = setup_origin_with_two_clones(tmp.path());

    bin(home_a.path(), &a)
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "double-archived-entry",
            "--body",
            "b1",
        ])
        .assert()
        .success();
    let seeded = spelunk_note_lines(&a);
    assert_eq!(seeded.len(), 1, "setup: A's own add");
    let a_id = record_field(&seeded[0], "id");
    git(&a, &["push", "-q", "origin", "refs/notes/spelunk"]);

    // B needs its own local (SQLite) copy of X to archive it through the
    // normal command, so it imports via a real `spelunk init` rather than the
    // manual `.spelunk` mkdir the other clones in this file use.
    fetch_notes(&b);
    let init_stdout = run_init(&b);
    assert!(
        init_stdout.contains("imported 1 entries from git notes"),
        "setup: B must import X via init, got:\n{init_stdout}"
    );
    let b_id = local_id_for_title(&b, &b, "double-archived-entry");

    // B archives its own copy — unaware A hasn't pushed an archive yet.
    bin(&b, &b)
        .args(["memory", "archive", &b_id.to_string()])
        .assert()
        .success();

    // A independently archives its own copy and pushes — unaware B already did.
    bin(home_a.path(), &a)
        .args(["memory", "archive", &a_id])
        .assert()
        .success();
    git(&a, &["push", "-q", "origin", "refs/notes/spelunk"]);

    fetch_notes(&b);

    let archived_list = bin(&b, &b)
        .args(["memory", "--backend", "git-notes", "list", "--archived"])
        .output()
        .expect("spawn spelunk memory list --archived");
    assert!(archived_list.status.success());
    let archived_stdout = String::from_utf8_lossy(&archived_list.stdout);
    assert_eq!(
        archived_stdout.matches("double-archived-entry").count(),
        1,
        "two independent archives of the same entity must fold to one entry, got:\n{archived_stdout}"
    );
    assert!(
        archived_stdout.contains("[archived]"),
        "the folded entry must be marked archived, got:\n{archived_stdout}"
    );
}

// The carrier write is best-effort (matching `memory add`/`memory
// supersede`'s contract): if `refs/notes` cannot be written, `memory
// archive` must still report success and the SQLite primary must still hold
// the archive. Only the carry is allowed to fail quietly.
#[cfg(unix)]
#[test]
fn carrier_write_failure_does_not_fail_the_sqlite_archive() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir).unwrap();
    init_repo_with_commit(&dir);
    std::fs::create_dir_all(dir.join(".spelunk")).unwrap();

    bin(home.path(), &dir)
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "carrier-fail-probe",
            "--body",
            "b",
        ])
        .assert()
        .success();
    let id = local_id_for_title(home.path(), &dir, "carrier-fail-probe");

    let refs_notes = dir.join(".git/refs/notes");
    let original = std::fs::metadata(&refs_notes).unwrap().permissions();
    let mut read_only = original.clone();
    read_only.set_mode(0o555);
    std::fs::set_permissions(&refs_notes, read_only).unwrap();

    // Probe with raw git, never the code under test: root (or a mount that
    // ignores the mode) can still write there, and there would be no failure
    // to assert against.
    let enforced = !git_out(
        &dir,
        &["notes", "--ref=spelunk", "add", "-f", "-m", "x", "HEAD"],
    )
    .status
    .success();
    if !enforced {
        std::fs::set_permissions(&refs_notes, original).unwrap();
        return;
    }

    let out = bin(home.path(), &dir)
        .args(["memory", "archive", &id.to_string()])
        .output()
        .expect("spawn spelunk memory archive");

    // Restore before asserting: a panic below would otherwise leave a
    // read-only directory behind that `TempDir` cannot clean up.
    std::fs::set_permissions(&refs_notes, original).unwrap();

    assert!(
        out.status.success(),
        "a failed git-notes carry must not fail the command, got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("git-notes carry failed"),
        "the failure must surface as a warning, got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let archived = bin(home.path(), &dir)
        .args([
            "memory",
            "list",
            "--archived",
            "--format",
            "jsonl",
            "--limit",
            "10",
        ])
        .output()
        .expect("spawn spelunk memory list --archived");
    assert!(
        String::from_utf8_lossy(&archived.stdout).contains("carrier-fail-probe"),
        "the SQLite primary must hold the archive even though the carrier failed, got:\n{}",
        String::from_utf8_lossy(&archived.stdout)
    );
}
