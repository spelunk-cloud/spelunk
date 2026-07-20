//! ADR-068 D3: git-notes memory carrier for `memory add`/`list` before `init`.
//!
//! Store priority for `memory add`/`list` (ADR-004, unchanged) resolves in order:
//!   1. `--backend git-notes` → git notes as the *primary* store
//!   2. explicit team `server_url` (CloudFirst → remote)
//!   3. a resolvable local `.spelunk/` DB (sqlite)
//!   4. no DB but inside a git repo → the universal git-notes write-through is
//!      the sole writer (ref `refs/notes/spelunk`); there is no SQLite primary
//!   5. neither → fail with the dual-escape-hatch message.
//!
//! Pre-`init` (case 4) rides the same `append_to_git_notes` write-through that
//! already runs post-`init`, so every note carries an identical record shape.
//! These tests cover cases 1, 3, 4, and 5, the single-record invariant, record
//! shape parity between the pre-init and post-init write-through forms, and the
//! secret-scan gate on the git-notes path. The complementary refuse-only tests
//! (case 5 from a bare temp dir) live in `fail_closed_no_project.rs`.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin_in;

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// ADR-068 D3 dual-escape-hatch error (case 5): neither a project DB nor a
/// usable git repo. Kept in sync with `fail_closed_no_project.rs`.
const NO_PROJECT_NO_REPO_ERR: &str = "no spelunk project here, and not inside a git repo. Run 'spelunk init' first, \
     or run inside a git repository.";

/// ADR-067 single-hatch error: no local `.spelunk/` project. This is what every
/// memory subcommand *except* the ADR-068 D3 add/list carrier still raises,
/// even inside a git repo (the carrier never widens to them). Distinct from
/// `NO_PROJECT_NO_REPO_ERR`: the dual-hatch text splices ", and not inside a git
/// repo" between "here" and ". Run", so this substring matches only the
/// single-hatch message.
const NO_PROJECT_ERR: &str = "no spelunk project here. Run 'spelunk init' first";

/// A `spelunk` command with an isolated HOME (so the "global" store lives under
/// `<home>/.config/spelunk`) and no server contact, run in `cwd`.
fn bin(home: &Path, cwd: &Path) -> Command {
    let mut cmd = spelunk_bin_in(home);
    cmd.current_dir(cwd)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL");
    cmd
}

/// The global memory store path under the isolated HOME. The git-notes fallback
/// must never create it.
fn global_memory_db(home: &Path) -> std::path::PathBuf {
    home.join(".config").join("spelunk").join("memory.db")
}

/// Run `git args` in `dir`, asserting success. Isolated identity so it works on a
/// machine with no global git config.
fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// stdout of `git args` in `dir` (whatever the exit status). Used for read-only
/// notes inspection where a missing ref is a legitimate empty result.
fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A git repo with one commit and no `.spelunk/`. `user.*` is set in the LOCAL
/// repo config so the `git notes add` that the spawned `spelunk` runs (which
/// does NOT inherit the test's `GIT_*` identity env) has a committer identity.
fn init_git_repo_with_commit(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    // Local (not env) identity: the spelunk child reads this from `.git/config`.
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    std::fs::write(dir.join("f.txt"), "x\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

/// The spelunk records currently in HEAD's `refs/notes/spelunk` note (one JSON
/// object per line). Empty when the ref/note does not exist.
fn spelunk_note_lines(dir: &Path) -> Vec<String> {
    let blob = git_stdout(dir, &["notes", "--ref=spelunk", "show", "HEAD"]);
    blob.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

// ── case 4: happy-path round-trip via git notes ────────────────────────────────

#[test]
fn memory_add_list_round_trips_via_git_notes_fallback() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    let title = "fallback-roundtrip-abc123";

    // add: no `.spelunk/`, but inside a git repo → falls back to git-notes.
    bin(home.path(), repo.path())
        .args([
            "memory", "add", "--kind", "note", "--title", title, "--body", "b",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    // The entry landed in `refs/notes/spelunk` on HEAD.
    let note_blob = git_stdout(repo.path(), &["notes", "--ref=spelunk", "show", "HEAD"]);
    assert!(
        note_blob.contains(title),
        "the note on HEAD must contain the added entry's title; got: {note_blob:?}"
    );
    // `git notes list` shows exactly one noted commit (HEAD).
    let list = git_stdout(repo.path(), &["notes", "--ref=spelunk", "list"]);
    assert_eq!(
        list.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "exactly one commit (HEAD) should carry a spelunk note; got: {list:?}"
    );

    // list: reads the entry back through the same git-notes fallback.
    bin(home.path(), repo.path())
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains(title));

    // The fallback must not create a local `.spelunk/` nor touch the global store.
    assert!(
        !repo.path().join(".spelunk").exists(),
        "git-notes fallback must not create a local .spelunk/ project"
    );
    assert!(
        !global_memory_db(home.path()).exists(),
        "git-notes fallback must not create the machine-global memory store"
    );
}

// ── single record per single `add`: the carrier is the sole writer ─────────────

#[test]
fn single_add_writes_exactly_one_note_record() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    // Pre-init there is no SQLite primary: the write-through carrier is the sole
    // writer, so a single `add` must leave exactly one JSON record in the note,
    // not two (no separate primary append + write-through).
    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "one-and-only",
            "--body",
            "b",
        ])
        .assert()
        .success();

    let lines = spelunk_note_lines(repo.path());
    assert_eq!(
        lines.len(),
        1,
        "a single `memory add` must write exactly one record line to the note; got: {lines:?}"
    );
    assert!(
        lines[0].contains("\"schema_version\":1") && lines[0].contains("one-and-only"),
        "the single record must be the well-formed entry we added; got: {:?}",
        lines[0]
    );
}

// ── record-shape parity: pre-init carrier == post-init write-through form ──────

/// Top-level object keys of a one-line JSON object, sorted. A minimal
/// depth-aware scan (integration tests can't reach the crate's `serde_json`):
/// only quoted strings at brace-depth 1 that are immediately followed by `:`
/// count, so nested-array elements and string *values* are ignored.
fn json_top_level_keys(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut keys = Vec::new();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut escaped = false;
    let mut cur = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_str {
            if escaped {
                cur.push(c);
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                    j += 1;
                }
                if depth == 1 && j < bytes.len() && bytes[j] as char == ':' {
                    keys.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
            } else {
                cur.push(c);
            }
        } else {
            match c {
                '"' => {
                    in_str = true;
                    cur.clear();
                }
                '{' | '[' => depth += 1,
                '}' | ']' => depth -= 1,
                _ => {}
            }
        }
        i += 1;
    }
    keys.sort();
    keys
}

/// The single note record a pre-init `add` writes (via the carrier) must have
/// the exact same field set as the record a post-init `add` writes (via the
/// SQLite-primary write-through). Both flow through one `append_to_git_notes`
/// path, so any divergence in the pre-init record shape is a regression.
#[test]
fn pre_init_and_post_init_records_have_identical_shape() {
    let home = TempDir::new().unwrap();

    // Pre-init: no `.spelunk/`, inside a git repo → carrier writes the record.
    let pre = TempDir::new().unwrap();
    init_git_repo_with_commit(pre.path());
    bin(home.path(), pre.path())
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "shape-pre",
            "--body",
            "b",
        ])
        .assert()
        .success();
    let pre_lines = spelunk_note_lines(pre.path());
    assert_eq!(pre_lines.len(), 1, "pre-init add writes one record");

    // Post-init: a local `.spelunk/` makes SQLite the primary; the same
    // write-through then appends the note. Creating the dir is enough for
    // `require_project_db` to resolve the project (matches the precedence test).
    let post = TempDir::new().unwrap();
    init_git_repo_with_commit(post.path());
    std::fs::create_dir_all(post.path().join(".spelunk")).unwrap();
    bin(home.path(), post.path())
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "shape-post",
            "--body",
            "b",
        ])
        .assert()
        .success();
    let post_lines = spelunk_note_lines(post.path());
    assert_eq!(
        post_lines.len(),
        1,
        "post-init write-through writes one record"
    );

    let pre_keys = json_top_level_keys(&pre_lines[0]);
    let post_keys = json_top_level_keys(&post_lines[0]);

    // Guard against a degenerate match: an empty (or shrunk) key set on both
    // sides would satisfy a bare set-equality check. Assert both records actually
    // carry the canonical NoteRecord field set a `note` add with no
    // tags/files/dates serializes. (The Option-typed fields source_ref,
    // valid_at, invalid_at, superseded_by, and remote_id are omitted by serde
    // when None, so the always-present core below is the shape under test.)
    for expected in [
        "body",
        "created_at",
        "entity_id",
        "id",
        "kind",
        "linked_files",
        "schema_version",
        "status",
        "tags",
        "title",
    ] {
        assert!(
            pre_keys.iter().any(|k| k == expected),
            "pre-init record is missing the canonical key {expected:?}; got {pre_keys:?}"
        );
        assert!(
            post_keys.iter().any(|k| k == expected),
            "post-init record is missing the canonical key {expected:?}; got {post_keys:?}"
        );
    }

    assert_eq!(
        pre_keys, post_keys,
        "pre-init carrier and post-init write-through records must share one shape\n\
         pre:  {}\npost: {}",
        pre_lines[0], post_lines[0]
    );
}

// ── identity survives a rowid renumber ────────────────────────────────────────

/// The value of `key` in a JSON-Lines record.
fn record_field(line: &str, key: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(line).expect("record parses as JSON");
    v.get(key)
        .unwrap_or_else(|| panic!("record has no {key:?}: {line}"))
        .to_string()
        .trim_matches('"')
        .to_string()
}

/// Re-`init` recreates memory.db, resetting its autoincrement rowid to 1. Two
/// different entries then land in one notes ref stamped `"id":1` — the observed
/// collision. Their `entity_id`s must still tell them apart.
#[test]
fn reinit_between_adds_yields_distinct_entity_ids() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    let add = |title: &str, body: &str| {
        bin(home.path(), repo.path())
            .args([
                "memory", "add", "--kind", "decision", "--title", title, "--body", body,
            ])
            .assert()
            .success();
    };

    // A local `.spelunk/` makes SQLite the primary, so the rowid is a real
    // autoincrement rather than the pre-init timestamp id.
    std::fs::create_dir_all(repo.path().join(".spelunk")).unwrap();
    add("first decision", "body one");

    // Re-init: the store is recreated, so the rowid counter restarts.
    std::fs::remove_dir_all(repo.path().join(".spelunk")).unwrap();
    std::fs::create_dir_all(repo.path().join(".spelunk")).unwrap();
    add("second decision", "body two");

    let lines = spelunk_note_lines(repo.path());
    assert_eq!(lines.len(), 2, "both adds carried into the notes ref");

    // The collision is real, not hypothetical: assert it before asserting the fix.
    assert_eq!(
        record_field(&lines[0], "id"),
        record_field(&lines[1], "id"),
        "re-init must reset the rowid — otherwise this test proves nothing"
    );

    let first = record_field(&lines[0], "entity_id");
    let second = record_field(&lines[1], "entity_id");
    assert_ne!(
        first, second,
        "two different decisions must have distinct entity_ids despite the rowid collision"
    );
    assert_eq!(first.len(), 64, "entity_id is hex sha256: {first}");
    assert_eq!(second.len(), 64, "entity_id is hex sha256: {second}");
}

/// Same `{kind, title, body}` recorded in two unrelated repos, on stores whose
/// rowids and timestamps differ, must produce a byte-identical `entity_id`.
#[test]
fn entity_id_is_stable_across_stores() {
    let home = TempDir::new().unwrap();

    let entity_id_for = |title: &str, seed_extra: bool| -> String {
        let repo = TempDir::new().unwrap();
        init_git_repo_with_commit(repo.path());
        std::fs::create_dir_all(repo.path().join(".spelunk")).unwrap();
        // Push the second store's rowid counter along so the two entries under
        // test cannot share a rowid.
        if seed_extra {
            for i in 0..3 {
                bin(home.path(), repo.path())
                    .args([
                        "memory",
                        "add",
                        "--kind",
                        "note",
                        "--title",
                        &format!("filler {i}"),
                        "--body",
                        "filler",
                    ])
                    .assert()
                    .success();
            }
        }
        bin(home.path(), repo.path())
            .args([
                "memory",
                "add",
                "--kind",
                "decision",
                "--title",
                title,
                "--body",
                "shared body",
            ])
            .assert()
            .success();
        let lines = spelunk_note_lines(repo.path());
        let last = lines.last().expect("at least one record");
        record_field(last, "entity_id")
    };

    assert_eq!(
        entity_id_for("portable", false),
        entity_id_for("portable", true),
        "entity_id must not depend on the store's rowid or write time"
    );
}

// ── case 5: refuse when not inside a git repo (empty / no-HEAD repo) ────────────

#[test]
fn memory_add_refuses_in_git_repo_without_any_commit() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    // `git init` but no commit → HEAD is unresolvable, so the fallback cannot
    // attach a note. This is case 5, not case 4.
    git(repo.path(), &["init", "-q", "-b", "main"]);

    bin(home.path(), repo.path())
        .args([
            "memory", "add", "--kind", "note", "--title", "t", "--body", "b",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_NO_REPO_ERR));

    assert!(!global_memory_db(home.path()).exists());
    assert!(
        spelunk_note_lines(repo.path()).is_empty(),
        "a refused add in an empty repo must not write any spelunk note"
    );
}

#[test]
fn memory_list_refuses_in_git_repo_without_any_commit() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    git(repo.path(), &["init", "-q", "-b", "main"]);

    bin(home.path(), repo.path())
        .args(["memory", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_NO_REPO_ERR));

    assert!(!global_memory_db(home.path()).exists());
}

// ── precedence #3 > #4: a local `.spelunk/` wins over the git-notes fallback ────

#[test]
fn local_dot_spelunk_takes_precedence_over_git_notes_fallback() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());
    // Both a git repo AND a local project: sqlite must win (fallback NOT taken).
    std::fs::create_dir_all(repo.path().join(".spelunk")).unwrap();

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "sqlite-wins",
            "--body",
            "b",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    // The entry went to the local sqlite store, proving branch 3 beat branch 4.
    assert!(
        repo.path().join(".spelunk").join("memory.db").exists(),
        "with a local .spelunk/, add must write sqlite, not fall back to git-notes"
    );

    // list resolves the same sqlite store and reads the entry back.
    bin(home.path(), repo.path())
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("sqlite-wins"));

    assert!(!global_memory_db(home.path()).exists());
}

// ── precedence #1: explicit `--backend git-notes` pre-init works ───────────────

#[test]
fn explicit_backend_git_notes_works_pre_init_in_git_repo() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    let title = "explicit-git-notes-xyz";
    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--backend",
            "git-notes",
            "--kind",
            "note",
            "--title",
            title,
            "--body",
            "b",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    let note_blob = git_stdout(repo.path(), &["notes", "--ref=spelunk", "show", "HEAD"]);
    assert!(
        note_blob.contains(title),
        "explicit git-notes add must write the note; got: {note_blob:?}"
    );

    // Double-write guard: with `--backend git-notes` git notes is the *primary*
    // store, so the universal write-through is suppressed. A single `add` must
    // therefore leave exactly one record (not a primary write plus a redundant
    // write-through): the other single-write path alongside the pre-init carrier.
    let lines = spelunk_note_lines(repo.path());
    assert_eq!(
        lines.len(),
        1,
        "explicit --backend git-notes must write exactly one record \
         (write-through suppressed); got: {lines:?}"
    );
    assert!(
        lines[0].contains("\"schema_version\":1") && lines[0].contains(title),
        "the single record must be the well-formed entry we added; got: {:?}",
        lines[0]
    );

    bin(home.path(), repo.path())
        .args(["memory", "list", "--backend", "git-notes"])
        .assert()
        .success()
        .stdout(predicate::str::contains(title));

    // Explicit git-notes must not create a project or touch the global store.
    assert!(!repo.path().join(".spelunk").exists());
    assert!(!global_memory_db(home.path()).exists());
}

// ── secret-scan gate on the git-notes path ─────────────────────────────────────

#[test]
fn secret_in_entry_is_refused_and_leaves_git_notes_untouched() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    // Title matches the AWS access-key-id pattern (`AKIA` + 16 upper/digits). The
    // secret scan runs before any persistence, so the git-notes fallback path
    // must refuse and write nothing.
    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "AKIAIOSFODNN7EXAMPLE",
            "--body",
            "harmless body",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("secret pattern"));

    assert!(
        spelunk_note_lines(repo.path()).is_empty(),
        "a secret-blocked add must leave refs/notes/spelunk absent/unmodified"
    );
    // And a body-borne secret is likewise blocked before any note is written
    // (GitHub PAT pattern, same fixture the secrets unit test uses).
    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "looks-innocent",
            "--body",
            "token = ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef123456789012",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("secret pattern"));

    assert!(
        spelunk_note_lines(repo.path()).is_empty(),
        "a body-secret-blocked add must also leave the note ref untouched"
    );
}

// ── carrier scope: only add/list ride it; siblings stay fail-closed ────────────

/// The ADR-068 D3 carrier is narrowed to `add`/`list`. Inside a git repo with a
/// commit (exactly the setup where `add`/`list` DO ride the carrier) every other
/// memory subcommand must still fail closed with the ADR-067 single-hatch
/// message, never reach the git-notes path, and never write a note. This guards
/// against the carrier accidentally widening its scope to non-add/list
/// subcommands.
#[test]
fn non_add_list_subcommands_stay_fail_closed_inside_git_repo() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    // A representative spread of the non-carrier subcommands, each needing no
    // server: read (search, timeline) and mutate (supersede).
    let invocations: [&[&str]; 3] = [
        &["memory", "search", "anything"],
        &["memory", "timeline", "anything"],
        &["memory", "supersede", "1", "2"],
    ];
    for args in invocations {
        bin(home.path(), repo.path())
            .args(args)
            .assert()
            .failure()
            // The ADR-067 single-hatch message, NOT the add/list dual-hatch: these
            // subcommands never consult the git repo for a carrier.
            .stderr(predicate::str::contains(NO_PROJECT_ERR))
            .stderr(predicate::str::contains("not inside a git repo").not());
    }

    assert!(
        spelunk_note_lines(repo.path()).is_empty(),
        "a fail-closed non-add/list subcommand must not write any spelunk note"
    );
    assert!(
        !global_memory_db(home.path()).exists(),
        "a fail-closed subcommand must not create the machine-global store"
    );
}

// ── case 6: post-init add writes BOTH the SQLite primary and the write-through ─

/// With a local `.spelunk/` project inside a git repo, a single `add` writes the
/// SQLite primary AND rides the universal git-notes write-through (exactly one
/// record, no double write), and `list` reads back from SQLite. This is the
/// unchanged post-`init` behaviour, asserted end-to-end in one flow.
#[test]
fn post_init_add_writes_sqlite_primary_and_git_notes_write_through() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());
    // A local project makes SQLite the primary (not the pre-init carrier).
    std::fs::create_dir_all(repo.path().join(".spelunk")).unwrap();

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "post-init-both",
            "--body",
            "b",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [decision]"));

    // Primary: the local SQLite store exists (proving branch 3, not the carrier).
    assert!(
        repo.path().join(".spelunk").join("memory.db").exists(),
        "post-init add must write the local SQLite primary"
    );

    // Write-through: exactly one record landed in refs/notes/spelunk (the SQLite
    // primary write plus the write-through must not double up).
    let lines = spelunk_note_lines(repo.path());
    assert_eq!(
        lines.len(),
        1,
        "post-init add must ride the write-through exactly once; got: {lines:?}"
    );
    assert!(
        lines[0].contains("post-init-both"),
        "the write-through record must be the entry we added; got: {:?}",
        lines[0]
    );

    // `list` (default sqlite backend) reads the entry back from SQLite.
    bin(home.path(), repo.path())
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("post-init-both"));

    assert!(!global_memory_db(home.path()).exists());
}

// ── case 7: a failed pre-init carry is fatal (no primary to fall back on) ───────

/// Pre-`init` the carrier is the SOLE writer, so a failed carry has no SQLite
/// primary to absorb it and must surface as a non-zero exit (an `Err`), never a
/// false "Stored". The carry is forced to fail deterministically: HEAD resolves
/// (so `git_head_reachable` engages the carrier) but the `git notes add` the
/// carrier runs has no usable committer identity: no local `user.*`, no
/// system/global config, and `user.useConfigOnly` on so git cannot auto-derive a
/// USER@host fallback (nor honour a stray `$EMAIL`). `git rev-parse HEAD` needs
/// no identity, so the carrier still engages and the failure is in the write.
#[test]
fn failed_pre_init_carry_is_fatal_and_writes_nothing() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();

    // A commit with NO local `user.*` identity in `.git/config`: the setup `git`
    // helper supplies identity via env for the commit only, so HEAD is resolvable
    // but the child's `git notes add` has nothing local to use.
    git(repo.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(repo.path().join("f.txt"), "x\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);

    let mut cmd = spelunk_bin_in(home.path());
    cmd.current_dir(repo.path())
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
        // Neutralize every identity source for the git subprocess spelunk spawns.
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "user.useConfigOnly")
        .env("GIT_CONFIG_VALUE_0", "true")
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "carry-fails",
            "--body",
            "b",
        ]);

    cmd.assert()
        .failure()
        // No false success line, and the error names the fatal-carry context.
        .stdout(predicate::str::contains("Stored").not())
        .stderr(predicate::str::contains(
            "recording memory entry to git notes",
        ));

    assert!(
        spelunk_note_lines(repo.path()).is_empty(),
        "a fatal failed carry must not leave a partial spelunk note"
    );
    assert!(
        !global_memory_db(home.path()).exists(),
        "a fatal failed carry must not create the machine-global store"
    );
}

// ── a contended notes lock fails the writer, loudly (ADR-069 D8) ──────────────

/// The wait budget the carrier allows before giving up on a contended notes
/// lock. Mirrors `LOCK_WAIT_BUDGET` in `storage/git_notes/lock.rs`.
const LOCK_WAIT_BUDGET: Duration = Duration::from_secs(5);

/// `<git-common-dir>/spelunk-notes.lock` — the file the carrier locks, resolved
/// the way the production code resolves it, canonicalization included.
fn notes_lock_path(repo: &Path) -> std::path::PathBuf {
    let raw = git_stdout(repo, &["rev-parse", "--git-common-dir"]);
    let raw = raw.trim();
    let raw = Path::new(raw);
    let common_dir = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        repo.join(raw)
    };
    let common_dir = std::fs::canonicalize(&common_dir).unwrap_or(common_dir);
    common_dir.join("spelunk-notes.lock")
}

/// A pre-`init` `memory add` that cannot take a contended notes lock fails,
/// visibly, and writes nothing (ADR-069 D8).
///
/// This inverts the pre-D8 pin that stood here: contention used to warn and
/// write unlocked, which is the unserialized read-modify-write that silently
/// erases a concurrent writer's entry (#185). An error the user can see and
/// retry costs a command; the silent clobber costs the record.
///
/// Deterministic: this test holds the lock across the child's whole run, from a
/// separate process, so the child is guaranteed to exhaust its budget.
#[test]
fn contended_notes_lock_fails_the_pre_init_carry_and_writes_nothing() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(notes_lock_path(repo.path()))
        .expect("open the notes lock file");
    held.lock()
        .expect("hold the notes lock across the child run");

    let started = Instant::now();
    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "contended-lock-must-not-store",
            "--body",
            "b",
        ])
        .assert()
        .failure()
        // No false success line, and the error tells the user what to do.
        .stdout(predicate::str::contains("Stored").not())
        .stderr(predicate::str::contains("notes lock").and(predicate::str::contains("Retry")));
    let took = started.elapsed();

    drop(held);

    // Negative control: the child must actually have contended. A fast run means
    // it locked a different path than the one held here, leaving the assertions
    // below vacuous.
    assert!(
        took >= LOCK_WAIT_BUDGET,
        "the child must wait out its {LOCK_WAIT_BUDGET:?} lock budget; it returned after \
         {took:?}, so it never contended on {}",
        notes_lock_path(repo.path()).display()
    );

    // The whole point: nothing may be written without the lock.
    let lines = spelunk_note_lines(repo.path());
    assert!(
        lines.is_empty(),
        "a contended carry must write nothing; got: {lines:?}"
    );
}

/// D8's one kept degradation must be **visible**, not merely traced: an
/// unusable lock file makes the write proceed unserialized, and the user must
/// be told on stderr even with `RUST_LOG` unset, because a warning routed only
/// through `tracing` reaches nobody in the shipped binary.
///
/// A directory planted at the lock path makes the open fail deterministically
/// on every platform (EISDIR on unix, access-denied on Windows).
#[test]
fn unusable_notes_lock_degradation_is_visible_without_rust_log() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    std::fs::create_dir_all(notes_lock_path(repo.path()))
        .expect("plant a directory at the notes lock path");

    bin(home.path(), repo.path())
        .env_remove("RUST_LOG")
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "unusable-lock-still-stores",
            "--body",
            "b",
        ])
        .assert()
        // The write itself proceeds: failing every write on a lock-hostile
        // filesystem would make spelunk unusable there (ADR-069 D8).
        .success()
        .stdout(predicate::str::contains("Stored [note]"))
        // And the degradation is surfaced, not swallowed.
        .stderr(predicate::str::contains("without the cross-process lock"));

    let lines = spelunk_note_lines(repo.path());
    assert!(
        lines.len() == 1 && lines[0].contains("unusable-lock-still-stores"),
        "the degraded write must still land exactly one record; got: {lines:?}"
    );
}

// ── ADR-068 A6 retrofit: supersede edges travel via the carrier ───────────────
//
// Both `memory add --supersedes` and `memory supersede` archive the OLD entry
// in the SQLite primary already; what was missing is carrying that edge to
// git notes too, via a second, appended record for OLD (never a rewrite of
// its original line — see `append_state_update`'s doc in
// `storage/git_notes/mod.rs`). The gap this closes: `add.rs` already passed
// `--supersedes` through to the SQLite backend, then wrote
// `superseded_by_entity_id: None` on the write-through record regardless, so
// the edge was silently dropped even when explicitly requested.

/// A JSON-Lines record's `title` and `status`, read together since several
/// assertions below need both to pick the right line out of a note with more
/// than one record for the same title (the OLD entity's original record and
/// its later state-update).
fn title_and_status(line: &str) -> (String, String) {
    (record_field(line, "title"), record_field(line, "status"))
}

/// `memory add --supersedes OLD` must carry OLD's edge to git notes, not just
/// write the NEW entry: OLD's original record stays untouched (append-only),
/// and a second record for OLD lands with `status: archived` and
/// `superseded_by_entity_id` pointing at NEW's `entity_id` — never the other
/// way around.
#[test]
fn post_init_add_supersedes_carries_edge_for_old_entry() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());
    std::fs::create_dir_all(repo.path().join(".spelunk")).unwrap();

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "old-decision",
            "--body",
            "b1",
        ])
        .assert()
        .success();
    let old_lines = spelunk_note_lines(repo.path());
    assert_eq!(old_lines.len(), 1, "setup: OLD's own add");
    let old_id = record_field(&old_lines[0], "id");
    let old_entity_id = record_field(&old_lines[0], "entity_id");

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "new-decision",
            "--body",
            "b2",
            "--supersedes",
            &old_id,
        ])
        .assert()
        .success();

    let lines = spelunk_note_lines(repo.path());
    assert_eq!(
        lines.len(),
        3,
        "OLD's untouched original, NEW's record, and OLD's state-update; got: {lines:?}"
    );

    let new_line = lines
        .iter()
        .find(|l| title_and_status(l) == ("new-decision".to_string(), "active".to_string()))
        .unwrap_or_else(|| panic!("no active new-decision record in {lines:?}"));
    let new_entity_id = record_field(new_line, "entity_id");
    assert!(
        !new_line.contains("superseded_by_entity_id"),
        "the edge must never land on NEW's record; got: {new_line}"
    );

    let old_original = lines
        .iter()
        .find(|l| title_and_status(l) == ("old-decision".to_string(), "active".to_string()))
        .unwrap_or_else(|| {
            panic!("OLD's original active record must survive untouched: {lines:?}")
        });
    assert_eq!(
        record_field(old_original, "entity_id"),
        old_entity_id,
        "OLD's original record must be byte-identical in identity, never rewritten"
    );

    let old_update = lines
        .iter()
        .find(|l| title_and_status(l) == ("old-decision".to_string(), "archived".to_string()))
        .unwrap_or_else(|| panic!("OLD's state-update record is missing: {lines:?}"));
    assert_eq!(
        record_field(old_update, "entity_id"),
        old_entity_id,
        "the state-update record must carry OLD's own entity_id, not NEW's"
    );
    assert_eq!(
        record_field(old_update, "superseded_by_entity_id"),
        new_entity_id,
        "the edge must point at NEW's entity_id"
    );
    assert!(
        !record_field(old_update, "invalid_at").is_empty(),
        "the state-update record must set invalid_at"
    );
}

/// `memory supersede OLD NEW` carries the same edge, via the same shared
/// carrier helper, when both entries already exist as separate `add`s.
#[test]
fn post_init_supersede_command_carries_edge_to_git_notes() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());
    std::fs::create_dir_all(repo.path().join(".spelunk")).unwrap();

    let add = |title: &str, body: &str| {
        bin(home.path(), repo.path())
            .args([
                "memory", "add", "--kind", "decision", "--title", title, "--body", body,
            ])
            .assert()
            .success();
    };
    add("old-via-supersede", "b1");
    add("new-via-supersede", "b2");

    let seeded = spelunk_note_lines(repo.path());
    assert_eq!(seeded.len(), 2, "setup: two independent adds");
    let old_id = record_field(&seeded[0], "id");
    let new_id = record_field(&seeded[1], "id");
    let new_entity_id = record_field(&seeded[1], "entity_id");

    bin(home.path(), repo.path())
        .args(["memory", "supersede", &old_id, &new_id])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Archived #").and(predicate::str::contains("superseded by #")),
        );

    let lines = spelunk_note_lines(repo.path());
    assert_eq!(
        lines.len(),
        3,
        "OLD's untouched original, NEW's record, and OLD's state-update; got: {lines:?}"
    );

    let old_update = lines
        .iter()
        .find(|l| title_and_status(l) == ("old-via-supersede".to_string(), "archived".to_string()))
        .unwrap_or_else(|| panic!("OLD's state-update record is missing: {lines:?}"));
    assert_eq!(
        record_field(old_update, "superseded_by_entity_id"),
        new_entity_id,
        "the edge must point at NEW's entity_id"
    );

    let new_line = lines
        .iter()
        .find(|l| title_and_status(l) == ("new-via-supersede".to_string(), "active".to_string()))
        .unwrap_or_else(|| panic!("NEW's record is missing: {lines:?}"));
    assert!(
        !new_line.contains("superseded_by_entity_id"),
        "the edge must never land on NEW's record; got: {new_line}"
    );
}

/// `memory add --supersedes OLD` run entirely **pre-`init`** (no `.spelunk/`,
/// carrier-only, as in `memory_add_list_round_trips_via_git_notes_fallback`
/// above): both OLD and NEW exist only via the git-notes carrier, since there
/// is no SQLite primary yet. The edge-carry block in `add.rs` ("Carry the OLD
/// entity's supersede edge too") only runs when a primary backend handle was
/// opened (`primary_backend.as_ref()`), which is never the case pre-init —
/// so today the edge is silently dropped: no state-update record is
/// appended, and no warning is printed, even though the command prints a
/// plain "Stored" success line as if the `--supersedes` request succeeded.
///
/// This currently fails, pinning the gap: `spelunk memory add --supersedes`
/// pre-init drops the edge exactly the way the pre-fix post-init path used
/// to (the case this whole task exists to close), just on the other half of
/// the carrier's supported command surface (ADR-068 D3).
#[test]
fn pre_init_add_supersedes_carries_edge_for_old_entry() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "pre-init-old",
            "--body",
            "b1",
        ])
        .assert()
        .success();
    let old_lines = spelunk_note_lines(repo.path());
    assert_eq!(old_lines.len(), 1, "setup: OLD's own pre-init add");
    let old_id = record_field(&old_lines[0], "id");

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "pre-init-new",
            "--body",
            "b2",
            "--supersedes",
            &old_id,
        ])
        .assert()
        .success();

    let lines = spelunk_note_lines(repo.path());
    assert_eq!(
        lines.len(),
        3,
        "expected OLD's untouched original, NEW's record, and a state-update \
         archiving OLD (mirroring the post-init behaviour proven above); got \
         only {lines:?} — pre-init, the `--supersedes` edge is being \
         silently dropped while the command still reports plain success"
    );
    assert!(
        lines
            .iter()
            .any(|l| title_and_status(l) == ("pre-init-old".to_string(), "archived".to_string())),
        "OLD must gain an archived state-update record even pre-init; got: {lines:?}"
    );
}

// ── ADR-068 amendment E4: re-supersede of an already-archived OLD is rejected ──
//
// `add_note_superseding`'s archive-OLD UPDATE used to silently no-op when OLD
// was already archived, and neither it nor `add.rs` inspected that outcome —
// unlike `memory supersede`, which already rejects a stale OLD. These pin the
// fix: `memory add --supersedes OLD` against an already-archived OLD must now
// fail the whole command, before any write, on both storage paths.

/// Post-`init` (SQLite primary + git-notes carrier): a second
/// `--supersedes OLD` against an OLD already archived by a first
/// `--supersedes OLD` call must fail loudly, write no new note, and leave the
/// git-notes carrier exactly as the first (successful) call left it — no
/// orphaned successor record, no second conflicting state-update for OLD.
#[test]
fn post_init_add_supersedes_rejects_already_archived_old() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());
    std::fs::create_dir_all(repo.path().join(".spelunk")).unwrap();

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "old-decision",
            "--body",
            "b1",
        ])
        .assert()
        .success();
    let old_lines = spelunk_note_lines(repo.path());
    let old_id = record_field(&old_lines[0], "id");

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "successor-a",
            "--body",
            "b2",
            "--supersedes",
            &old_id,
        ])
        .assert()
        .success();

    let lines_after_first_supersede = spelunk_note_lines(repo.path());
    assert_eq!(
        lines_after_first_supersede.len(),
        3,
        "setup: OLD's original, successor A's record, OLD's state-update"
    );

    // Re-supersede the now-archived OLD with a second, different successor.
    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "successor-b",
            "--body",
            "b3",
            "--supersedes",
            &old_id,
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(format!(
            "No active memory entry with id {old_id} (old)"
        )));

    // No new SQLite row: `memory list --archived` still shows only the two
    // entries from the first (successful) supersede.
    let list_output = bin(home.path(), repo.path())
        .args([
            "memory",
            "list",
            "--format",
            "jsonl",
            "--archived",
            "--limit",
            "100",
        ])
        .output()
        .unwrap();
    assert!(list_output.status.success());
    let stdout = String::from_utf8_lossy(&list_output.stdout);
    let entry_count = stdout.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(
        entry_count, 2,
        "a rejected --supersedes must not create an orphaned new note row; got: {stdout}"
    );

    // No new git-notes carrier record either: still exactly the 3 lines the
    // first, successful supersede produced.
    let lines_after_rejected_supersede = spelunk_note_lines(repo.path());
    assert_eq!(
        lines_after_rejected_supersede.len(),
        3,
        "a rejected --supersedes must write neither a new-entry record nor a \
         second conflicting state-update for OLD; got: {lines_after_rejected_supersede:?}"
    );
}

/// Pre-`init` (git-notes-only, no SQLite primary): the same rejection, and the
/// same "write nothing" contract — critically, the new entry's *own*
/// git-notes record must never be written either, since the pre-flight check
/// runs before it.
#[test]
fn pre_init_add_supersedes_rejects_already_archived_old() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "pre-init-old",
            "--body",
            "b1",
        ])
        .assert()
        .success();
    let old_lines = spelunk_note_lines(repo.path());
    let old_id = record_field(&old_lines[0], "id");

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "pre-init-successor-a",
            "--body",
            "b2",
            "--supersedes",
            &old_id,
        ])
        .assert()
        .success();

    let lines_after_first_supersede = spelunk_note_lines(repo.path());
    assert_eq!(
        lines_after_first_supersede.len(),
        3,
        "setup: OLD's original, successor A's record, OLD's state-update"
    );

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "pre-init-successor-b",
            "--body",
            "b3",
            "--supersedes",
            &old_id,
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(format!(
            "No active memory entry with id {old_id} (old)"
        )));

    let lines_after_rejected_supersede = spelunk_note_lines(repo.path());
    assert_eq!(
        lines_after_rejected_supersede.len(),
        3,
        "a rejected pre-init --supersedes must not write successor B's own \
         record, nor a second state-update for OLD; got: {lines_after_rejected_supersede:?}"
    );
    assert!(
        !lines_after_rejected_supersede
            .iter()
            .any(|l| l.contains("pre-init-successor-b")),
        "successor B's record must never be written when the pre-flight check \
         rejects the supersede; got: {lines_after_rejected_supersede:?}"
    );
}
