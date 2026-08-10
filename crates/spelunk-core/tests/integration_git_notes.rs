//! Integration tests for `GitNotesBackend` — concurrency and round-trip.
//!
//! These tests require `git` to be on PATH and are skipped if the current
//! working directory is not inside a git repository (CI environments without
//! git are unaffected).
//!
//! ## Concurrent-write safety (#185)
//!
//! Writes are a read-modify-write that rewrites the HEAD note with
//! `git notes add -f` (replace semantics). Two agents writing to the *same
//! HEAD* concurrently would race, and the loser's entry would vanish silently
//! with both exiting 0. ADR-069 (D6) serializes every read-modify-write with a
//! lock in the git common dir; D8 makes a writer that cannot take it fail
//! visibly. The invariant is therefore "every entry lands or its writer fails
//! loudly", never "all must land": heavy legitimate contention may exceed the
//! wait budget, and that surfaces as an error, not a loss.

mod common;

use serial_test::serial;
use spelunk_core::storage::GitNotesBackend;
use spelunk_core::storage::MemoryBackend;
use spelunk_core::storage::NoteId;
use spelunk_core::storage::NoteInput;

// ── helpers ──────────────────────────────────────────────────────────────────
//
// Git-config isolation (`isolate_git_config` / `git_command`) now lives in
// `tests/common/mod.rs`, shared across every spelunk-core integration test
// that spawns git rather than copy-pasted per file.

/// Create a temporary git repo with one initial commit.
/// Returns the path; the repo is cleaned up when the returned `TempDir` drops.
fn make_temp_git_repo() -> tempfile::TempDir {
    common::isolate_git_config();
    let dir = tempfile::TempDir::new().expect("tempdir");
    let p = dir.path();

    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(p)
            .output()
            .expect("git command")
    };

    run(&["init", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    // Create an initial commit so HEAD resolves.
    std::fs::write(p.join("README.md"), "test").expect("write");
    run(&["add", "."]);
    run(&[
        "commit",
        "--no-gpg-sign",
        "-m",
        "init",
        "--allow-empty-message",
    ]);

    dir
}

fn note_input(kind: &str, title: &str) -> NoteInput {
    NoteInput {
        kind: kind.to_string(),
        title: title.to_string(),
        body: format!("body for {title}"),
        tags: vec![],
        linked_files: vec![],
        embedding: None,
        source_ref: None,
        valid_at: None,
        supersedes: None,
    }
}

// ── basic round-trip ─────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn git_notes_add_and_list_round_trip() {
    let dir = make_temp_git_repo();
    let backend = GitNotesBackend::with_root(dir.path().to_path_buf());

    let (id, _) = backend
        .add(note_input("decision", "use sqlcipher"))
        .await
        .expect("add");

    let notes = backend
        .list(Some("decision"), 10, false, None)
        .await
        .expect("list");

    assert_eq!(notes.len(), 1, "expected exactly one note");
    assert_eq!(notes[0].id, id);
    assert_eq!(notes[0].title, "use sqlcipher");
    assert_eq!(notes[0].kind, "decision");
}

#[tokio::test]
#[serial]
async fn git_notes_list_without_kind_returns_all() {
    let dir = make_temp_git_repo();
    let backend = GitNotesBackend::with_root(dir.path().to_path_buf());

    backend
        .add(note_input("decision", "first"))
        .await
        .expect("add 1");

    // Make a second commit so the two notes don't overwrite each other.
    std::fs::write(dir.path().join("a.txt"), "a").expect("write");
    std::process::Command::new("git")
        .args(["-C", dir.path().to_str().unwrap(), "add", "."])
        .output()
        .ok();
    std::process::Command::new("git")
        .args([
            "-C",
            dir.path().to_str().unwrap(),
            "commit",
            "--no-gpg-sign",
            "--allow-empty",
            "-m",
            "second commit",
        ])
        .output()
        .expect("second commit");

    backend
        .add(note_input("note", "second"))
        .await
        .expect("add 2");

    let all = backend.list(None, 10, false, None).await.expect("list");
    assert_eq!(all.len(), 2, "expected two notes across two commits");
}

// ── list_by_source_ref: anchor-commit filtering ──────────────────────────────

// The HEAD sha of the repo at `root`, isolated from the developer's git config.
fn head_sha(root: &std::path::Path) -> String {
    let out = common::git_command(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git rev-parse HEAD");
    assert!(out.status.success(), "git rev-parse HEAD failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// On the git-notes-primary path, `list_by_source_ref` filters by the commit
// each entry's note is ANCHORED to (the attachment target), not by a stored
// field — the carrier never records the anchor commit inside the record. An
// entry added while HEAD was commit A must be returned for A and only A.
#[tokio::test]
#[serial]
async fn git_notes_list_by_source_ref_filters_by_anchor_commit() {
    let dir = make_temp_git_repo();
    let root = dir.path();
    let backend = GitNotesBackend::with_root(root.to_path_buf());

    // Entry one anchors to commit A (the initial commit / current HEAD).
    backend
        .add(note_input("decision", "on-A"))
        .await
        .expect("add A");
    let sha_a = head_sha(root);

    // Move HEAD to commit B, then entry two anchors to B.
    common::git_command(root)
        .args(["commit", "--no-gpg-sign", "--allow-empty", "-m", "second"])
        .output()
        .expect("second commit");
    let sha_b = head_sha(root);
    assert_ne!(
        sha_a, sha_b,
        "the two entries must anchor to distinct commits"
    );
    backend
        .add(note_input("note", "on-B"))
        .await
        .expect("add B");

    let on_a = backend
        .list_by_source_ref(&sha_a, 50, false, None)
        .await
        .expect("list A");
    let titles_a: Vec<&str> = on_a.iter().map(|n| n.title.as_str()).collect();
    assert_eq!(titles_a, vec!["on-A"], "A must return only A's entry");

    let on_b = backend
        .list_by_source_ref(&sha_b, 50, false, None)
        .await
        .expect("list B");
    let titles_b: Vec<&str> = on_b.iter().map(|n| n.title.as_str()).collect();
    assert_eq!(titles_b, vec!["on-B"], "B must return only B's entry");

    // A short prefix of A's sha matches too.
    let on_a_prefix = backend
        .list_by_source_ref(&sha_a[..8], 50, false, None)
        .await
        .expect("list A prefix");
    assert_eq!(
        on_a_prefix.len(),
        1,
        "the 8-char prefix must match A's entry"
    );

    // A commit that carries no note returns nothing (no false positives).
    let none = backend
        .list_by_source_ref("0000000000000000000000000000000000000000", 50, false, None)
        .await
        .expect("list unrelated");
    assert!(none.is_empty(), "a commit with no note must return nothing");
}

// ── concurrent write safety (#185) ───────────────────────────────────────────

/// Two tasks writing a note to the same HEAD concurrently must both survive, as
/// well-formed records of the two distinct writers. Serialized by the notes
/// lock, so the count is exact rather than timing-dependent.
#[tokio::test]
#[serial]
async fn git_notes_concurrent_same_head_stays_consistent() {
    let dir = make_temp_git_repo();

    let root = dir.path().to_path_buf();
    let b1 = GitNotesBackend::with_root(root.clone());
    let b2 = GitNotesBackend::with_root(root.clone());

    // Two concurrent adds to the same HEAD; both must return Ok (no panic).
    let (r1, r2) = tokio::join!(
        b1.add(note_input("note", "agent A")),
        b2.add(note_input("note", "agent B")),
    );
    r1.expect("agent A add");
    r2.expect("agent B add");

    let notes = GitNotesBackend::with_root(dir.path().to_path_buf())
        .list(None, 10, false, None)
        .await
        .expect("list");

    // Serialized by the notes lock: neither writer's entry may be dropped.
    assert_eq!(
        notes.len(),
        2,
        "both concurrent same-HEAD adds must survive; got {} ({:?})",
        notes.len(),
        notes.iter().map(|n| &n.title).collect::<Vec<_>>()
    );
    // Every surviving entry is a well-formed note we wrote — no partial/garbled
    // records from an interleaved write.
    for n in &notes {
        assert_eq!(n.kind, "note", "unexpected kind: {}", n.kind);
        assert!(
            n.title == "agent A" || n.title == "agent B",
            "unexpected title: {}",
            n.title
        );
    }
    assert_ne!(
        notes[0].title, notes[1].title,
        "the survivors are the two distinct writers, not one entry twice"
    );
}

/// Deterministic sibling of the concurrent test: two *sequential* adds to the
/// same HEAD both survive as distinct records. The concurrent test only reaches
/// the 2-note path on a rare scheduling race (~1% of runs), so its distinct /
/// well-formed checks are seldom exercised; this locks the same-HEAD append
/// contract down on every run without any timing dependence.
#[tokio::test]
#[serial]
async fn git_notes_sequential_same_head_both_survive_distinct() {
    let dir = make_temp_git_repo();
    let backend = GitNotesBackend::with_root(dir.path().to_path_buf());

    backend.add(note_input("note", "agent A")).await.expect("A");
    backend.add(note_input("note", "agent B")).await.expect("B");

    let notes = backend.list(None, 10, false, None).await.expect("list");
    assert_eq!(notes.len(), 2, "both sequential same-HEAD adds survive");
    for n in &notes {
        assert_eq!(n.kind, "note", "unexpected kind: {}", n.kind);
    }
    let titles: std::collections::HashSet<&str> = notes.iter().map(|n| n.title.as_str()).collect();
    assert_eq!(
        titles,
        ["agent A", "agent B"].into_iter().collect(),
        "both writers present and distinct"
    );
}

/// Archive marks an entry with status=archived and hides it from default list.
#[tokio::test]
#[serial]
async fn git_notes_archive_hides_entry() {
    let dir = make_temp_git_repo();
    let backend = GitNotesBackend::with_root(dir.path().to_path_buf());

    let (id, _) = backend
        .add(note_input("decision", "archive me"))
        .await
        .expect("add");

    let archived = backend.archive(id.clone()).await.expect("archive");
    assert!(archived);

    let active = backend
        .list(None, 10, false, None)
        .await
        .expect("list active");
    assert!(
        active.is_empty(),
        "archived entry should not appear in default list"
    );

    let all = backend
        .list(None, 10, true, None)
        .await
        .expect("list including archived");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].status, "archived");
}

/// Unsupported methods return clear errors rather than panicking.
#[tokio::test]
#[serial]
async fn git_notes_unsupported_methods_return_errors() {
    let dir = make_temp_git_repo();
    let backend = GitNotesBackend::with_root(dir.path().to_path_buf());

    assert!(backend.search(&[], "q", 5, None).await.is_err());
    assert!(backend.search_hybrid(&[], "q", 5, None).await.is_err());
    assert!(backend.search_text("q", 5, None).await.is_err());
    assert!(backend.search_timeline(&[], "q", 5).await.is_err());
    assert!(backend.harvested_shas().await.is_err());
    assert!(backend.has_source_ref("abc123").await.is_err());
    assert!(backend.add_edge(1, 2, "relates_to").await.is_err());
    assert!(backend.get_edges(1).await.is_err());
    assert!(
        backend
            .supersede(NoteId::from_i64(1), NoteId::from_i64(2))
            .await
            .is_err()
    );
}

// ── append_to_git_notes write-through helper ─────────────────────────────────

use spelunk_core::storage::{NoteRecord, append_to_git_notes, entity_id};

fn make_note_record(id: i64, title: &str) -> NoteRecord {
    let kind = "decision".to_string();
    let body = format!("body for {title}");
    NoteRecord {
        schema_version: 1,
        id,
        entity_id: Some(entity_id(&kind, title, &body)),
        kind,
        title: title.to_string(),
        body,
        tags: vec![],
        linked_files: vec![],
        created_at: 0,
        status: "active".to_string(),
        source_ref: None,
        valid_at: None,
        invalid_at: None,
        superseded_by: None,
        remote_id: None,
        superseded_by_entity_id: None,
    }
}

/// (a) A single `append_to_git_notes` call writes a parseable note to HEAD.
#[tokio::test]
#[serial]
async fn append_to_git_notes_writes_note_when_enabled() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let record = make_note_record(1, "first decision");
    append_to_git_notes(Some(root), &record)
        .await
        .expect("append should succeed");

    // Read back the raw note text.
    let out = std::process::Command::new("git")
        .args(["notes", "--ref=spelunk", "show", "HEAD"])
        .current_dir(root)
        .output()
        .expect("git notes show");
    assert!(out.status.success(), "note should exist on HEAD");

    let text = String::from_utf8_lossy(&out.stdout);
    let trimmed = text.trim();
    assert!(!trimmed.is_empty(), "note should not be empty");

    // Should be valid JSON on a single line.
    let parsed: serde_json::Value =
        serde_json::from_str(trimmed).expect("note should be valid JSON");
    assert_eq!(
        parsed["title"].as_str().unwrap(),
        "first decision",
        "title should round-trip"
    );
    assert_eq!(parsed["id"].as_i64().unwrap(), 1);
}

/// (b) A second `append_to_git_notes` call appends rather than overwrites.
#[tokio::test]
#[serial]
async fn append_to_git_notes_appends_not_overwrites() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let rec1 = make_note_record(10, "first");
    let rec2 = make_note_record(20, "second");

    append_to_git_notes(Some(root), &rec1)
        .await
        .expect("first append");
    append_to_git_notes(Some(root), &rec2)
        .await
        .expect("second append");

    // Both entries should be present as separate JSON lines.
    let out = std::process::Command::new("git")
        .args(["notes", "--ref=spelunk", "show", "HEAD"])
        .current_dir(root)
        .output()
        .expect("git notes show");
    assert!(out.status.success());

    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();

    assert_eq!(
        lines.len(),
        2,
        "expected 2 JSON lines after two appends; got:\n{text}"
    );

    let ids: Vec<i64> = lines
        .iter()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).expect("valid JSON line");
            v["id"].as_i64().expect("id field")
        })
        .collect();

    assert!(ids.contains(&10), "first record (id=10) should be present");
    assert!(ids.contains(&20), "second record (id=20) should be present");
}

/// (c) `store_in_git_notes = false` must skip the git note write entirely.
///
/// We verify this by calling `append_to_git_notes` only when the flag is true
/// and confirming no note exists when it is false.  The actual config-flag
/// gating happens in `memory_add`; here we test the conditional call pattern.
#[tokio::test]
#[serial]
async fn append_to_git_notes_skipped_when_flag_is_false() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    // Simulate store_in_git_notes = false: do NOT call append_to_git_notes.
    let store_in_git_notes = false;
    let record = make_note_record(99, "should not appear");
    if store_in_git_notes {
        append_to_git_notes(Some(root), &record)
            .await
            .expect("would append");
    }

    let out = std::process::Command::new("git")
        .args(["notes", "--ref=spelunk", "show", "HEAD"])
        .current_dir(root)
        .output()
        .expect("git notes show command");

    // When the flag is false no note is written, so git notes show should fail.
    assert!(
        !out.status.success(),
        "no note should exist when store_in_git_notes=false"
    );
}

/// (d) `append_to_git_notes` returns `Err` when HEAD does not exist (not a git repo).
///     The CLI path wraps this in a `tracing::warn!` + `return Ok(())` — this test
///     verifies the error surface so callers can rely on it.
#[tokio::test]
#[serial]
async fn append_to_git_notes_returns_err_outside_git_repo() {
    let non_git_dir = tempfile::TempDir::new().expect("tempdir");
    let record = make_note_record(1, "test");
    let result = append_to_git_notes(Some(non_git_dir.path()), &record).await;
    assert!(result.is_err(), "should return Err when not in a git repo");
}

// ── security regression: note bodies must go via stdin, never argv ──────────
//
// `git notes add` used to receive the note body as a `-m <arg>` argv
// value. A body that itself looked like a git option (e.g. starting with `-`)
// could previously be misparsed as an option to `git notes add` rather than
// literal note content. These tests prove (a) option-like / metacharacter-
// laden bodies round-trip as *literal* note text rather than being
// interpreted or truncated, which is only possible if they're delivered via
// stdin (`-F -`) rather than argv (`-m`), and (b) the same holds for
// `GitNotesBackend::add`/`archive`, which route through `add_note_stdin`.

/// A note body that is itself option-shaped (starts with `-`) must be stored
/// and read back byte-for-byte, not interpreted as a `git notes add` option
/// or truncated. This would fail (or `git notes add` would error/misbehave)
/// if the body were still passed via `-m <body>` on argv.
#[tokio::test]
#[serial]
async fn append_to_git_notes_body_starting_with_dash_is_literal() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let mut record = make_note_record(1, "dash body");
    record.body = "--force-with-lease=origin/main --output=/tmp/pwned".to_string();

    append_to_git_notes(Some(root), &record)
        .await
        .expect("append should succeed even with option-like body");

    let out = std::process::Command::new("git")
        .args(["notes", "--ref=spelunk", "show", "HEAD"])
        .current_dir(root)
        .output()
        .expect("git notes show");
    assert!(out.status.success(), "note should exist on HEAD");

    let text = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(text.trim()).expect("note should be valid JSON");
    assert_eq!(
        parsed["body"].as_str().unwrap(),
        "--force-with-lease=origin/main --output=/tmp/pwned",
        "option-like body must round-trip literally, proving it was never argv-parsed"
    );

    // The exploit this guards against: if the body had leaked onto argv as an
    // option value or been split by the shell/argv parser, it could reach
    // paths outside the repo. Confirm no such file was created as a side
    // effect of writing this note.
    assert!(
        !std::path::Path::new("/tmp/pwned").exists(),
        "option-like note body must never be interpreted as a git option"
    );
    let _ = std::fs::remove_file("/tmp/pwned");
}

/// A note body containing shell metacharacters must round-trip untouched.
/// All git spawns in this codebase use argv vectors (no shell), so this is
/// defense-in-depth / regression coverage rather than a shell-injection PoC.
#[tokio::test]
#[serial]
async fn append_to_git_notes_body_with_shell_metacharacters_is_literal() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let mut record = make_note_record(2, "metachar body");
    record.body = "$(rm -rf /); `echo pwned`; a && b || c; a | b > /tmp/x; a; b\nnewline".into();

    append_to_git_notes(Some(root), &record)
        .await
        .expect("append should succeed with shell-metacharacter body");

    let out = std::process::Command::new("git")
        .args(["notes", "--ref=spelunk", "show", "HEAD"])
        .current_dir(root)
        .output()
        .expect("git notes show");
    assert!(out.status.success());

    let text = String::from_utf8_lossy(&out.stdout);
    // Body is one line of a `\n`-joined ledger; find the JSON line for id=2.
    let line = text
        .lines()
        .find(|l| l.contains("\"id\":2"))
        .expect("record for id=2 present");
    let parsed: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
    assert_eq!(
        parsed["body"].as_str().unwrap(),
        "$(rm -rf /); `echo pwned`; a && b || c; a | b > /tmp/x; a; b\nnewline",
        "shell-metacharacter body must round-trip literally"
    );
}

/// `GitNotesBackend::add` (used by the `MemoryBackend` trait impl, i.e. the
/// `spelunk memory add` path) also writes via `add_note_stdin`. Confirm an
/// option-like note title/body round-trips literally through that path too —
/// not just through the lower-level `append_to_git_notes` free function.
#[tokio::test]
#[serial]
async fn git_notes_backend_add_with_option_like_body_round_trips() {
    let dir = make_temp_git_repo();
    let backend = GitNotesBackend::with_root(dir.path().to_path_buf());

    let mut input = note_input("decision", "--amend");
    input.body = "-f --force --output=/tmp/should-not-exist-oss61".to_string();

    let (id, _) = backend.add(input).await.expect("add");

    let notes = backend
        .list(Some("decision"), 10, false, None)
        .await
        .expect("list");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].id, id);
    assert_eq!(notes[0].title, "--amend");
    assert_eq!(
        notes[0].body, "-f --force --output=/tmp/should-not-exist-oss61",
        "option-like body must round-trip literally via GitNotesBackend::add"
    );

    assert!(!std::path::Path::new("/tmp/should-not-exist-oss61").exists());
}

// ── JSONL canonical: permissive read / preserving write (ADR-059) ────────────
//
// A note blob is JSON Lines interleaved with foreign content (prose, other
// tools' lines). Reads skip foreign lines without erroring; writes preserve
// every foreign line and every untargeted spelunk record byte-for-byte.

/// Write a raw note blob verbatim to HEAD's `refs/notes/spelunk` note, via
/// stdin (`-F -`) so arbitrary content is delivered literally.
fn write_raw_note(root: &std::path::Path, blob: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let head = String::from_utf8(
        Command::new("git")
            .args(["-C", root.to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
            .expect("rev-parse")
            .stdout,
    )
    .expect("utf8");
    let head = head.trim();
    let mut child = Command::new("git")
        .args([
            "-C",
            root.to_str().unwrap(),
            "notes",
            "--ref=spelunk",
            "add",
            "-f",
            "-F",
            "-",
            "--",
            head,
        ])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn git notes add");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(blob.as_bytes())
        .expect("write blob");
    assert!(child.wait().expect("wait").success(), "git notes add");
}

/// Read HEAD's raw `refs/notes/spelunk` note blob.
fn read_raw_note(root: &std::path::Path) -> String {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            root.to_str().unwrap(),
            "notes",
            "--ref=spelunk",
            "show",
            "HEAD",
        ])
        .output()
        .expect("git notes show");
    assert!(out.status.success(), "note should exist");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The ADR-059 conformance fixture: three real `decision` records interleaved
/// with markdown prose and blank lines.
fn adr_conformance_blob() -> String {
    let rec = |id: i64, memory: &str| {
        serde_json::to_string(&make_note_record(id, memory)).expect("serialize record")
    };
    format!(
        "# Implement payment by Stripe\n\
         \n\
         We're implementing a payment rail, in this case Stripe...\n\
         \n\
         {}\n\
         \n\
         ## Technical details\n\
         \n\
         ...Axum as the http handler layer.\n\
         \n\
         {}\n\
         {}\n",
        rec(1, "use stripe for payment processing"),
        rec(2, "rust is our language of choice"),
        rec(3, "use axum to implement api's over restful http"),
    )
}

/// (a) The ADR worked example round-trips: reading yields exactly the three
/// `decision` records in order, ignoring the four prose blocks and blank lines,
/// with no error.
#[tokio::test]
#[serial]
async fn git_notes_adr_conformance_read_skips_prose() {
    let dir = make_temp_git_repo();
    let root = dir.path();
    write_raw_note(root, &adr_conformance_blob());

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let notes = backend.list(None, 100, false, None).await.expect("list");

    assert_eq!(notes.len(), 3, "three records, prose skipped");
    assert_eq!(notes[0].id, NoteId::from_i64(1));
    assert_eq!(notes[1].id, NoteId::from_i64(2));
    assert_eq!(notes[2].id, NoteId::from_i64(3));
    assert_eq!(notes[0].title, "use stripe for payment processing");
    assert_eq!(
        notes[2].title,
        "use axum to implement api's over restful http"
    );
}

/// (b) A blob with a foreign line and no spelunk records reads as an empty list
/// with no error; a permissive read never fails on unparseable lines.
#[tokio::test]
#[serial]
async fn git_notes_read_foreign_only_is_empty_no_error() {
    let dir = make_temp_git_repo();
    let root = dir.path();
    // Prose, a non-object JSON value, and a malformed line: all foreign.
    write_raw_note(
        root,
        "just some freeform notes from another tool\n[1, 2, 3]\n{not valid json",
    );

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let notes = backend.list(None, 100, true, None).await.expect("list");
    assert!(notes.is_empty(), "no spelunk records, no error");
}

/// (c) `add` (append) preserves all prior content: the four prose blocks and the
/// three original records are retained, and the blob now holds four records.
#[tokio::test]
#[serial]
async fn git_notes_add_preserves_prose_and_siblings() {
    let dir = make_temp_git_repo();
    let root = dir.path();
    write_raw_note(root, &adr_conformance_blob());

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    backend
        .add(note_input("decision", "a fourth decision"))
        .await
        .expect("add fourth");

    // Prose retained verbatim.
    let blob = read_raw_note(root);
    assert!(
        blob.contains("# Implement payment by Stripe"),
        "heading kept"
    );
    assert!(
        blob.contains("...Axum as the http handler layer."),
        "prose kept"
    );

    // Four records now readable, originals intact and in order.
    let notes = backend.list(None, 100, false, None).await.expect("list");
    assert_eq!(notes.len(), 4, "three original + one appended");
    assert_eq!(notes[0].id, NoteId::from_i64(1));
    assert_eq!(notes[1].id, NoteId::from_i64(2));
    assert_eq!(notes[2].id, NoteId::from_i64(3));
    assert_eq!(notes[3].title, "a fourth decision");
}

/// (c) `archive` of the middle record sets only that record's status; the other
/// two records and all prose lines are unchanged in content and position.
#[tokio::test]
#[serial]
async fn git_notes_archive_does_not_clobber_siblings_or_prose() {
    let dir = make_temp_git_repo();
    let root = dir.path();
    write_raw_note(root, &adr_conformance_blob());

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let archived = backend
        .archive(NoteId::from_i64(2))
        .await
        .expect("archive middle");
    assert!(archived, "middle record archived");

    // Prose retained.
    let blob = read_raw_note(root);
    assert!(
        blob.contains("# Implement payment by Stripe"),
        "heading kept"
    );
    assert!(blob.contains("## Technical details"), "second heading kept");

    // Records 1 and 3 still active; only record 2 archived.
    let active = backend.list(None, 100, false, None).await.expect("active");
    let active_ids: Vec<NoteId> = active.iter().map(|n| n.id.clone()).collect();
    assert_eq!(
        active_ids,
        vec![NoteId::from_i64(1), NoteId::from_i64(3)],
        "only the middle record is hidden"
    );

    let all = backend.list(None, 100, true, None).await.expect("all");
    assert_eq!(all.len(), 3, "all three records still present");
    let rec2 = all
        .iter()
        .find(|n| n.id == NoteId::from_i64(2))
        .expect("record 2 present");
    assert_eq!(rec2.status, "archived", "only record 2 is archived");
}

/// `archive` must append a new state-update record for the entity rather than
/// rewrite the matched line in place, mirroring
/// `append_state_update_appends_never_rewrites` but for `GitNotesBackend`'s
/// own primary-store `archive` (`--backend git-notes`, not the SQLite-primary
/// carrier that free function serves). Checked directly on the raw ref,
/// single-repo: a two-clone merge cannot distinguish the two here, because
/// `cat_sort_uniq` reconstructs a rewrite's discarded original line from any
/// clone that never touched it, making a rewrite and an append converge to
/// the same union either way. Only the raw line count on the writing repo
/// itself, before any merge, tells them apart.
#[tokio::test]
#[serial]
async fn git_notes_backend_archive_appends_never_rewrites() {
    let dir = make_temp_git_repo();
    let root = dir.path();
    let backend = GitNotesBackend::with_root(root.to_path_buf());

    let (id, _created) = backend
        .add(note_input("decision", "archive me"))
        .await
        .expect("add");
    let original = read_raw_note(root);
    let original_line = original
        .lines()
        .next()
        .expect("one seeded line")
        .to_string();
    let original_record: NoteRecord = serde_json::from_str(&original_line).expect("parse original");

    let archived = backend.archive(id.clone()).await.expect("archive");
    assert!(archived);

    let after = read_raw_note(root);
    let lines: Vec<&str> = after.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        2,
        "archiving must append a state-update record, not rewrite the \
         original line in place: {after}"
    );
    assert_eq!(
        lines[0], original_line,
        "the original active line must survive byte-for-byte"
    );

    let update: NoteRecord = serde_json::from_str(lines[1]).expect("parse appended record");
    assert_eq!(update.status, "archived");
    assert_ne!(
        NoteId::from_i64(update.id),
        id.clone(),
        "the appended state-update mints its own id, never reusing the \
         original rowid"
    );
    assert_eq!(
        update.entity_id, original_record.entity_id,
        "the appended record must target the entity by entity_id (ADR-068 A6), \
         not the rowid the old in-place rewrite matched on"
    );

    // The fold still converges to one entry, correctly archived.
    let all = backend.list(None, 10, true, None).await.expect("list all");
    assert_eq!(all.len(), 1, "the two raw lines fold to one entity");
    assert_eq!(all[0].status, "archived");
}

/// Archiving the same entity twice (sequentially, one machine) must converge
/// to one folded, archived entry, not a duplicate or a conflict, even though
/// each call appends its own state-update record with its own minted id.
#[tokio::test]
#[serial]
async fn git_notes_backend_archive_twice_is_idempotent() {
    let dir = make_temp_git_repo();
    let root = dir.path();
    let backend = GitNotesBackend::with_root(root.to_path_buf());

    let (id, _created) = backend
        .add(note_input("decision", "archive me twice"))
        .await
        .expect("add");

    assert!(backend.archive(id.clone()).await.expect("first archive"));
    assert!(backend.archive(id).await.expect("second archive"));

    let all = backend.list(None, 10, true, None).await.expect("list all");
    assert_eq!(
        all.len(),
        1,
        "two archives of the same entity must still fold to one entry"
    );
    assert_eq!(all[0].status, "archived");
}

// ── concurrent append safety (#185 / ADR-069 D8) ─────────────────────────────

/// The D8 invariant for N concurrent writers: every writer's entry **lands or
/// its writer fails visibly**, and the two sets are disjoint and complete.
///
/// Regression guard for #185, where an unserialized read-modify-write dropped
/// entries with every writer exiting 0. "All must land" is deliberately NOT
/// the invariant: on a slow runner, N queued writers legitimately exceed the
/// lock's wait budget and the back of the queue fails with the D8 error. That
/// is the designed outcome; silence is the bug.
///
/// Asserts, given landed = ids read back and failed = ids whose append
/// returned `Err`:
/// - silent loss is zero: every id is in landed or in failed,
/// - no false failure: no id is in both (an `Err` writer must not have written),
/// - landed is non-empty: the first holder takes a free lock, so a wedged
///   lock cannot vacuously pass as eight visible failures.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[serial]
async fn append_to_git_notes_concurrent_writers_land_or_fail_visibly() {
    const WRITERS: i64 = 8;

    let dir = make_temp_git_repo();
    let root = dir.path().to_path_buf();

    let mut tasks = Vec::new();
    for id in 1..=WRITERS {
        let root = root.clone();
        tasks.push(tokio::spawn(async move {
            let record = make_note_record(id, &format!("concurrent decision {id}"));
            let started = std::time::Instant::now();
            let result = append_to_git_notes(Some(&root), &record).await;
            (id, result, started.elapsed())
        }));
    }

    let mut failed: Vec<i64> = Vec::new();
    let mut failure_msgs = Vec::new();
    for task in tasks {
        let (id, result, took) = task.await.expect("writer task should not panic");
        if let Err(e) = result {
            failed.push(id);
            failure_msgs.push(format!("writer {id} failed after {took:?}: {e:#}"));
        }
    }

    let blob = note_on_head(&root).unwrap_or_default();
    let landed: Vec<i64> = blob
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .filter_map(|v| v["id"].as_i64())
        .collect();

    let silent: Vec<i64> = (1..=WRITERS)
        .filter(|id| !landed.contains(id) && !failed.contains(id))
        .collect();
    let both: Vec<i64> = landed
        .iter()
        .copied()
        .filter(|id| failed.contains(id))
        .collect();

    assert!(
        silent.is_empty(),
        "SILENT LOSS of {} of {WRITERS} entries (ids {silent:?}): neither landed \
         {landed:?} nor failed visibly {failed:?}\n\
         failures: {failure_msgs:#?}\nfinal note body:\n{blob}",
        silent.len(),
    );
    assert!(
        both.is_empty(),
        "writers {both:?} reported failure but their entries landed; an `Err` \
         append must not write\nfailures: {failure_msgs:#?}"
    );
    assert!(
        !landed.is_empty(),
        "no writer landed at all; the first holder takes a free lock, so this \
         means the lock or the write path is wedged\nfailures: {failure_msgs:#?}"
    );
}

/// The same D8 invariant across **separate worktrees**: land or fail visibly,
/// disjoint and complete, at least one landing.
///
/// Worktrees share one `refs/notes/spelunk` (it resolves through the git
/// common dir to the main repo's copy), so they are real contenders on one
/// note body. This pins the lock to the common dir: a lock keyed on the
/// per-worktree git dir would still pass the single-repo test above while
/// serializing nothing here.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[serial]
async fn append_to_git_notes_concurrent_worktrees_land_or_fail_visibly() {
    const PER_TREE: i64 = 4;

    let dir = make_temp_git_repo();
    let main_root = dir.path().to_path_buf();

    // Second worktree on a new branch at the same commit, so both HEADs
    // resolve to one note object.
    let wt_parent = tempfile::TempDir::new().expect("tempdir");
    let wt_root = wt_parent.path().join("wt");
    let out = std::process::Command::new("git")
        .args(["worktree", "add", "-b", "wt", wt_root.to_str().unwrap()])
        .current_dir(&main_root)
        .output()
        .expect("git worktree add");
    assert!(
        out.status.success(),
        "worktree add: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Precondition: the two trees really do share one notes ref.
    let head_of = |root: &std::path::Path| -> String {
        let o = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .expect("rev-parse");
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    assert_eq!(
        head_of(&main_root),
        head_of(&wt_root),
        "both worktrees must sit on the same commit"
    );

    let mut tasks = Vec::new();
    for (tree, base) in [(&main_root, 0), (&wt_root, PER_TREE)] {
        for n in 1..=PER_TREE {
            let root = tree.clone();
            let id = base + n;
            tasks.push(tokio::spawn(async move {
                let record = make_note_record(id, &format!("worktree decision {id}"));
                let started = std::time::Instant::now();
                let result = append_to_git_notes(Some(&root), &record).await;
                (id, result, started.elapsed())
            }));
        }
    }

    let mut failed: Vec<i64> = Vec::new();
    let mut failure_msgs = Vec::new();
    for task in tasks {
        let (id, result, took) = task.await.expect("writer task should not panic");
        if let Err(e) = result {
            failed.push(id);
            failure_msgs.push(format!("writer {id} failed after {took:?}: {e:#}"));
        }
    }

    let total = PER_TREE * 2;
    let blob = note_on_head(&main_root).unwrap_or_default();
    let landed: Vec<i64> = blob
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .filter_map(|v| v["id"].as_i64())
        .collect();

    let silent: Vec<i64> = (1..=total)
        .filter(|id| !landed.contains(id) && !failed.contains(id))
        .collect();
    let both: Vec<i64> = landed
        .iter()
        .copied()
        .filter(|id| failed.contains(id))
        .collect();

    assert!(
        silent.is_empty(),
        "SILENT LOSS of {} of {total} cross-worktree entries (ids {silent:?}): \
         neither landed {landed:?} nor failed visibly {failed:?}\n\
         failures: {failure_msgs:#?}\nfinal note body:\n{blob}",
        silent.len(),
    );
    assert!(
        both.is_empty(),
        "writers {both:?} reported failure but their entries landed; an `Err` \
         append must not write\nfailures: {failure_msgs:#?}"
    );
    assert!(
        !landed.is_empty(),
        "no writer landed at all; the first holder takes a free lock, so this \
         means the lock or the write path is wedged\nfailures: {failure_msgs:#?}"
    );
}

// ── the lock itself: contract and degraded paths (ADR-069 D6) ────────────────

use spelunk_core::storage::{LOCK_WAIT_BUDGET, LockAttempt, lock_notes};

/// The path production locks: `<git-common-dir>/spelunk-notes.lock`. Mirrors
/// `notes_lock_path`, including resolving git's relative answer (a plain repo
/// answers `.git`) against the repo root and canonicalizing the result.
fn notes_lock_path(root: &std::path::Path) -> std::path::PathBuf {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            root.to_str().unwrap(),
            "rev-parse",
            "--git-common-dir",
        ])
        .output()
        .expect("git rev-parse --git-common-dir");
    assert!(out.status.success(), "rev-parse --git-common-dir");

    let raw = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let raw = std::path::Path::new(&raw);
    let common_dir = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    let common_dir = std::fs::canonicalize(&common_dir).unwrap_or(common_dir);
    common_dir.join("spelunk-notes.lock")
}

/// Open the lock file the way production does. A lock taken through this handle
/// really contends with `lock_notes`: std leaves same-handle (or cloned-handle)
/// re-locking unspecified, but a distinct `open` is a distinct handle on every
/// platform, so the conflict is well-defined.
fn open_lock_file(path: &std::path::Path) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .expect("open notes lock file")
}

/// The `lock_notes` contract D5 and D8 build on: `Acquired` when free,
/// `Contended` when held past the budget, and the guard releases on drop.
///
/// `Contended` must be reachable (never an indefinite block) and a dropped
/// guard must not keep the lock held.
#[tokio::test]
#[serial]
async fn lock_notes_grants_when_free_reports_contention_and_frees_on_drop() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let held = lock_notes(Some(root)).await.expect("resolve lock path");
    assert!(
        matches!(held, LockAttempt::Acquired(_)),
        "an uncontended lock must be granted, got {held:?}"
    );

    let started = std::time::Instant::now();
    let contended = lock_notes(Some(root)).await.expect("resolve lock path");
    let waited = started.elapsed();
    assert!(
        matches!(contended, LockAttempt::Contended { .. }),
        "a lock held elsewhere must report Contended, not block indefinitely \
         or degrade; got {contended:?}"
    );
    // Negative control: a `Contended` returned immediately would mean the lock
    // was never taken and the contention above was never actually exercised.
    assert!(
        waited >= LOCK_WAIT_BUDGET,
        "the contended caller must wait out the {LOCK_WAIT_BUDGET:?} budget before giving up; \
         gave up after {waited:?}, so it never contended"
    );

    drop(held);

    assert!(
        matches!(
            lock_notes(Some(root)).await.expect("resolve lock path"),
            LockAttempt::Acquired(_)
        ),
        "the guard must release the lock on drop"
    );
}

/// Contenders in **separate worktrees** must converge on one lock file.
///
/// A linked worktree's `--git-common-dir` answer and the main worktree's
/// relative answer must resolve to the same identity; if each computed its
/// own, the cross-worktree writers the lock exists for would exclude nothing.
#[tokio::test]
#[serial]
async fn lock_notes_contends_across_worktrees() {
    let dir = make_temp_git_repo();
    let main_root = dir.path();

    let wt_parent = tempfile::TempDir::new().expect("tempdir");
    let wt_root = wt_parent.path().join("wt");
    let out = std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            "wt-lock",
            wt_root.to_str().unwrap(),
        ])
        .current_dir(main_root)
        .output()
        .expect("git worktree add");
    assert!(
        out.status.success(),
        "worktree add: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let held = lock_notes(Some(main_root))
        .await
        .expect("resolve lock path");
    let LockAttempt::Acquired(guard) = held else {
        panic!("main worktree must acquire the free lock, got {held:?}");
    };

    let from_worktree = lock_notes(Some(&wt_root)).await.expect("resolve lock path");
    assert!(
        matches!(from_worktree, LockAttempt::Contended { .. }),
        "a lock held from the main worktree must contend from a linked \
         worktree; anything else means the two resolved different lock files. \
         Held: {:?}, got: {from_worktree:?}",
        guard.path(),
    );
}

/// The lock path itself is one identity: absolute, inside an existing git
/// common dir, and byte-identical whether resolved from the main worktree or a
/// linked one.
///
/// The contention test above proves the two exclude each other while both
/// locks are live; this pins the path property directly, so a resolution
/// change that happens to keep same-process exclusion (say, one spelling
/// absolute and one relative to a shared cwd) still fails.
#[tokio::test]
#[serial]
async fn lock_path_is_one_identity_from_main_and_linked_worktrees() {
    let dir = make_temp_git_repo();
    let main_root = dir.path();

    let wt_parent = tempfile::TempDir::new().expect("tempdir");
    let wt_root = wt_parent.path().join("wt");
    let out = std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            "wt-lock-path",
            wt_root.to_str().unwrap(),
        ])
        .current_dir(main_root)
        .output()
        .expect("git worktree add");
    assert!(
        out.status.success(),
        "worktree add: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let from_main = match lock_notes(Some(main_root))
        .await
        .expect("resolve lock path")
    {
        LockAttempt::Acquired(guard) => guard.path().to_path_buf(),
        other => panic!("main worktree must acquire the free lock, got {other:?}"),
    };
    // Sequential acquisitions: the first guard is dropped, so this re-acquires
    // rather than contends, and only the resolved paths are compared.
    let from_linked = match lock_notes(Some(&wt_root)).await.expect("resolve lock path") {
        LockAttempt::Acquired(guard) => guard.path().to_path_buf(),
        other => panic!("linked worktree must acquire the free lock, got {other:?}"),
    };

    assert!(
        from_main.is_absolute(),
        "the lock path must be absolute, got {}",
        from_main.display()
    );
    assert_eq!(
        from_main, from_linked,
        "both worktrees must resolve one lock file identity"
    );
    assert!(
        from_main.parent().is_some_and(|d| d.is_dir()),
        "the lock must live in an existing directory (the git common dir), \
         got {}",
        from_main.display()
    );
}

/// An unusable lock file degrades to an unlocked write, never an `Err`.
///
/// ADR-069 D8's one kept degradation: where the lock cannot exist at all,
/// failing every write would make spelunk unusable on that filesystem to
/// prevent a race that needs a second concurrent writer to matter.
///
/// A directory at the lock path makes the open fail deterministically on every
/// platform (EISDIR on unix, access-denied on Windows).
#[tokio::test]
#[serial]
async fn append_to_git_notes_proceeds_when_lock_file_is_unusable() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    std::fs::create_dir_all(notes_lock_path(root)).expect("place a directory at the lock path");

    // Negative control: prove the lock really is unusable at the path production
    // resolves, so the write below exercises the degraded path rather than
    // quietly locking a file this test never blocked.
    assert!(
        matches!(
            lock_notes(Some(root)).await.expect("resolve lock path"),
            LockAttempt::Unusable { .. }
        ),
        "setup: an unopenable lock path must report Unusable"
    );

    let record = make_note_record(1, "unusable lock still writes");
    append_to_git_notes(Some(root), &record)
        .await
        .expect("an unusable lock must degrade to an unlocked write, not an error");

    assert!(
        read_raw_note(root).contains("unusable lock still writes"),
        "the entry must still be written when the lock cannot be opened"
    );
}

/// A writer that cannot take a **contended** lock fails and writes nothing
/// (ADR-069 D8). Proceeding unlocked here is the #185 read-modify-write this
/// lock exists to prevent: the holder's entry would be silently erased.
///
/// Deterministic: the lock is held here for the whole call, so the writer is
/// guaranteed to exhaust its budget rather than race for it.
#[tokio::test]
#[serial]
async fn append_to_git_notes_fails_when_the_lock_is_contended() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let held = open_lock_file(&notes_lock_path(root));
    held.lock()
        .expect("hold the notes lock for the whole write");

    let started = std::time::Instant::now();
    let result = append_to_git_notes(
        Some(root),
        &make_note_record(1, "contended lock must not write"),
    )
    .await;
    let waited = started.elapsed();

    drop(held);

    let err = result.expect_err(
        "a writer that cannot take a contended lock must fail, never proceed unlocked (D8)",
    );
    // Negative control: too fast means the writer took the lock uncontended, so
    // the contended path was never exercised.
    assert!(
        waited >= LOCK_WAIT_BUDGET,
        "the writer must have waited out the {LOCK_WAIT_BUDGET:?} budget; \
         returned after {waited:?}, so it never contended"
    );
    // The error must be actionable: name the lock and tell the user what to do.
    let msg = format!("{err:#}");
    assert!(
        msg.contains("notes lock") && msg.contains("Retry"),
        "the error must name the lock and say to retry; got: {msg}"
    );
    assert!(
        note_on_head(root).is_none(),
        "nothing may be written when the lock is contended; the unserialized \
         write is exactly the data loss D8 forbids"
    );
}

/// The `--backend git-notes` writers carry the same D8 contract as the
/// write-through helper: a contended lock fails `add` and `archive`, and
/// nothing is written. Each call site matches on the lock outcome on its own,
/// so one swallowed arm reopens the #185 unlocked write at that site only;
/// this pins both sites, not just the shared helper.
#[tokio::test]
#[serial]
async fn git_notes_backend_add_and_archive_fail_when_the_lock_is_contended() {
    let dir = make_temp_git_repo();
    let root = dir.path();
    let backend = GitNotesBackend::with_root(root.to_path_buf());

    let (id, _) = backend
        .add(note_input("decision", "the sibling entry at stake"))
        .await
        .expect("seed");
    let ref_before = git_stdout_ok(root, &["rev-parse", "refs/notes/spelunk"]);

    let held = open_lock_file(&notes_lock_path(root));
    held.lock().expect("hold the notes lock across both writes");

    let started = std::time::Instant::now();
    let err = backend
        .add(note_input("decision", "must not land"))
        .await
        .expect_err("a contended lock must fail the backend add, never write unlocked (D8)");
    let waited = started.elapsed();
    // Negative control: too fast means the add failed for some unrelated
    // reason instead of waiting out a held lock.
    assert!(
        waited >= LOCK_WAIT_BUDGET,
        "the add must have waited out the {LOCK_WAIT_BUDGET:?} budget; \
         returned after {waited:?}, so it never contended"
    );
    assert!(
        format!("{err:#}").contains("notes lock"),
        "the add error must name the lock; got: {err:#}"
    );

    let err = backend
        .archive(id)
        .await
        .expect_err("a contended lock must fail the backend archive, never write unlocked (D8)");
    assert!(
        format!("{err:#}").contains("notes lock"),
        "the archive error must name the lock; got: {err:#}"
    );

    drop(held);

    assert_eq!(
        git_stdout_ok(root, &["rev-parse", "refs/notes/spelunk"]),
        ref_before,
        "the note ref must be untouched after both contended writes"
    );
}

// ── a failed note read must never be mistaken for "no note yet" ───────────────
//
// The windows-latest losses on main (#185's worst case): a writer whose
// `git notes show` failed transiently treated the note as empty and rewrote it
// as just its own line, erasing every sibling entry while holding the lock.
// Survivor patterns in the CI logs (a contiguous tail of the serialization
// order) are the fingerprint of exactly one such wipe per run.

/// Delete the note's blob object so reads of it fail while writes still work:
/// `git notes show` dies (exit 128), but `git notes add -f` never opens the
/// old blob, so an unguarded read-modify-write would "succeed" and wipe it.
fn corrupt_note_blob(root: &std::path::Path) {
    let listing = git_stdout_ok(root, &["notes", "--ref=spelunk", "list"]);
    let blob_sha = listing
        .split_whitespace()
        .next()
        .expect("a note blob sha")
        .to_string();
    let object = root
        .join(".git")
        .join("objects")
        .join(&blob_sha[..2])
        .join(&blob_sha[2..]);
    // Loose objects are read-only; make the unlink work on every platform.
    let mut perms = std::fs::metadata(&object)
        .expect("stat object")
        .permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&object, perms).expect("make object writable");
    std::fs::remove_file(&object).expect("remove note blob object");

    // Negative controls: the read must now fail, and the ref must still point
    // at the blob, so a write-back would replace a body that still "exists".
    let show = std::process::Command::new("git")
        .args([
            "-C",
            root.to_str().unwrap(),
            "notes",
            "--ref=spelunk",
            "show",
            "HEAD",
        ])
        .output()
        .expect("git notes show");
    assert!(
        !show.status.success() && show.status.code() != Some(1),
        "setup: the note read must fail hard, not report a missing note"
    );
}

/// A writer whose read of the existing note fails must fail too, leaving the
/// note alone. Treating the failure as "no note yet" is how one transient git
/// error erased 6 of 8 entries on Windows CI.
#[tokio::test]
#[serial]
async fn append_to_git_notes_fails_when_the_existing_note_cannot_be_read() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    append_to_git_notes(
        Some(root),
        &make_note_record(1, "the sibling entry at stake"),
    )
    .await
    .expect("seed");
    let ref_before = git_stdout_ok(root, &["rev-parse", "refs/notes/spelunk"]);

    corrupt_note_blob(root);

    let err = append_to_git_notes(Some(root), &make_note_record(2, "must not land"))
        .await
        .expect_err("a failed read of the existing note must fail the append, not wipe the note");
    assert!(
        format!("{err:#}").contains("not overwriting"),
        "the error must say the note was left alone; got: {err:#}"
    );
    assert_eq!(
        git_stdout_ok(root, &["rev-parse", "refs/notes/spelunk"]),
        ref_before,
        "the note ref must be untouched after a failed read"
    );
}

/// The `--backend git-notes` write paths carry the same read-modify-write and
/// need the same guarantee, for both `add` and `archive`.
#[tokio::test]
#[serial]
async fn git_notes_backend_add_and_archive_fail_when_the_note_cannot_be_read() {
    let dir = make_temp_git_repo();
    let root = dir.path();
    let backend = GitNotesBackend::with_root(root.to_path_buf());

    let (id, _) = backend
        .add(note_input("decision", "the sibling entry at stake"))
        .await
        .expect("seed");
    let ref_before = git_stdout_ok(root, &["rev-parse", "refs/notes/spelunk"]);

    corrupt_note_blob(root);

    backend
        .add(note_input("decision", "must not land"))
        .await
        .expect_err("a failed read must fail the add, not wipe the note");
    backend
        .archive(id)
        .await
        .expect_err("a failed read must fail the archive, not report the entry missing");
    assert_eq!(
        git_stdout_ok(root, &["rev-parse", "refs/notes/spelunk"]),
        ref_before,
        "the note ref must be untouched after failed writes"
    );
}

/// The in-lock read retry paces its attempts: a persistently failing read
/// waits out the inter-attempt backoff before surfacing, never burns its
/// attempts in one instant.
///
/// Four attempts with a linearly growing 50ms base backoff sleep 300ms in
/// total, so the 250ms floor holds deterministically on the unmutated code,
/// while a backoff zeroed out (or a loop that no longer retries) returns in
/// the time of a few git spawns. Pins the retry pacing that keeps the
/// windows-latest flake absorbable; a deliberate retuning of the constants
/// updates this floor with it.
#[tokio::test]
#[serial]
async fn append_read_retry_paces_its_attempts_before_surfacing_the_failure() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    append_to_git_notes(
        Some(root),
        &make_note_record(1, "the sibling entry at stake"),
    )
    .await
    .expect("seed");
    corrupt_note_blob(root);

    let started = std::time::Instant::now();
    append_to_git_notes(Some(root), &make_note_record(2, "must not land"))
        .await
        .expect_err("a persistent read failure must still surface after the retries");
    let took = started.elapsed();

    assert!(
        took >= std::time::Duration::from_millis(250),
        "a persistently failing read must wait out ~300ms of backoff across \
         its retries; returned after {took:?}, so it never retried or never \
         slept between attempts"
    );
}

// ── fault injection: a scripted `git` on PATH ─────────────────────────────────
//
// The corruption tests above can only make a read fail *persistently*; the
// windows-latest loss was a *transient* failure, gone by the next attempt.
// Only unix, because the shim is a shell script: the same retry code runs on
// Windows, where the persistent-failure tests above still pin classification
// and pacing.

/// A `git` shim prepended to PATH. Pass-through except for one scripted
/// behaviour scoped to a single repo (matched on the child's physical cwd),
/// so concurrent git use elsewhere is untouched. Restores PATH on drop.
#[cfg(unix)]
mod git_shim {
    use std::path::{Path, PathBuf};

    pub struct ShimGuard {
        original_path: std::ffi::OsString,
        counter: PathBuf,
        _dir: tempfile::TempDir,
    }

    impl Drop for ShimGuard {
        fn drop(&mut self) {
            // SAFETY: every test in this binary is #[serial] (and nextest runs
            // one process per test), so nothing reads the environment
            // concurrently. Same argument as `common::isolate_git_config`.
            unsafe { std::env::set_var("PATH", &self.original_path) };
        }
    }

    impl ShimGuard {
        /// How many spelunk note reads the shim has intercepted so far.
        pub fn note_reads(&self) -> u32 {
            std::fs::read_to_string(&self.counter)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0)
        }
    }

    /// The real git, resolved before the shim shadows the name.
    fn real_git() -> String {
        let out = std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .expect("resolve the real git");
        assert!(out.status.success(), "git must be on PATH");
        String::from_utf8(out.stdout)
            .expect("utf8 git path")
            .trim()
            .to_string()
    }

    fn install(dir: tempfile::TempDir, script: String, counter: PathBuf) -> ShimGuard {
        use std::os::unix::fs::PermissionsExt;
        let shim = dir.path().join("git");
        std::fs::write(&shim, script).expect("write shim");
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
            .expect("chmod shim");

        let original_path = std::env::var_os("PATH").expect("PATH is set");
        let mut parts = vec![dir.path().to_path_buf()];
        parts.extend(std::env::split_paths(&original_path));
        let new_path = std::env::join_paths(parts).expect("join PATH");
        // SAFETY: see Drop.
        unsafe { std::env::set_var("PATH", &new_path) };

        ShimGuard {
            original_path,
            counter,
            _dir: dir,
        }
    }

    /// Count every `notes --ref=spelunk show` in `repo`, failing the first
    /// `fail_n` of them with exit 128 (the transient-infrastructure shape the
    /// windows-latest losses had). Everything else passes through.
    pub fn failing_note_reads(repo: &Path, fail_n: u32) -> ShimGuard {
        let repo = std::fs::canonicalize(repo).expect("canonical repo path");
        let real = real_git();
        let dir = tempfile::TempDir::new().expect("shim dir");
        let counter = dir.path().join("note-reads");
        let script = format!(
            "#!/bin/sh\n\
             case \" $* \" in\n\
               *\" notes --ref=spelunk show \"*)\n\
                 if [ \"$(pwd -P)\" = \"{repo}\" ]; then\n\
                   n=$(cat \"{counter}\" 2>/dev/null || echo 0)\n\
                   n=$((n+1))\n\
                   printf '%s\\n' \"$n\" > \"{counter}\"\n\
                   if [ \"$n\" -le {fail_n} ]; then\n\
                     echo 'shim: simulated transient note read failure' >&2\n\
                     exit 128\n\
                   fi\n\
                 fi\n\
                 ;;\n\
             esac\n\
             exec \"{real}\" \"$@\"\n",
            repo = repo.display(),
            counter = counter.display(),
        );
        install(dir, script, counter)
    }

    /// Emulate git < 2.31 for `repo`: `rev-parse --path-format=absolute
    /// --git-common-dir` echoes the unknown flag back and exits 0, exactly the
    /// output shape old git produces. Everything else passes through.
    pub fn old_git_echoing_path_format(repo: &Path) -> ShimGuard {
        let repo = std::fs::canonicalize(repo).expect("canonical repo path");
        let real = real_git();
        let dir = tempfile::TempDir::new().expect("shim dir");
        let counter = dir.path().join("note-reads");
        let script = format!(
            "#!/bin/sh\n\
             if [ \"$*\" = \"rev-parse --path-format=absolute --git-common-dir\" ] \
                && [ \"$(pwd -P)\" = \"{repo}\" ]; then\n\
               printf '%s\\n%s\\n' '--path-format=absolute' '.git'\n\
               exit 0\n\
             fi\n\
             exec \"{real}\" \"$@\"\n",
            repo = repo.display(),
        );
        install(dir, script, counter)
    }
}

/// A transiently failing note read is absorbed by the in-lock retry: the
/// append succeeds on a later attempt and every sibling entry survives. This
/// is the windows-latest #185 shape itself: the same read succeeded for every
/// sibling writer moments apart.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn append_read_retry_absorbs_a_transient_note_read_failure() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    append_to_git_notes(
        Some(root),
        &make_note_record(1, "the sibling entry at stake"),
    )
    .await
    .expect("seed");

    // The first two reads fail, the third succeeds; well inside the four
    // attempts the retry allows.
    let shim = git_shim::failing_note_reads(root, 2);

    append_to_git_notes(
        Some(root),
        &make_note_record(2, "lands on the third attempt"),
    )
    .await
    .expect("a transient read failure must be absorbed by the in-lock retry, not surfaced");
    let reads = shim.note_reads();
    drop(shim);

    assert!(
        reads >= 3,
        "the append must have read again past the two failed attempts; \
         saw {reads} read(s)"
    );
    let blob = note_on_head(root).expect("note on HEAD");
    assert!(
        blob.contains("the sibling entry at stake") && blob.contains("lands on the third attempt"),
        "both entries must survive the transient failure; note body:\n{blob}"
    );
}

/// "No note yet" is a definitive answer and is not retried: the append's one
/// read exits 1 and the write proceeds immediately. Retrying it would bill
/// every first write on a commit the full retry backoff for nothing.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn a_missing_note_is_answered_in_one_read_without_retrying() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    // Count only (fail_n = 0): HEAD has no note, so the read exits 1.
    let shim = git_shim::failing_note_reads(root, 0);

    append_to_git_notes(
        Some(root),
        &make_note_record(1, "first entry on a bare HEAD"),
    )
    .await
    .expect("append");
    let reads = shim.note_reads();
    drop(shim);

    assert_eq!(
        reads, 1,
        "a missing note must be read exactly once, never retried"
    );
    assert!(
        note_on_head(root)
            .expect("note on HEAD")
            .contains("first entry on a bare HEAD"),
        "the first entry must land"
    );
}

/// Under git < 2.31 (the flag echoed back, exit 0) the lock still lands in
/// the real common dir, canonicalized, and no path spelled after the echoed
/// flag is ever used. The unit tests pin `parse_absolute_dir` itself; this
/// pins that `notes_lock_path` actually routes through that validation and
/// that its fallback converges on the same canonical identity.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn old_git_echoing_the_flag_still_locks_the_real_common_dir() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let shim = git_shim::old_git_echoing_path_format(root);
    let attempt = lock_notes(Some(root)).await.expect("resolve lock path");
    drop(shim);

    let LockAttempt::Acquired(guard) = attempt else {
        panic!("the fallback must still acquire a free lock under old git, got {attempt:?}");
    };
    let expected = std::fs::canonicalize(root.join(".git"))
        .expect("the common dir exists")
        .join("spelunk-notes.lock");
    assert_eq!(
        guard.path(),
        expected.as_path(),
        "old git's echoed flag must fall back to the real common dir, in the \
         one canonical spelling every contender computes"
    );
    assert!(
        !root.join("--path-format=absolute").exists(),
        "no path spelled after the echoed flag may be created"
    );
}

// ── the `--backend git-notes` path (append_record / archive_record) ───────────

/// `GitNotesBackend::add` → `append_record` carries the same read-modify-write
/// as the write-through helper, so it gets the same D8 invariant as the
/// free-function guards: every add lands or fails visibly, disjoint and
/// complete, and at least one lands. "All succeed" is deliberately not
/// asserted: on a slow runner N queued writers legitimately exceed the lock's
/// wait budget and the back of the queue fails with the D8 error.
///
/// Asserted on titles, not ids: `add` mints its id from `now_millis()` *before*
/// taking the lock, so adds landing in one millisecond can share an id. Entry
/// survival is what the lock guarantees.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[serial]
async fn git_notes_backend_concurrent_adds_land_or_fail_visibly() {
    const WRITERS: usize = 8;

    let dir = make_temp_git_repo();
    let root = dir.path().to_path_buf();

    let mut tasks = Vec::new();
    for n in 1..=WRITERS {
        let root = root.clone();
        tasks.push(tokio::spawn(async move {
            let result = GitNotesBackend::with_root(root)
                .add(note_input("decision", &format!("backend writer {n}")))
                .await;
            (n, result)
        }));
    }

    let mut failed: Vec<usize> = Vec::new();
    let mut failure_msgs = Vec::new();
    for task in tasks {
        let (n, result) = task.await.expect("writer task should not panic");
        if let Err(e) = result {
            failed.push(n);
            failure_msgs.push(format!("backend writer {n} failed: {e:#}"));
        }
    }

    let notes = GitNotesBackend::with_root(root)
        .list(Some("decision"), 100, false, None)
        .await
        .expect("list");
    let titles: std::collections::HashSet<String> = notes.into_iter().map(|n| n.title).collect();
    let landed: Vec<usize> = (1..=WRITERS)
        .filter(|n| titles.contains(&format!("backend writer {n}")))
        .collect();

    let silent: Vec<usize> = (1..=WRITERS)
        .filter(|n| !landed.contains(n) && !failed.contains(n))
        .collect();
    let both: Vec<usize> = landed
        .iter()
        .copied()
        .filter(|n| failed.contains(n))
        .collect();

    assert!(
        silent.is_empty(),
        "SILENT LOSS of {} of {WRITERS} backend adds ({silent:?}): neither landed \
         {landed:?} nor failed visibly {failed:?}\nfailures: {failure_msgs:#?}",
        silent.len(),
    );
    assert!(
        both.is_empty(),
        "backend adds {both:?} reported failure but their entries landed; an \
         `Err` add must not write\nfailures: {failure_msgs:#?}"
    );
    assert!(
        !landed.is_empty(),
        "no backend add landed at all; the first holder takes a free lock, so \
         this means the lock or the write path is wedged\nfailures: {failure_msgs:#?}"
    );
}

/// `GitNotesBackend::archive` → `archive_record` is the same read-modify-write
/// in reverse, with the same D8 invariant: each archive of a distinct seeded
/// entry either lands (the entry ends archived) or fails visibly (the entry
/// stays active), the two sets are exactly complementary, and at least one
/// lands. Unserialized, each writer's write-back would revive the entries its
/// peers had just archived, silently.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[serial]
async fn git_notes_backend_concurrent_archives_land_or_fail_visibly() {
    const ENTRIES: usize = 6;

    let dir = make_temp_git_repo();
    let root = dir.path().to_path_buf();
    let backend = GitNotesBackend::with_root(root.clone());

    // Sequential adds: each is several git subprocesses, so the `now_millis()`
    // ids are distinct — `archive(id)` targets the first id match, so a
    // collision here would archive the wrong record.
    let mut ids = Vec::new();
    for n in 1..=ENTRIES {
        let (id, _) = backend
            .add(note_input("decision", &format!("archive target {n}")))
            .await
            .expect("add");
        ids.push(id);
    }
    let distinct: std::collections::HashSet<NoteId> = ids.iter().cloned().collect();
    assert_eq!(distinct.len(), ENTRIES, "setup: the ids must be distinct");

    let mut tasks = Vec::new();
    for id in ids {
        let root = root.clone();
        tasks.push(tokio::spawn(async move {
            let result = GitNotesBackend::with_root(root).archive(id.clone()).await;
            (id, result)
        }));
    }

    let mut failed: std::collections::HashSet<NoteId> = std::collections::HashSet::new();
    let mut failure_msgs = Vec::new();
    let mut not_found: Vec<NoteId> = Vec::new();
    for task in tasks {
        let (id, result) = task.await.expect("archive task should not panic");
        match result {
            Ok(true) => {}
            Ok(false) => not_found.push(id),
            Err(e) => {
                failure_msgs.push(format!("archive of {id} failed: {e:#}"));
                failed.insert(id);
            }
        }
    }
    // Every id was seeded, so "entry not found" is neither a landing nor a
    // visible failure: it is a silent anomaly.
    assert!(
        not_found.is_empty(),
        "archives {not_found:?} reported their entry missing; every id exists, \
         so a miss is a silently lost archive"
    );

    let all = backend
        .list(Some("decision"), 100, true, None)
        .await
        .expect("list all");
    assert_eq!(
        all.len(),
        ENTRIES,
        "archiving must rewrite entries in place, never drop them"
    );

    // Exactly the visibly-failed archives may remain active: an active entry
    // whose archive reported success is a silently revived/lost archive, and
    // an archived entry whose archive reported failure wrote after erroring.
    let active: std::collections::HashSet<NoteId> = backend
        .list(Some("decision"), 100, false, None)
        .await
        .expect("list active")
        .into_iter()
        .map(|n| n.id)
        .collect();
    assert_eq!(
        active, failed,
        "the active set must be exactly the visibly-failed set\nfailures: {failure_msgs:#?}"
    );
    assert!(
        failed.len() < ENTRIES,
        "no archive landed at all; the first holder takes a free lock, so this \
         means the lock or the write path is wedged\nfailures: {failure_msgs:#?}"
    );
}

/// `add` and `archive` take the notes lock exactly once; nothing beneath them
/// takes it again. A nested acquisition would not hang (the wait is bounded),
/// it would silently burn a full budget and then degrade to an unlocked write.
/// An *uncontended* op is a handful of git subprocesses (~30ms), so reaching the
/// budget at all means it waited on a lock it already holds.
#[tokio::test]
#[serial]
async fn git_notes_backend_uncontended_add_and_archive_never_reach_the_wait_budget() {
    let dir = make_temp_git_repo();
    let backend = GitNotesBackend::with_root(dir.path().to_path_buf());

    let started = std::time::Instant::now();
    let (id, _) = backend
        .add(note_input("decision", "no nested acquisition"))
        .await
        .expect("add");
    let add_took = started.elapsed();

    let started = std::time::Instant::now();
    assert!(
        backend.archive(id).await.expect("archive"),
        "entry archived"
    );
    let archive_took = started.elapsed();

    for (op, took) in [("add", add_took), ("archive", archive_took)] {
        assert!(
            took < LOCK_WAIT_BUDGET,
            "uncontended {op} took {took:?}, reaching the {LOCK_WAIT_BUDGET:?} lock budget: \
             it waited on a lock it already holds (a nested acquisition)"
        );
    }
}

// ── survival across git history rewrites ─────────────────────────────────────
//
// git copies a note onto a rewritten commit only when `notes.rewriteRef` names
// the ref, and it has no built-in default. Pre-`init` git notes is the sole
// store, so an unconfigured repo lost the only copy on every amend/rebase.
//
// Known gap: `merge --squash` and cherry-pick onto a divergent base do not
// carry notes even with `notes.rewriteRef` set. git honours it for `amend` and
// `rebase` only.

/// Run `git args` in `root`, asserting success.
fn git_ok(root: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .expect("git command");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Trimmed stdout of `git args` in `root`, asserting success.
fn git_stdout_ok(root: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .expect("git command");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Run a rebase with scripted editors. git runs an editor through a shell when
/// the command holds shell metacharacters, so a value ending in `>` redirects
/// into the file git appends as the trailing argument. Portable across the CI
/// matrix, unlike `sed -i` (BSD and GNU disagree on its argument).
fn git_rebase_scripted(root: &std::path::Path, args: &[&str], seq_editor: &str, editor: &str) {
    let out = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_SEQUENCE_EDITOR", seq_editor)
        .env("GIT_EDITOR", editor)
        .output()
        .expect("git rebase");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The spelunk note body on HEAD, or `None` when HEAD carries no note.
fn note_on_head(root: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .current_dir(root)
        .args(["notes", "--ref=spelunk", "show", "HEAD"])
        .output()
        .expect("git notes show");
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Commit a new `file` in `root` with subject `msg`.
fn commit_file(root: &std::path::Path, file: &str, msg: &str) {
    std::fs::write(root.join(file), "x").expect("write");
    git_ok(root, &["add", "."]);
    git_ok(root, &["commit", "--no-gpg-sign", "-m", msg]);
}

/// A repo whose rewrites are deterministic regardless of ambient git config.
fn rewrite_test_repo() -> tempfile::TempDir {
    let dir = make_temp_git_repo();
    git_ok(dir.path(), &["config", "commit.gpgsign", "false"]);
    dir
}

const PRECIOUS: &str = "precious decision";

/// `git commit --amend` must carry the entry onto the rewritten commit.
#[tokio::test]
#[serial]
async fn note_survives_git_commit_amend() {
    let dir = rewrite_test_repo();
    let root = dir.path();

    append_to_git_notes(Some(root), &make_note_record(1, PRECIOUS))
        .await
        .expect("append should succeed");

    let before = git_stdout_ok(root, &["rev-parse", "HEAD"]);
    // Amend the subject, not just `--no-edit`: an amend that changes nothing
    // re-creates a byte-identical commit object within the same second, so the
    // sha is unchanged and the note is never actually asked to move.
    git_ok(
        root,
        &[
            "commit",
            "--amend",
            "--no-gpg-sign",
            "-m",
            "amended subject",
        ],
    );
    assert_ne!(
        before,
        git_stdout_ok(root, &["rev-parse", "HEAD"]),
        "the amend must actually have rewritten the commit"
    );

    let body = note_on_head(root).expect("entry must survive `git commit --amend`");
    assert!(
        body.contains(PRECIOUS),
        "amended HEAD must carry the entry; got: {body:?}"
    );
}

/// A plain `git rebase` must carry the entry onto the replayed commit.
#[tokio::test]
#[serial]
async fn note_survives_git_rebase() {
    let dir = rewrite_test_repo();
    let root = dir.path();

    git_ok(root, &["checkout", "-q", "-b", "feat"]);
    commit_file(root, "feat.txt", "feat work");
    append_to_git_notes(Some(root), &make_note_record(1, PRECIOUS))
        .await
        .expect("append should succeed");

    // Move main ahead so the rebase really replays onto new shas.
    git_ok(root, &["checkout", "-q", "main"]);
    commit_file(root, "main.txt", "main moves");
    git_ok(root, &["checkout", "-q", "feat"]);
    git_ok(root, &["rebase", "main"]);

    let body = note_on_head(root).expect("entry must survive `git rebase`");
    assert!(
        body.contains(PRECIOUS),
        "rebased HEAD must carry the entry; got: {body:?}"
    );
}

/// `git rebase -i` reword must carry the entry.
#[tokio::test]
#[serial]
async fn note_survives_rebase_interactive_reword() {
    let dir = rewrite_test_repo();
    let root = dir.path();

    commit_file(root, "work.txt", "target commit");
    append_to_git_notes(Some(root), &make_note_record(1, PRECIOUS))
        .await
        .expect("append should succeed");

    let sha = git_stdout_ok(root, &["rev-parse", "--short", "HEAD"]);
    git_rebase_scripted(
        root,
        &["rebase", "-i", "HEAD~1"],
        &format!("printf 'reword {sha}\\n' >"),
        "printf 'reworded subject\\n' >",
    );

    assert_eq!(
        git_stdout_ok(root, &["log", "-1", "--format=%s"]),
        "reworded subject",
        "the reword must actually have rewritten the commit"
    );
    let body = note_on_head(root).expect("entry must survive a `rebase -i` reword");
    assert!(
        body.contains(PRECIOUS),
        "reworded HEAD must carry the entry; got: {body:?}"
    );
}

/// `git rebase --autosquash` (fixup) must carry the entry onto the squashed commit.
#[tokio::test]
#[serial]
async fn note_survives_rebase_autosquash_fixup() {
    let dir = rewrite_test_repo();
    let root = dir.path();

    commit_file(root, "work.txt", "target commit");
    append_to_git_notes(Some(root), &make_note_record(1, PRECIOUS))
        .await
        .expect("append should succeed");

    let noted = git_stdout_ok(root, &["rev-parse", "HEAD"]);
    std::fs::write(root.join("more.txt"), "x").expect("write");
    git_ok(root, &["add", "."]);
    git_ok(root, &["commit", "--no-gpg-sign", "--fixup", &noted]);

    // `-i` plus a no-op sequence editor accepts the autosquash-arranged todo.
    // A non-interactive `--autosquash` would need git >= 2.38.
    git_rebase_scripted(
        root,
        &["rebase", "-i", "--autosquash", "HEAD~2"],
        "exit 0 #",
        "exit 0 #",
    );

    assert_eq!(
        git_stdout_ok(root, &["log", "--oneline"]).lines().count(),
        2,
        "the fixup must actually have been squashed away"
    );
    let body = note_on_head(root).expect("entry must survive an autosquash fixup");
    assert!(
        body.contains(PRECIOUS),
        "squashed HEAD must carry the entry; got: {body:?}"
    );
}

// ── notes.rewriteRef carry config ────────────────────────────────────────────

use spelunk_core::storage::{RewriteRefStatus, ensure_notes_rewrite_ref};

/// Every configured `notes.rewriteRef` value, in config order.
fn rewrite_ref_values(root: &std::path::Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .current_dir(root)
        .args(["config", "--get-all", "notes.rewriteRef"])
        .output()
        .expect("git config");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .collect()
}

/// Re-running must not stack duplicate config lines.
#[tokio::test]
#[serial]
async fn ensure_notes_rewrite_ref_is_idempotent() {
    let dir = rewrite_test_repo();
    let root = dir.path();

    assert_eq!(
        ensure_notes_rewrite_ref(Some(root)).await,
        RewriteRefStatus::Configured,
        "first call must set it"
    );
    assert_eq!(
        ensure_notes_rewrite_ref(Some(root)).await,
        RewriteRefStatus::AlreadyCovered,
        "second call must detect its own value and stay quiet"
    );
    assert_eq!(
        rewrite_ref_values(root),
        vec!["refs/notes/spelunk"],
        "re-running must not duplicate the config line"
    );
}

/// A user's own rewriteRef must survive: the value is multi-valued, so `--add`
/// composes instead of clobbering, and both refs keep carrying.
#[tokio::test]
#[serial]
async fn ensure_notes_rewrite_ref_composes_with_a_users_existing_value() {
    let dir = rewrite_test_repo();
    let root = dir.path();
    git_ok(
        root,
        &["config", "--add", "notes.rewriteRef", "refs/notes/commits"],
    );
    git_ok(
        root,
        &["notes", "--ref=commits", "add", "-m", "user note", "HEAD"],
    );

    assert_eq!(
        ensure_notes_rewrite_ref(Some(root)).await,
        RewriteRefStatus::Configured
    );
    assert_eq!(
        rewrite_ref_values(root),
        vec!["refs/notes/commits", "refs/notes/spelunk"],
        "the user's value must be kept alongside ours"
    );

    append_to_git_notes(Some(root), &make_note_record(1, PRECIOUS))
        .await
        .expect("append should succeed");
    git_ok(
        root,
        &["commit", "--amend", "--no-gpg-sign", "-m", "amended"],
    );

    let user_note = git_stdout_ok(root, &["notes", "--ref=commits", "show", "HEAD"]);
    assert_eq!(user_note, "user note", "the user's note must still carry");
    assert!(
        note_on_head(root).is_some_and(|b| b.contains(PRECIOUS)),
        "our note must carry too"
    );
}

// Restores `GIT_CONFIG_GLOBAL` to `previous` on drop, including during a
// panic unwind. A plain "set, run, restore-before-assert" sequence only
// protects against a panic in the assertions: it still leaks the override
// into every later test in this process if the guarded call itself panics
// or the run is interrupted between the set and the restore.
struct RestoreGitConfigGlobal(std::ffi::OsString);

impl RestoreGitConfigGlobal {
    fn to(previous: impl Into<std::ffi::OsString>) -> Self {
        Self(previous.into())
    }
}

impl Drop for RestoreGitConfigGlobal {
    fn drop(&mut self) {
        // SAFETY: every caller of this guard is #[serial] (and nextest
        // gives each test its own process), so nothing reads the
        // environment concurrently while this runs.
        unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", &self.0) };
    }
}

/// The read is deliberately unscoped: a value the user set in *global* scope is
/// theirs and must be honoured, so we add nothing on top of it. Pins the intent
/// that `isolate_git_config` would otherwise hide from every test here.
#[tokio::test]
#[serial]
async fn ensure_notes_rewrite_ref_honours_a_users_global_value() {
    let dir = rewrite_test_repo();
    let root = dir.path();

    let home = tempfile::TempDir::new().expect("tempdir");
    let global = home.path().join("gitconfig");
    std::fs::write(&global, "[notes]\n\trewriteRef = refs/notes/spelunk\n").expect("write");

    // SAFETY: `#[serial]` keeps this the only running test under `cargo test`,
    // and nextest gives each test its own process.
    unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", &global) };
    let _restore = RestoreGitConfigGlobal::to("/dev/null");
    let status = ensure_notes_rewrite_ref(Some(root)).await;
    // The guard above restores on drop, unconditionally: it already ran (or
    // will run at end of scope) whether or not the call above panicked.
    drop(_restore);

    assert_eq!(
        status,
        RewriteRefStatus::AlreadyCovered,
        "a global value is the user's own and must be read"
    );
    assert!(
        rewrite_ref_values(root).is_empty(),
        "honouring the global value means writing no local one"
    );
}

// Proves `RestoreGitConfigGlobal`'s `Drop` runs on unwind, not just on the
// happy path: forces a panic between the guard's construction and where the
// old set/restore code used to sit, then confirms the value is back
// afterward. Backs the panic-safety fix for
// `ensure_notes_rewrite_ref_honours_a_users_global_value` above.
#[test]
#[serial]
fn restore_git_config_global_guard_restores_after_a_panic() {
    common::isolate_git_config();
    unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", "sentinel-before-panic") };

    let result = std::panic::catch_unwind(|| {
        let _restore = RestoreGitConfigGlobal::to("/dev/null");
        panic!("simulated failure inside the guarded scope");
    });

    assert!(result.is_err(), "the closure must have actually panicked");
    assert_eq!(
        std::env::var("GIT_CONFIG_GLOBAL").as_deref(),
        Ok("/dev/null"),
        "the guard's Drop must run during unwind and restore the isolated value"
    );
}

/// A user glob that already covers our ref is left alone.
///
/// Deferring is only safe if the glob genuinely carries, so this asserts git's
/// behaviour end to end rather than trusting our reading of it: silently
/// skipping the fix against a glob that did not really cover us would orphan
/// the entry, which is the whole bug.
#[tokio::test]
#[serial]
async fn ensure_notes_rewrite_ref_defers_to_a_covering_glob() {
    let dir = rewrite_test_repo();
    let root = dir.path();
    git_ok(
        root,
        &["config", "--add", "notes.rewriteRef", "refs/notes/*"],
    );

    assert_eq!(
        ensure_notes_rewrite_ref(Some(root)).await,
        RewriteRefStatus::AlreadyCovered
    );
    assert_eq!(
        rewrite_ref_values(root),
        vec!["refs/notes/*"],
        "a covering glob needs no addition"
    );

    append_to_git_notes(Some(root), &make_note_record(1, PRECIOUS))
        .await
        .expect("append should succeed");
    git_ok(
        root,
        &["commit", "--amend", "--no-gpg-sign", "-m", "amended"],
    );
    assert!(
        note_on_head(root).is_some_and(|b| b.contains(PRECIOUS)),
        "the glob we deferred to must actually carry the entry"
    );
}

/// A glob reaching outside `refs/notes/` does NOT cover us: git refuses to
/// rewrite notes there ("Refusing to rewrite notes in refs/*"), so treating it
/// as coverage would silently skip the fix and lose the entry.
#[tokio::test]
#[serial]
async fn ensure_notes_rewrite_ref_ignores_a_glob_outside_the_notes_namespace() {
    let dir = rewrite_test_repo();
    let root = dir.path();
    git_ok(root, &["config", "--add", "notes.rewriteRef", "refs/*"]);

    assert_eq!(
        ensure_notes_rewrite_ref(Some(root)).await,
        RewriteRefStatus::Configured,
        "refs/* is not coverage; our exact ref must still be added"
    );

    append_to_git_notes(Some(root), &make_note_record(1, PRECIOUS))
        .await
        .expect("append should succeed");
    git_ok(
        root,
        &["commit", "--amend", "--no-gpg-sign", "-m", "amended"],
    );
    assert!(
        note_on_head(root).is_some_and(|b| b.contains(PRECIOUS)),
        "entry must survive despite the user's out-of-namespace glob"
    );
}

/// `--backend git-notes` makes notes the primary store, so an unconfigured carry
/// ref there orphans the only copy. Asserted through `list`, not just the raw
/// note: `list` intersects against `git log`, which is what actually made the
/// orphaned entry unreachable.
#[tokio::test]
#[serial]
async fn git_notes_backend_add_configures_carry_and_entry_survives_amend() {
    let dir = rewrite_test_repo();
    let root = dir.path();
    let backend = GitNotesBackend::with_root(root.to_path_buf());

    backend
        .add(note_input("decision", PRECIOUS))
        .await
        .expect("add");

    assert_eq!(
        rewrite_ref_values(root),
        vec!["refs/notes/spelunk"],
        "the backend write path must configure the carry ref too"
    );

    let before = git_stdout_ok(root, &["rev-parse", "HEAD"]);
    git_ok(
        root,
        &[
            "commit",
            "--amend",
            "--no-gpg-sign",
            "-m",
            "amended subject",
        ],
    );
    assert_ne!(
        before,
        git_stdout_ok(root, &["rev-parse", "HEAD"]),
        "the amend must actually have rewritten the commit"
    );

    let entries = backend
        .list(Some("decision"), 100, false, None)
        .await
        .expect("list");
    assert_eq!(
        entries.iter().filter(|n| n.title == PRECIOUS).count(),
        1,
        "the entry must still be reachable after an amend; got: {:?}",
        entries.iter().map(|n| &n.title).collect::<Vec<_>>()
    );
}

/// The carry config is ensured even when the notes lock is unavailable. It runs
/// before `lock_notes` and guards a write that proceeds either way, so moving it
/// inside the lock-acquired branch would silently stop configuring it under
/// contention, which is exactly when an entry is most at risk.
#[tokio::test]
#[serial]
async fn append_to_git_notes_ensures_carry_config_even_when_the_lock_is_unusable() {
    let dir = rewrite_test_repo();
    let root = dir.path();

    std::fs::create_dir_all(notes_lock_path(root)).expect("place a directory at the lock path");
    assert!(
        matches!(
            lock_notes(Some(root)).await.expect("resolve lock path"),
            LockAttempt::Unusable { .. }
        ),
        "setup: an unopenable lock path must report Unusable"
    );

    append_to_git_notes(Some(root), &make_note_record(1, PRECIOUS))
        .await
        .expect("append should succeed");

    assert_eq!(
        rewrite_ref_values(root),
        vec!["refs/notes/spelunk"],
        "the carry config must be set even when the lock degrades to unlocked"
    );
}

/// A carry config that cannot be written reports `Failed` and the write still
/// proceeds: pre-`init` the entry has no other copy, so a config failure must
/// never sink it.
///
/// A read-only `.git` blocks the config lock file while leaving object and ref
/// writes working (they land in subdirectories), isolating a config failure from
/// the note write. Unix-only: it turns on the directory mode.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn append_to_git_notes_proceeds_when_the_carry_config_cannot_be_written() {
    use std::os::unix::fs::PermissionsExt;

    const ONLY_COPY: &str = "a config failure must not sink the entry";

    let dir = rewrite_test_repo();
    let root = dir.path();
    let git_dir = root.join(".git");
    let original = std::fs::metadata(&git_dir)
        .expect("stat .git")
        .permissions();

    let mut read_only = original.clone();
    read_only.set_mode(0o555);
    std::fs::set_permissions(&git_dir, read_only).expect("make .git read-only");

    // Probe with raw git, never with the code under test: root (or a mount that
    // ignores the mode) can still write the config, and there is no failure to
    // assert against then. Deciding that from the status would let a mutation
    // that misreports failure as success route itself into the skip and pass.
    let enforced = !std::process::Command::new("git")
        .current_dir(root)
        .args(["config", "--add", "notes.rewriteRefProbe", "x"])
        .output()
        .expect("git config probe")
        .status
        .success();
    if !enforced {
        std::fs::set_permissions(&git_dir, original).expect("restore .git permissions");
        return;
    }

    let status = ensure_notes_rewrite_ref(Some(root)).await;
    let write = append_to_git_notes(Some(root), &make_note_record(1, ONLY_COPY)).await;
    let note = note_on_head(root);

    // Restore before asserting: a panic below would otherwise leave a read-only
    // directory behind that `TempDir` cannot clean up.
    std::fs::set_permissions(&git_dir, original).expect("restore .git permissions");

    assert_eq!(
        status,
        RewriteRefStatus::Failed,
        "an unwritable config must report Failed"
    );
    assert_eq!(
        write
            .expect("a config failure must never fail the write")
            .rewrite_ref,
        RewriteRefStatus::Failed,
        "the write must report the carry it could not make"
    );
    assert!(
        note.is_some_and(|b| b.contains(ONLY_COPY)),
        "the entry must still be written when the carry config fails"
    );
}

// ── ADR-069 D2/D5: merging fetched notes ─────────────────────────────────────

use spelunk_core::storage::{NotesMergeOutcome, merge_tracking_notes};

/// Raw stored bytes of HEAD's `refs/notes/spelunk` blob.
///
/// Reads the blob object rather than `git notes show`, so the assertion is
/// about what git actually stored and not about what the porcelain prints.
fn raw_note_blob_bytes(root: &std::path::Path) -> Vec<u8> {
    let list = std::process::Command::new("git")
        .args([
            "-C",
            root.to_str().unwrap(),
            "notes",
            "--ref=spelunk",
            "list",
        ])
        .output()
        .expect("git notes list");
    let listing = String::from_utf8_lossy(&list.stdout);
    let blob_sha = listing
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
        .expect("a note blob must exist");
    let out = std::process::Command::new("git")
        .args(["-C", root.to_str().unwrap(), "cat-file", "blob", blob_sha])
        .output()
        .expect("git cat-file blob");
    assert!(
        out.status.success(),
        "cat-file should resolve the note blob"
    );
    out.stdout
}

/// Point `refs/notes/origin/spelunk` at the current working ref, then reset the
/// working ref to `state` — simulating "a teammate's notes arrived on the
/// tracking ref via `git fetch`" without any network.
fn park_working_ref_as_tracking(root: &std::path::Path) {
    let run = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(["-C", root.to_str().unwrap()])
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&[
        "update-ref",
        "refs/notes/origin/spelunk",
        "refs/notes/spelunk",
    ]);
    run(&["update-ref", "-d", "refs/notes/spelunk"]);
}

/// (D2) The newline invariant that `cat_sort_uniq` rests on.
///
/// `append_to_git_notes` builds its body with `format!("{}\n{}", …)` and **no**
/// trailing newline; git's `notes add -F -` normalization is the only thing
/// that appends one. Without it a union welds the last line of one side onto
/// the first line of the other and both records stop parsing. That behaviour is
/// owned by git, not by spelunk, so it is pinned here rather than assumed: this
/// fails if git ever stops normalizing.
#[tokio::test]
#[serial]
async fn git_notes_add_normalizes_a_body_with_no_trailing_newline() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let body = r#"{"schema_version":1,"id":1,"kind":"decision","title":"no trailing newline"}"#;
    assert!(
        !body.ends_with('\n'),
        "fixture must lack a trailing newline"
    );
    write_raw_note(root, body);

    let stored = raw_note_blob_bytes(root);
    assert!(
        stored.ends_with(b"\n"),
        "git must normalize a note body to end with a newline, else cat_sort_uniq \
         welds records together; stored: {:?}",
        String::from_utf8_lossy(&stored)
    );
}

/// (D2) The invariant's payoff: a `cat_sort_uniq` union of two notes that were
/// each written without a trailing newline leaves every record parseable, with
/// no welded line.
#[tokio::test]
#[serial]
async fn cat_sort_uniq_union_never_welds_records() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    // Side A lands on the tracking ref, side B stays on the working ref: the
    // exact shape of a fetched teammate note meeting a local one.
    write_raw_note(
        root,
        r#"{"schema_version":1,"id":1,"kind":"decision","title":"theirs"}"#,
    );
    park_working_ref_as_tracking(root);
    write_raw_note(
        root,
        r#"{"schema_version":1,"id":2,"kind":"decision","title":"mine"}"#,
    );

    assert_eq!(
        merge_tracking_notes(Some(root)).await,
        NotesMergeOutcome::Merged
    );

    let merged = read_raw_note(root);
    for line in merged.lines().filter(|l| !l.trim().is_empty()) {
        serde_json::from_str::<serde_json::Value>(line.trim())
            .unwrap_or_else(|e| panic!("every merged line must still parse ({e}): {line:?}"));
    }
    assert!(
        merged.contains("theirs") && merged.contains("mine"),
        "both sides survive: {merged}"
    );
}

/// (D5) A fetched teammate note is invisible until the merge, and visible
/// after it. This is the whole point of the read-path merge.
#[tokio::test]
#[serial]
async fn merge_makes_fetched_notes_visible() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    append_to_git_notes(Some(root), &make_note_record(1, "their decision"))
        .await
        .expect("seed");
    park_working_ref_as_tracking(root);

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let before = backend.list(None, 50, false, None).await.expect("list");
    assert!(
        before.is_empty(),
        "a note on the tracking ref must not be visible before the merge"
    );

    assert_eq!(
        merge_tracking_notes(Some(root)).await,
        NotesMergeOutcome::Merged
    );

    let after = backend.list(None, 50, false, None).await.expect("list");
    assert!(
        after.iter().any(|n| n.title == "their decision"),
        "the merge must make the fetched note visible, got: {after:?}"
    );
}

/// (D5) No tracking ref (the solo / no-remote user) is a silent no-op that
/// never disturbs local notes and never fails the read.
#[tokio::test]
#[serial]
async fn merge_without_a_tracking_ref_is_a_silent_no_op() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    // With a local note present git exits 0; with both refs empty it exits 128.
    // Neither may surface to the caller, and neither may touch the local note.
    assert_eq!(
        merge_tracking_notes(Some(root)).await,
        NotesMergeOutcome::Skipped,
        "an empty repo with no tracking ref has nothing to merge"
    );

    append_to_git_notes(Some(root), &make_note_record(1, "only local"))
        .await
        .expect("seed");
    let before = read_raw_note(root);

    assert_eq!(
        merge_tracking_notes(Some(root)).await,
        NotesMergeOutcome::Merged,
        "git no-ops at exit 0 once the working ref exists"
    );
    assert_eq!(
        read_raw_note(root),
        before,
        "a merge with no tracking ref must leave the local note byte-identical"
    );
}

/// (D5/D6) A held lock skips the merge instead of waiting the reader out or
/// failing it. The union is idempotent, so the next read catches up.
#[tokio::test]
#[serial]
async fn merge_skips_when_the_lock_is_held() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    append_to_git_notes(Some(root), &make_note_record(1, "their decision"))
        .await
        .expect("seed");
    park_working_ref_as_tracking(root);

    // A distinct open, so the conflict with the merge's own handle is
    // well-defined. Costs one LOCK_WAIT_BUDGET of wall clock.
    let held = open_lock_file(&notes_lock_path(root));
    held.try_lock().expect("lock should start free");
    assert_eq!(
        merge_tracking_notes(Some(root)).await,
        NotesMergeOutcome::LockUnavailable,
        "a contended lock must skip the merge, not block or fail the read"
    );

    // …and the read still works, just without the fetched entry yet.
    let backend = GitNotesBackend::with_root(root.to_path_buf());
    backend
        .list(None, 50, false, None)
        .await
        .expect("a read must never fail on lock contention");

    drop(held);
    assert_eq!(
        merge_tracking_notes(Some(root)).await,
        NotesMergeOutcome::Merged,
        "the next read after the lock frees must catch up"
    );
    let after = backend.list(None, 50, false, None).await.expect("list");
    assert!(after.iter().any(|n| n.title == "their decision"));
}

/// (D2) `cat_sort_uniq` sorts lines lexicographically, so blob order stops
/// being chronological after a merge. Reads must sort by `created_at`.
///
/// The fixture is written so blob/lexicographic order and chronological order
/// disagree: ids ascend while `created_at` descends.
#[tokio::test]
#[serial]
async fn read_orders_records_by_created_at_not_blob_order() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let line = |id: i64, created_at: i64, title: &str| {
        format!(
            r#"{{"schema_version":1,"id":{id},"kind":"decision","title":"{title}","body":"b","tags":[],"linked_files":[],"created_at":{created_at},"status":"active"}}"#
        )
    };
    write_raw_note(
        root,
        &[
            line(1, 300, "third"),
            line(2, 100, "first"),
            line(3, 200, "second"),
        ]
        .join("\n"),
    );

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let notes = backend.list(None, 50, false, None).await.expect("list");
    let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();

    assert_eq!(
        titles,
        vec!["first", "second", "third"],
        "records must read back in created_at order regardless of blob order"
    );
}

// ── ADR-069 D5: what the merge must never do ─────────────────────────────────

/// Run `git args` in `root`, returning the raw `Output` (no success assertion).
fn git_at(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(["-C", root.to_str().unwrap()])
        .args(args)
        .output()
        .expect("git")
}

/// Trimmed stdout of `git args` in `root`, asserting success.
fn git_stdout_at(root: &std::path::Path, args: &[&str]) -> String {
    let out = git_at(root, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Put a genuinely diverged pair in place: a different note for HEAD on the
/// tracking ref and on the working ref, so a merge has a real conflict to
/// resolve. A fast-forward (empty working ref) would not exercise the strategy
/// at all, which is what most of these assertions are about.
fn seed_diverged_notes(root: &std::path::Path) {
    write_raw_note(
        root,
        r#"{"schema_version":1,"id":1,"kind":"decision","title":"theirs"}"#,
    );
    park_working_ref_as_tracking(root);
    write_raw_note(
        root,
        r#"{"schema_version":1,"id":2,"kind":"decision","title":"mine"}"#,
    );
}

/// (D5) The merge leaves no merge state behind in the git dir.
///
/// The `notes.mergeStrategy` default is `manual`: on a real conflict it exits 1
/// and strands `NOTES_MERGE_WORKTREE`, `NOTES_MERGE_PARTIAL` and
/// `NOTES_MERGE_REF`, wedging the user's own `git notes merge` until they run
/// `--abort`. Passing `-s cat_sort_uniq` per invocation is the only thing that
/// avoids it, so the absence of the debris is asserted, not assumed.
#[tokio::test]
#[serial]
async fn merge_leaves_no_notes_merge_state_in_the_git_dir() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    seed_diverged_notes(root);

    assert_eq!(
        merge_tracking_notes(Some(root)).await,
        NotesMergeOutcome::Merged
    );

    let git_dir = root.join(".git");
    for debris in [
        "NOTES_MERGE_WORKTREE",
        "NOTES_MERGE_PARTIAL",
        "NOTES_MERGE_REF",
    ] {
        assert!(
            !git_dir.join(debris).exists(),
            "the merge must not strand {debris} in the git dir"
        );
    }
}

/// (D5) A user's own `notes.mergeStrategy` can neither drop a teammate's note
/// nor be rewritten by spelunk.
///
/// `ours` is the dangerous setting: it resolves a conflict by discarding the
/// other side, so a merge that honoured it would silently drop exactly what the
/// merge exists to surface. `-s` on the command line outranks both the general
/// and the per-ref (`notes.spelunk.mergeStrategy`) scopes, which is why the
/// strategy is passed per invocation instead of configured.
#[tokio::test]
#[serial]
async fn merge_overrides_a_user_merge_strategy_that_would_drop_the_other_side() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    // Both scopes git consults for this ref, set to the lossy strategy.
    git_ok(root, &["config", "notes.mergeStrategy", "ours"]);
    git_ok(root, &["config", "notes.spelunk.mergeStrategy", "ours"]);

    seed_diverged_notes(root);

    assert_eq!(
        merge_tracking_notes(Some(root)).await,
        NotesMergeOutcome::Merged
    );

    let merged = read_raw_note(root);
    assert!(
        merged.contains("theirs") && merged.contains("mine"),
        "-s must outrank the user's 'ours' so neither side is dropped, got:\n{merged}"
    );
    // …and spelunk must not have rewritten the setting out from under them.
    assert_eq!(
        git_stdout_at(root, &["config", "--get", "notes.mergeStrategy"]),
        "ours",
        "the user's own merge strategy must be left exactly as they set it"
    );
    assert_eq!(
        git_stdout_at(root, &["config", "--get", "notes.spelunk.mergeStrategy"]),
        "ours",
        "the user's own per-ref merge strategy must be left exactly as they set it"
    );
}

/// (D5, security) The read path does no network.
///
/// The merge folds in only what the user's own `git fetch` already wrote. That
/// is what lets a read work with the remote unreachable, and it keeps egress off
/// a path the user never pointed at a remote. Pinned by making any network
/// attempt fail loudly: `origin` is configured, with the refspec `spelunk init`
/// writes, but points at a path that does not exist.
#[tokio::test]
#[serial]
async fn merge_does_no_network_and_reads_with_an_unreachable_origin() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let nowhere = root.join("no-such-origin.git");
    assert!(!nowhere.exists(), "setup: the origin must not exist");
    git_ok(
        root,
        &["remote", "add", "origin", nowhere.to_str().unwrap()],
    );
    git_ok(
        root,
        &[
            "config",
            "--add",
            "remote.origin.fetch",
            "+refs/notes/spelunk*:refs/notes/origin/spelunk*",
        ],
    );

    append_to_git_notes(Some(root), &make_note_record(1, "their decision"))
        .await
        .expect("seed");
    park_working_ref_as_tracking(root);
    let tracking_before = git_stdout_at(root, &["rev-parse", "refs/notes/origin/spelunk"]);

    // Negative control: prove a fetch really would fail from here, so `Merged`
    // below means the merge never reached for the network — not that it tried
    // and happened to get away with it.
    assert!(
        !git_at(root, &["fetch", "origin"]).status.success(),
        "setup: a fetch against a nonexistent origin must fail"
    );

    assert_eq!(
        merge_tracking_notes(Some(root)).await,
        NotesMergeOutcome::Merged,
        "the merge must not depend on reaching the remote"
    );

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let after = backend
        .list(None, 50, false, None)
        .await
        .expect("a read must work with the remote unreachable");
    assert!(
        after.iter().any(|n| n.title == "their decision"),
        "the offline read must still surface the fetched entry, got: {after:?}"
    );

    // A merge that fetched would have moved the tracking ref.
    assert_eq!(
        git_stdout_at(root, &["rev-parse", "refs/notes/origin/spelunk"]),
        tracking_before,
        "the merge must leave the tracking ref alone: updating it would mean it fetched"
    );
}

/// (D5, security) The merge never fetches: an entry the user has not fetched
/// stays invisible.
///
/// The sibling test proves a read survives an unreachable remote, but an
/// unreachable remote cannot tell a merge that skipped the network from one that
/// tried and failed — both look identical. Here `origin` is real, reachable, and
/// holds a note this repo has never fetched, so an implicit fetch would visibly
/// succeed: the tracking ref would appear and the entry would surface on the
/// read. Both are asserted absent, which is what makes the property testable at
/// all.
///
/// Travel is deliberately fetch **then** merge: the user's own `git fetch` is
/// the only thing that moves data, so spelunk reading memory never reaches the
/// network on its own.
#[tokio::test]
#[serial]
async fn merge_never_fetches_so_an_unfetched_entry_stays_invisible() {
    let dir = make_temp_git_repo();
    let root = dir.path();
    // The origin lives outside the working tree so it cannot perturb the repo.
    let origin_dir = tempfile::TempDir::new().expect("tempdir");
    let origin = origin_dir.path().join("origin.git");

    // `-b main` matches the repo: a bare origin defaulting to `master` leaves a
    // clone with nothing checked out.
    git_ok(
        root,
        &[
            "init",
            "--bare",
            "-q",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    git_ok(root, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git_ok(root, &["push", "-q", "origin", "main"]);
    git_ok(
        root,
        &[
            "config",
            "--add",
            "remote.origin.fetch",
            "+refs/notes/spelunk*:refs/notes/origin/spelunk*",
        ],
    );

    // Publish a note, then drop every local trace of it: origin now holds an
    // entry this repo has never fetched, exactly as a teammate's push would
    // leave it. The tracking ref is deleted too because `git push` populates it
    // itself once a fetch refspec maps the pushed ref.
    write_raw_note(
        root,
        r#"{"schema_version":1,"id":1,"kind":"decision","title":"never fetched"}"#,
    );
    git_ok(root, &["push", "-q", "origin", "refs/notes/spelunk"]);
    git_ok(root, &["update-ref", "-d", "refs/notes/spelunk"]);
    git_at(root, &["update-ref", "-d", "refs/notes/origin/spelunk"]);

    // A local entry of my own, so the read has something to return.
    append_to_git_notes(Some(root), &make_note_record(2, "my decision"))
        .await
        .expect("seed local");

    // Setup control: nothing has been fetched, so there is no tracking ref.
    assert!(
        !git_at(
            root,
            &["rev-parse", "--verify", "refs/notes/origin/spelunk"]
        )
        .status
        .success(),
        "setup: the tracking ref must be absent until the user fetches"
    );

    assert_eq!(
        merge_tracking_notes(Some(root)).await,
        NotesMergeOutcome::Merged,
        "nothing to merge is a silent no-op, not a failure"
    );

    assert!(
        !git_at(
            root,
            &["rev-parse", "--verify", "refs/notes/origin/spelunk"]
        )
        .status
        .success(),
        "the merge must not populate the tracking ref: that would mean it fetched"
    );

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let notes = backend.list(None, 50, false, None).await.expect("list");
    let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();
    assert!(
        !titles.contains(&"never fetched"),
        "an entry the user never fetched must not appear on a read: the read path \
         does no network, got: {titles:?}"
    );
    assert!(
        titles.contains(&"my decision"),
        "the local entry must still read, got: {titles:?}"
    );
}

// ── D5/D6: the lock contract, across real processes ──────────────────────────

/// Repo root the [`lock_holder_child`] helper process locks. Set only by
/// [`merge_skips_without_touching_the_ref_when_another_process_holds_the_lock`].
const LOCK_HOLDER_REPO_ENV: &str = "SPELUNK_TEST_LOCK_HOLDER_REPO";

/// Marker the helper writes once it holds the lock.
fn held_marker(root: &std::path::Path) -> std::path::PathBuf {
    root.join("lock-held.marker")
}

/// Marker the parent writes to tell the helper to release and exit.
fn release_marker(root: &std::path::Path) -> std::path::PathBuf {
    root.join("lock-release.marker")
}

/// Poll for `path` to appear, up to `budget`. Returns whether it showed up.
fn wait_for_marker(path: &std::path::Path, budget: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

/// Not a test: the lock-holding half of the cross-process test below, which
/// re-executes this binary to run it. Inert (and instantly green) unless
/// [`LOCK_HOLDER_REPO_ENV`] is set, so a plain `--ignored` run cannot hang.
///
/// Takes the lock through the real `lock_notes`, so the contention it creates is
/// the production one rather than a re-implementation of it.
#[tokio::test]
#[ignore = "helper process: driven by the cross-process lock test"]
async fn lock_holder_child() {
    let Ok(root) = std::env::var(LOCK_HOLDER_REPO_ENV) else {
        return;
    };
    let root = std::path::PathBuf::from(root);

    let attempt = lock_notes(Some(&root)).await.expect("resolve lock path");
    let LockAttempt::Acquired(guard) = attempt else {
        panic!("helper must get the lock (the parent takes it only after we signal): {attempt:?}");
    };
    std::fs::write(held_marker(&root), "held").expect("write held marker");

    // Hold until released, with a ceiling so a wedged parent cannot leak this
    // process. Comfortably above the parent's one LOCK_WAIT_BUDGET wait.
    assert!(
        wait_for_marker(&release_marker(&root), std::time::Duration::from_secs(120)),
        "parent never released the helper"
    );
    drop(guard);
}

/// (D5/D6) A lock held by a **separate process** makes the merge skip: it does
/// not block, does not fail the read, and does not touch the ref.
///
/// The sibling test above contends two handles inside one process, which is the
/// same OS primitive but not the same situation. This is the real one: two
/// spelunk processes (agents, worktrees, a hook racing a shell) on one repo,
/// which is the case the lock exists for. The helper takes the lock via
/// `lock_notes` itself, so nothing here re-implements the locking under test.
///
/// Costs one `LOCK_WAIT_BUDGET` of wall clock: the merge must genuinely wait the
/// budget out before giving up.
#[tokio::test]
#[serial]
async fn merge_skips_without_touching_the_ref_when_another_process_holds_the_lock() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    append_to_git_notes(Some(root), &make_note_record(1, "their decision"))
        .await
        .expect("seed");
    park_working_ref_as_tracking(root);
    // A local note too, so the working ref exists and has a sha to compare.
    append_to_git_notes(Some(root), &make_note_record(2, "my decision"))
        .await
        .expect("seed local");
    let ref_before = git_stdout_at(root, &["rev-parse", "refs/notes/spelunk"]);

    let mut child = std::process::Command::new(std::env::current_exe().expect("current_exe"))
        .args(["--exact", "lock_holder_child", "--ignored"])
        .env(LOCK_HOLDER_REPO_ENV, root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the lock-holder helper");

    assert!(
        wait_for_marker(&held_marker(root), std::time::Duration::from_secs(60)),
        "the helper process never acquired the lock"
    );

    let started = std::time::Instant::now();
    let outcome = merge_tracking_notes(Some(root)).await;
    let waited = started.elapsed();

    assert_eq!(
        outcome,
        NotesMergeOutcome::LockUnavailable,
        "a lock held by another process must skip the merge, not block or fail the read"
    );
    // Negative control: an immediate `None` would mean the lock was never
    // contended (a bad path, an unopenable file) and this test proved nothing.
    assert!(
        waited >= LOCK_WAIT_BUDGET,
        "the merge must wait the {LOCK_WAIT_BUDGET:?} budget out before skipping; \
         gave up after {waited:?}, so it never contended"
    );
    assert_eq!(
        git_stdout_at(root, &["rev-parse", "refs/notes/spelunk"]),
        ref_before,
        "a skipped merge must leave the working ref untouched"
    );

    // The read still works — it just does not see the fetched entry yet.
    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let during = backend
        .list(None, 50, false, None)
        .await
        .expect("a read must never fail because another process holds the lock");
    assert!(
        during.iter().any(|n| n.title == "my decision"),
        "the local entry must still read while the lock is held, got: {during:?}"
    );

    std::fs::write(release_marker(root), "go").expect("write release marker");
    assert!(
        child.wait().expect("wait for helper").success(),
        "the lock-holder helper must exit cleanly"
    );

    // The union is idempotent, so the next read catches up.
    assert_eq!(
        merge_tracking_notes(Some(root)).await,
        NotesMergeOutcome::Merged,
        "the read after the lock frees must catch up"
    );
    let after = backend.list(None, 50, false, None).await.expect("list");
    assert!(
        after.iter().any(|n| n.title == "their decision"),
        "the fetched entry must arrive on the catch-up read, got: {after:?}"
    );
}

// ── D2: the merge invariants with `entity_id` present ────────────────────────

/// (D2/D5) A union of records carrying `entity_id` keeps every record parseable
/// and every identity intact.
///
/// `cat_sort_uniq` unions raw lines knowing nothing of the schema, so a record
/// growing a field is exactly when welding or identity loss would show up. Both
/// sides go through `append_to_git_notes`, so the bodies are what production
/// writes, including the missing trailing newline git has to normalize.
#[tokio::test]
#[serial]
async fn union_merge_preserves_entity_id_on_both_sides() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let theirs = make_note_record(1, "their decision");
    let mine = make_note_record(2, "my decision");
    let their_entity = theirs
        .entity_id
        .clone()
        .expect("fixture must carry an entity_id");
    let my_entity = mine
        .entity_id
        .clone()
        .expect("fixture must carry an entity_id");
    assert_ne!(
        their_entity, my_entity,
        "fixture: distinct entries need distinct identities"
    );

    append_to_git_notes(Some(root), &theirs)
        .await
        .expect("seed");
    park_working_ref_as_tracking(root);
    append_to_git_notes(Some(root), &mine).await.expect("seed");

    assert_eq!(
        merge_tracking_notes(Some(root)).await,
        NotesMergeOutcome::Merged
    );

    // Every merged line still deserializes as a record (no welding), and both
    // identities came through.
    let merged = read_raw_note(root);
    let entity_ids: Vec<String> = merged
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str::<NoteRecord>(l.trim())
                .unwrap_or_else(|e| panic!("every merged line must parse as a record ({e}): {l:?}"))
                .resolve_entity_id()
        })
        .collect();
    assert!(
        entity_ids.contains(&their_entity) && entity_ids.contains(&my_entity),
        "both entity_ids must survive the union, got: {entity_ids:?}"
    );

    // …and the read path surfaces both entries.
    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let notes = backend.list(None, 50, false, None).await.expect("list");
    let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();
    assert!(
        titles.contains(&"their decision") && titles.contains(&"my decision"),
        "both entries must be visible after the union, got: {titles:?}"
    );
}

/// (D2) The `created_at` ordering holds for records carrying `entity_id`,
/// serialized the way production writes them.
///
/// The sibling ordering test hand-writes a minimal line, which cannot notice a
/// record-shape change. This one serializes a real `NoteRecord`, so the fixture
/// tracks whatever the struct currently is.
#[tokio::test]
#[serial]
async fn read_orders_records_by_created_at_with_entity_id_present() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    // ids ascend while created_at descends, so blob/lexicographic order and
    // chronological order disagree and only a sort can get this right.
    let mut lines = Vec::new();
    for (id, created_at, title) in [(1, 300, "third"), (2, 100, "first"), (3, 200, "second")] {
        let mut rec = make_note_record(id, title);
        rec.created_at = created_at;
        assert!(
            rec.entity_id.is_some(),
            "fixture must carry an entity_id, else this adds nothing"
        );
        lines.push(serde_json::to_string(&rec).expect("serialize"));
    }
    write_raw_note(root, &lines.join("\n"));

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let notes = backend.list(None, 50, false, None).await.expect("list");
    let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();

    assert_eq!(
        titles,
        vec!["first", "second", "third"],
        "created_at order must hold with entity_id present"
    );
}

// ── The entity fold: one decision recorded twice is one entry ────────────────

/// One entity, as a second machine would have recorded it: same
/// `{kind, title, body}` (so the same `entity_id`), its own `id`/`created_at`/
/// tags.
fn entity_copy(title: &str, id: i64, created_at: i64, tags: &[&str]) -> NoteRecord {
    let mut r = make_note_record(id, title);
    r.created_at = created_at;
    r.tags = tags.iter().map(|t| t.to_string()).collect();
    r
}

fn write_lines(root: &std::path::Path, records: &[&NoteRecord]) {
    let lines: Vec<String> = records
        .iter()
        .map(|r| serde_json::to_string(r).expect("serialize"))
        .collect();
    write_raw_note(root, &lines.join("\n"));
}

/// Two machines at the same HEAD: `cat_sort_uniq` leaves both copies as two
/// lines of one blob, and the read folds them into one entry.
#[tokio::test]
#[serial]
async fn same_head_duplicate_folds_to_one_entry() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let mine = entity_copy("shared decision", 1, 100, &["a"]);
    let theirs = entity_copy("shared decision", 7, 300, &["b"]);
    assert_eq!(
        mine.resolve_entity_id(),
        theirs.resolve_entity_id(),
        "fixture: both copies must be one entity"
    );
    assert_ne!(mine.id, theirs.id, "fixture: rowids differ per machine");

    write_lines(root, &[&theirs, &mine]);

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let notes = backend.list(None, 50, false, None).await.expect("list");

    assert_eq!(notes.len(), 1, "one decision, one entry, got: {notes:?}");
    assert_eq!(notes[0].created_at, 100, "earliest recording survives");
    assert_eq!(notes[0].tags, vec!["a", "b"], "tags merge by union");
}

/// Two machines at **different** HEADs: the copies land on two separate notes,
/// so only a fold that sees every commit can collapse them.
///
/// This is the test that pins the fold's placement: it fails if the fold moves
/// into a per-commit blob read, which sees one commit's blob at a time.
#[tokio::test]
#[serial]
async fn different_head_duplicate_folds_to_one_entry() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    // Their copy, recorded at their HEAD, arriving via `git fetch`.
    append_to_git_notes(Some(root), &entity_copy("shared decision", 7, 300, &["b"]))
        .await
        .expect("seed theirs");
    park_working_ref_as_tracking(root);

    // We commit before recording, so our copy attaches to a different commit.
    commit_file(root, "work.txt", "our work");
    append_to_git_notes(Some(root), &entity_copy("shared decision", 1, 100, &["a"]))
        .await
        .expect("seed mine");

    assert_eq!(
        merge_tracking_notes(Some(root)).await,
        NotesMergeOutcome::Merged
    );

    let listing = git_stdout_at(root, &["notes", "--ref=spelunk", "list"]);
    assert_eq!(
        listing.lines().count(),
        2,
        "fixture: the copies must sit on two separate notes, got: {listing}"
    );

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let notes = backend.list(None, 50, false, None).await.expect("list");

    assert_eq!(
        notes.len(),
        1,
        "one decision recorded on two machines is one entry, got: {notes:?}"
    );
    assert_eq!(notes[0].created_at, 100, "earliest recording survives");
    assert_eq!(
        notes[0].tags,
        vec!["a", "b"],
        "tags union across two commits' notes"
    );
}

/// Archival is monotonic, and the fold runs before the status filter: filtering
/// first would drop the archived copy and let the active one resurrect the
/// entity as live.
#[tokio::test]
#[serial]
async fn an_archived_copy_wins_and_the_entity_stays_hidden() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let active = entity_copy("shared decision", 1, 100, &[]);
    let mut archived = entity_copy("shared decision", 7, 300, &[]);
    archived.status = "archived".to_string();

    write_lines(root, &[&active, &archived]);

    let backend = GitNotesBackend::with_root(root.to_path_buf());

    let visible = backend.list(None, 50, false, None).await.expect("list");
    assert!(
        visible.is_empty(),
        "an archived entity must not come back as active, got: {visible:?}"
    );

    let all = backend.list(None, 50, true, None).await.expect("list all");
    assert_eq!(all.len(), 1, "still one entity, got: {all:?}");
    assert_eq!(all[0].status, "archived");
}

/// A line written before `entity_id` existed folds with a fresh copy of the
/// same entry: `resolve_entity_id()` recomputes the key it never stored.
#[tokio::test]
#[serial]
async fn a_legacy_line_folds_with_a_fresh_copy_of_the_same_entry() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let fresh = entity_copy("shared decision", 2, 300, &["new"]);
    let legacy = format!(
        r#"{{"schema_version":1,"id":1,"kind":"decision","title":"shared decision","body":"{}","tags":["old"],"linked_files":[],"created_at":100,"status":"active"}}"#,
        fresh.body
    );
    assert!(
        !legacy.contains("entity_id"),
        "fixture: the legacy line must not carry the key"
    );

    write_raw_note(
        root,
        &[legacy, serde_json::to_string(&fresh).expect("serialize")].join("\n"),
    );

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let notes = backend.list(None, 50, false, None).await.expect("list");

    assert_eq!(
        notes.len(),
        1,
        "a recomputed entity_id must fold with a stored one, got: {notes:?}"
    );
    assert_eq!(notes[0].created_at, 100, "the legacy copy is the earliest");
    assert_eq!(notes[0].tags, vec!["new", "old"], "tags merge by union");
}

/// `limit` keeps the **newest** entries, as the sqlite backend's
/// `ORDER BY created_at DESC LIMIT` does, and still emits them ascending.
///
/// The failure this pins: a repo past the cap returning its oldest entries and
/// hiding every recent decision.
#[tokio::test]
#[serial]
async fn limit_keeps_the_newest_entries_and_emits_them_ascending() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let records: Vec<NoteRecord> = (1..=5)
        .map(|i| entity_copy(&format!("decision {i}"), i, i * 100, &[]))
        .collect();
    write_lines(root, &records.iter().collect::<Vec<_>>());

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let notes = backend.list(None, 2, false, None).await.expect("list");
    let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();

    assert_eq!(
        titles,
        vec!["decision 4", "decision 5"],
        "the newest N, oldest-first"
    );
}

/// Folding before `limit` is what makes the count exact: were `limit` applied
/// to raw records, an entity's copies would each spend a slot and the read
/// would return fewer entries than asked for.
#[tokio::test]
#[serial]
async fn duplicate_copies_do_not_spend_the_limit_budget() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    // One decision recorded on three machines, plus two others.
    let a1 = entity_copy("recorded thrice", 1, 100, &[]);
    let a2 = entity_copy("recorded thrice", 2, 110, &[]);
    let a3 = entity_copy("recorded thrice", 3, 120, &[]);
    let b = entity_copy("second", 4, 200, &[]);
    let c = entity_copy("third", 5, 300, &[]);
    write_lines(root, &[&a1, &a2, &a3, &b, &c]);

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let notes = backend.list(None, 3, false, None).await.expect("list");
    let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();

    assert_eq!(
        titles,
        vec!["recorded thrice", "second", "third"],
        "three entities must fill a limit of three, got: {titles:?}"
    );
}

/// The fold collapses copies, never distinct entries.
#[tokio::test]
#[serial]
async fn three_distinct_entities_stay_three_entries() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let first = entity_copy("first", 1, 100, &[]);
    let second = entity_copy("second", 2, 200, &[]);
    let third = entity_copy("third", 3, 300, &[]);
    write_lines(root, &[&first, &second, &third]);

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let notes = backend.list(None, 50, false, None).await.expect("list");
    let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();

    assert_eq!(
        titles,
        vec!["first", "second", "third"],
        "distinct entities must not collapse"
    );
}

// ── ADR-068 A6 retrofit: supersede edges travel via the carrier ─────────────
//
// `memory supersede` and `memory add --supersedes` append a state-update
// record for the OLD entity rather than rewriting it in place — a live-git
// experiment showed an in-place rewrite does not converge under a genuine
// merge (a second clone holding the original line plus a divergent note of
// its own keeps both the stale original and the rewrite after
// `cat_sort_uniq`). These tests exercise `append_state_update`'s write shape
// together with the fold above, the way a receiving clone actually sees it.

use spelunk_core::storage::append_state_update;
use spelunk_core::storage::memory::Note;

/// A `Note` as `backend.get()` would hand back right after an entry is
/// created — the shape `append_state_update`'s `base` parameter expects.
fn note_for(title: &str, id: i64, created_at: i64) -> Note {
    Note {
        id: NoteId::from_i64(id),
        kind: "decision".to_string(),
        title: title.to_string(),
        body: format!("body for {title}"),
        tags: vec![],
        linked_files: vec![],
        created_at,
        status: "active".to_string(),
        superseded_by: None,
        source_ref: None,
        valid_at: None,
        invalid_at: None,
        distance: None,
        score: None,
        source_project: None,
        source_project_path: None,
        remote_id: None,
    }
}

/// `append_state_update` appends a new line; it never touches the entity's
/// existing one. Pinning "the original line survives byte-for-byte" is what
/// rules out the in-place rewrite the spec's live-git experiment showed does
/// not converge.
#[tokio::test]
#[serial]
async fn append_state_update_appends_never_rewrites() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    append_to_git_notes(Some(root), &make_note_record(1, "decision X"))
        .await
        .expect("seed original");
    let original = read_raw_note(root);
    let original_line = original
        .lines()
        .next()
        .expect("one seeded line")
        .to_string();

    let old = note_for("decision X", 1, 100);
    append_state_update(
        Some(root),
        &old,
        "archived",
        Some(900),
        Some("succ-entity".to_string()),
    )
    .await
    .expect("append state update");

    let after = read_raw_note(root);
    let lines: Vec<&str> = after.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        2,
        "a state update must be a second line, not a rewrite of the first: {after}"
    );
    assert_eq!(
        lines[0], original_line,
        "the original line must survive byte-for-byte"
    );

    let updated: NoteRecord = serde_json::from_str(lines[1]).expect("parse state update");
    assert_eq!(updated.status, "archived");
    assert_eq!(updated.invalid_at, Some(900));
    assert_eq!(
        updated.superseded_by_entity_id.as_deref(),
        Some("succ-entity")
    );
}

/// The edge direction is the easy mistake to get backwards: it must land on
/// OLD's appended record, pointing at NEW's `entity_id` — never on NEW's own
/// record.
#[tokio::test]
#[serial]
async fn supersede_edge_lands_on_old_record_not_new() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    append_to_git_notes(Some(root), &make_note_record(1, "decision X"))
        .await
        .expect("seed X");
    append_to_git_notes(Some(root), &make_note_record(2, "decision Y"))
        .await
        .expect("seed Y");

    let old = note_for("decision X", 1, 100);
    let new = note_for("decision Y", 2, 200);
    let new_entity_id = spelunk_core::storage::note_entity_id(&new);
    let old_entity_id = spelunk_core::storage::note_entity_id(&old);

    append_state_update(
        Some(root),
        &old,
        "archived",
        Some(500),
        Some(new_entity_id.clone()),
    )
    .await
    .expect("append state update");

    let blob = read_raw_note(root);
    let records: Vec<NoteRecord> = blob
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse"))
        .collect();

    for r in &records {
        if r.resolve_entity_id() == new_entity_id {
            assert!(
                r.superseded_by_entity_id.is_none(),
                "the edge must never land on NEW's record: {r:?}"
            );
        }
    }
    let old_update = records
        .iter()
        .find(|r| r.resolve_entity_id() == old_entity_id && r.status == "archived")
        .expect("OLD's state-update record must exist");
    assert_eq!(
        old_update.superseded_by_entity_id.as_deref(),
        Some(new_entity_id.as_str()),
        "the edge must land on OLD's record, pointing at NEW's entity_id"
    );
}

/// A clone that received X's supersede state-update but never received Y's
/// own record must still show X archived, never live: `status` travels on
/// X's own record rather than being inferred from whether the edge resolves
/// (ADR-068 A3/A6). X is still counted — not silently dropped just because
/// its successor is unresolvable.
#[tokio::test]
#[serial]
async fn unresolved_successor_never_renders_as_live_and_is_counted() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    append_to_git_notes(Some(root), &make_note_record(1, "decision X"))
        .await
        .expect("seed X");
    let old = note_for("decision X", 1, 100);

    // Y's entity_id, computed the same way a real successor's would be, but Y's
    // own record never lands on this clone.
    let missing_successor = entity_id("decision", "decision Y", "body for decision Y");

    append_state_update(
        Some(root),
        &old,
        "archived",
        Some(900),
        Some(missing_successor),
    )
    .await
    .expect("append state update");

    let backend = GitNotesBackend::with_root(root.to_path_buf());

    let live = backend
        .list(None, 50, false, None)
        .await
        .expect("list live");
    assert!(
        live.is_empty(),
        "X must never render as live once archived, got: {live:?}"
    );

    let all = backend.list(None, 50, true, None).await.expect("list all");
    assert_eq!(
        all.len(),
        1,
        "X must still be counted, not silently dropped: {all:?}"
    );
    assert_eq!(all[0].status, "archived");
    assert_eq!(all[0].title, "decision X");
}

/// The headline test: clone A records X, then supersedes it with Y; clone B —
/// holding a divergent local note of its own so the merge genuinely unions
/// rather than fast-forwards — fetches and merges. B's `list` must show X
/// archived (superseded by Y) exactly once, and Y live exactly once.
///
/// A test that fast-forwards here "passes while proving nothing": with no
/// divergent content on B's side, `git notes merge` never has to invoke
/// `cat_sort_uniq`'s union at all, so it would never exercise the fold this
/// task's writes depend on.
#[tokio::test]
#[serial]
async fn supersede_edge_travels_across_a_genuinely_diverging_merge() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    // ── Clone A's history, about to be "fetched" onto B's tracking ref ───────
    append_to_git_notes(Some(root), &make_note_record(1, "decision X"))
        .await
        .expect("A: record X");
    append_to_git_notes(Some(root), &make_note_record(2, "decision Y"))
        .await
        .expect("A: record Y");
    let old = note_for("decision X", 1, 100);
    let new = note_for("decision Y", 2, 200);
    let new_entity_id = spelunk_core::storage::note_entity_id(&new);
    append_state_update(Some(root), &old, "archived", Some(300), Some(new_entity_id))
        .await
        .expect("A: supersede X with Y");

    // "git fetch" lands A's whole history on the tracking ref.
    park_working_ref_as_tracking(root);

    // B's own, never-pushed local note — genuine divergence: without this the
    // merge below fast-forwards and the test would prove nothing.
    append_to_git_notes(Some(root), &make_note_record(3, "decision Z"))
        .await
        .expect("B: local decision Z");

    assert_eq!(
        merge_tracking_notes(Some(root)).await,
        NotesMergeOutcome::Merged,
        "setup: the merge must be a genuine union, not a no-op"
    );

    let backend = GitNotesBackend::with_root(root.to_path_buf());

    let live = backend
        .list(None, 50, false, None)
        .await
        .expect("list live");
    let live_titles: Vec<&str> = live.iter().map(|n| n.title.as_str()).collect();
    assert_eq!(
        live_titles.iter().filter(|&&t| t == "decision X").count(),
        0,
        "X must not render as live after being superseded: {live_titles:?}"
    );
    assert_eq!(
        live_titles.iter().filter(|&&t| t == "decision Y").count(),
        1,
        "Y must be live exactly once: {live_titles:?}"
    );
    assert!(
        live_titles.contains(&"decision Z"),
        "B's own note must survive the merge: {live_titles:?}"
    );

    let all = backend.list(None, 50, true, None).await.expect("list all");
    let x_entries: Vec<_> = all.iter().filter(|n| n.title == "decision X").collect();
    assert_eq!(
        x_entries.len(),
        1,
        "X must fold to exactly one entry, archived, superseded by Y, not duplicated: {all:?}"
    );
    assert_eq!(x_entries[0].status, "archived");
}

/// Idempotence: the same supersede recorded twice (e.g. a retried carrier
/// write, or two machines independently linking the same pair) still
/// converges to one folded entry, never two.
#[tokio::test]
#[serial]
async fn supersede_is_idempotent_across_repeated_state_updates() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    append_to_git_notes(Some(root), &make_note_record(1, "decision X"))
        .await
        .expect("record X");
    append_to_git_notes(Some(root), &make_note_record(2, "decision Y"))
        .await
        .expect("record Y");
    let old = note_for("decision X", 1, 100);
    let new = note_for("decision Y", 2, 200);
    let new_entity_id = spelunk_core::storage::note_entity_id(&new);

    for _ in 0..2 {
        append_state_update(
            Some(root),
            &old,
            "archived",
            Some(300),
            Some(new_entity_id.clone()),
        )
        .await
        .expect("append state update");
    }

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let all = backend.list(None, 50, true, None).await.expect("list all");
    let x_entries: Vec<_> = all.iter().filter(|n| n.title == "decision X").collect();
    assert_eq!(
        x_entries.len(),
        1,
        "repeating the identical state update must still fold to one entry: {all:?}"
    );
    assert_eq!(x_entries[0].status, "archived");
}

// ── ADR-068 E4/E5: the pre-flight check does not close every race ──────────
//
// E4's pre-flight ("is OLD still active?") runs once, as a plain read, before
// either write in `memory add --supersedes`'s pre-init (git-notes-only) path;
// nothing holds `writer_lock` across the read and the eventual write, and
// there is no SQL-style atomic "UPDATE ... WHERE status = 'active'" available
// on a git-notes carrier to re-validate at write time (unlike the post-init
// SQLite path, where `add_note_superseding`'s own guarded UPDATE is what
// actually enforces correctness under a race — the CLI's pre-flight read is
// there only to fail fast, not to carry the guarantee).
//
// So two `memory add --supersedes OLD` processes racing pre-init, each reading
// OLD as active before either commits, can both proceed to append their own
// "OLD archived, superseded by ME" state update — reproducing, via a genuine
// race, the exact conflicting-carrier state E4 was written to stop from a
// single sequential caller. This is not fixable without real cross-process
// locking around the read (out of scope here — ADR-068 D3 already documents
// the carrier has no cross-machine locking); what following documents and
// pins is the shape of the residual gap, plus the one guarantee that *does*
// still hold: `status` still folds to a coherent, monotonic "archived",
// because `merge_into`'s archival rule is whole-group-safe regardless of how
// many conflicting copies exist. (Which successor `superseded_by_entity_id`
// resolves to is the E5 fold rule, pinned separately and exhaustively in
// `storage::git_notes::fold`'s own unit tests — `Note`, the type every public
// `MemoryBackend` method returns, does not carry that field, so it cannot be
// asserted on from an external integration test like this one.)

/// Simulates exactly what two `memory add --supersedes OLD` CLI invocations
/// racing on the same OLD (both reading it as active before either writes)
/// leave behind: two independent, both-successful `append_state_update` calls
/// against the same OLD, naming two different successors. Pins that this is
/// still possible today (the write-side race window E4 cannot fully close
/// pre-init) and that it does not corrupt the fold beyond the successor
/// pointer: `status` still converges to exactly one archived entry.
#[tokio::test]
#[serial]
async fn racing_supersedes_of_the_same_old_both_land_conflicting_state_updates() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    append_to_git_notes(Some(root), &make_note_record(1, "decision OLD"))
        .await
        .expect("record OLD");
    append_to_git_notes(Some(root), &make_note_record(2, "successor A"))
        .await
        .expect("record successor A");
    append_to_git_notes(Some(root), &make_note_record(3, "successor B"))
        .await
        .expect("record successor B");

    // Both racers' pre-flight reads see OLD as active (neither has written
    // yet) — modeled by reading OLD once, here, and reusing it for both
    // "processes'" writes below, exactly like `add.rs`'s `old_note_for_carrier`
    // reuses one pre-flight read for its own single write.
    let old = note_for("decision OLD", 1, 100);
    let successor_a = note_for("successor A", 2, 200);
    let successor_b = note_for("successor B", 3, 250);
    let entity_id_a = spelunk_core::storage::note_entity_id(&successor_a);
    let entity_id_b = spelunk_core::storage::note_entity_id(&successor_b);

    // Racer A commits first.
    append_state_update(Some(root), &old, "archived", Some(300), Some(entity_id_a))
        .await
        .expect("racer A's state update");
    // Racer B commits second, having made its own "OLD is active" decision
    // before racer A's write landed — nothing prevents this today.
    append_state_update(Some(root), &old, "archived", Some(300), Some(entity_id_b))
        .await
        .expect("racer B's state update");

    // The write-side gap: both conflicting state updates actually landed: two
    // JSON lines both naming OLD's entity_id, with `status: archived`, and two
    // different `superseded_by_entity_id` values. A closed race would have
    // rejected the second.
    let blob = read_raw_note(root);
    let old_entity_id =
        spelunk_core::storage::entity_id("decision", "decision OLD", "body for decision OLD");
    let conflicting_lines = blob
        .lines()
        .filter(|l| l.contains(&old_entity_id) && l.contains("\"archived\""))
        .count();
    assert_eq!(
        conflicting_lines, 2,
        "both racers' conflicting state updates must have landed (the gap this \
         test documents); got blob: {blob:?}"
    );

    // What still holds despite the race: the fold's monotonic archival rule
    // means every reader sees OLD as archived, coherently, exactly once —
    // never a resurrected active copy, never two entries.
    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let all = backend.list(None, 50, true, None).await.expect("list all");
    let old_entries: Vec<_> = all.iter().filter(|n| n.title == "decision OLD").collect();
    assert_eq!(
        old_entries.len(),
        1,
        "conflicting copies must still fold to exactly one entry: {all:?}"
    );
    assert_eq!(
        old_entries[0].status, "archived",
        "must fold to archived regardless of which successor's copy is newer"
    );
}
