// Integration tests for ADR-077: a teammate's fetched memory reaches the
// DEFAULT SQLite `memory.db` read paths, not only the git ref.
//
// The crux these tests exist to pin — and the precise shape of a prior
// false-close in this area — is that every two-clone round trip asserts through
// the default `memory.db` path (no `--backend git-notes`). Reading the git ref
// directly proves the merge, not the read path a real user hits.
//
// Every spawned `spelunk` pins `SPELUNK_SECRET_STORE=file` (via `spelunk_bin`),
// `SPELUNK_NO_SERVER=1`, and `init --no-index` for an offline, fast run.

mod plumbing_helpers;
use plumbing_helpers::{register_sqlite_vec, spelunk_bin};

use std::path::{Path, PathBuf};
use std::process::Output;
use tempfile::{TempDir, tempdir};

const NOTES_REFSPEC: &str = "+refs/notes/spelunk*:refs/notes/origin/spelunk*";
const TRACKING_REF: &str = "refs/notes/origin/spelunk";

// ── git helpers (hermetic: never read the developer's global git config) ──────

fn git(dir: &Path, args: &[&str]) {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> Output {
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

// `stdout` of `git args` as a trimmed `String`.
fn git_stdout(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&git_out(dir, args).stdout)
        .trim()
        .to_string()
}

fn init_repo_with_commit(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "# test\n").unwrap();
    git(dir, &["add", "README.md"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

// Write HEAD's note on `git_ref`, standing in for a `git fetch` that landed a
// teammate's note on the tracking ref — no network, no remote required.
fn add_note_on_ref(dir: &Path, git_ref: &str, body: &str) {
    git(
        dir,
        &[
            "notes",
            &format!("--ref={git_ref}"),
            "add",
            "-f",
            "-m",
            body,
            "HEAD",
        ],
    );
}

// ── spelunk helpers ───────────────────────────────────────────────────────────

// A personal config that satisfies `Config::validate` and points the index-db
// resolution somewhere harmless. `store_in_git_notes` opts the memory writes
// into the git-notes carrier (the publish side of the round trip).
fn write_config(dir: &Path, store_in_git_notes: bool) -> PathBuf {
    let cfg = dir.join("spelunk-config.toml");
    let index_db = dir.join(".spelunk").join("index.db");
    let mut body = format!("db_path = {index_db:?}\nllm_model = \"x\"\n");
    if store_in_git_notes {
        body.push_str("store_in_git_notes = true\n");
    }
    std::fs::write(&cfg, body).unwrap();
    cfg
}

fn mem_db(dir: &Path) -> PathBuf {
    dir.join(".spelunk").join("memory.db")
}

// Run `spelunk init --no-index` in `dir` (offline, non-TTY); returns stdout.
fn run_init(dir: &Path) -> String {
    let cfg = write_config(dir, false);
    let out = spelunk_bin()
        .current_dir(dir)
        .env("HOME", dir)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
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

// Author records a decision (into `memory.db` and `refs/notes/spelunk`) and
// publishes the notes ref to `origin`. This is the publish side of the round
// trip; the note lands on the shared HEAD commit the reader also has.
fn publish_note(author: &Path, title: &str, body: &str) {
    let cfg = write_config(author, true);
    let out = spelunk_bin()
        .current_dir(author)
        .env("HOME", author)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(&cfg)
        .arg("memory")
        .arg("--db")
        .arg(mem_db(author))
        .arg("add")
        .arg("--kind")
        .arg("decision")
        .arg("--title")
        .arg(title)
        .arg("--body")
        .arg(body)
        .output()
        .expect("spawn spelunk memory add");
    assert!(
        out.status.success(),
        "memory add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Publish the notes ref (the pre-push hook does this on `git push` in real
    // use; an explicit push is the deterministic equivalent for a test).
    git(author, &["push", "-q", "origin", "refs/notes/spelunk"]);
}

// Run a `spelunk memory <sub>` command on the DEFAULT backend in `dir`,
// against `dir`'s project `memory.db`. Deliberately never passes
// `--backend git-notes`: the whole point is the SQLite read path.
fn read_memory(dir: &Path, sub_args: &[&str]) -> Output {
    let cfg = write_config(dir, false);
    let mut cmd = spelunk_bin();
    cmd.current_dir(dir)
        .env("HOME", dir)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(&cfg)
        .arg("memory")
        .arg("--db")
        .arg(mem_db(dir));
    for a in sub_args {
        cmd.arg(a);
    }
    cmd.output().expect("spawn spelunk memory")
}

// Run `spelunk context` on the DEFAULT backend in `dir`.
fn read_context(dir: &Path) -> Output {
    let cfg = write_config(dir, false);
    let memdb = mem_db(dir);
    spelunk_bin()
        .current_dir(dir)
        .env("HOME", dir)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(&cfg)
        .arg("context")
        .arg("--db")
        .arg(&memdb)
        .arg("--local-only")
        .output()
        .expect("spawn spelunk context")
}

fn stdout_of(out: &Output) -> String {
    assert!(
        out.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

// Bare origin + an author clone (committed, pushed main) + a fresh reader
// clone (identity set, NOT yet init'd). The two clones share HEAD, so a note
// on that commit is reachable from either.
struct Team {
    _tmp: TempDir,
    author: PathBuf,
    reader: PathBuf,
}

fn setup_team() -> Team {
    let tmp = tempdir().unwrap();
    let origin = tmp.path().join("origin.git");
    let author = tmp.path().join("author");
    let reader = tmp.path().join("reader");
    std::fs::create_dir_all(&author).unwrap();

    git(
        tmp.path(),
        &[
            "init",
            "--bare",
            "-q",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    init_repo_with_commit(&author);
    git(
        &author,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&author, &["push", "-q", "origin", "main"]);

    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            reader.to_str().unwrap(),
        ],
    );
    git(&reader, &["config", "user.email", "reader@example.com"]);
    git(&reader, &["config", "user.name", "Reader"]);

    Team {
        _tmp: tmp,
        author,
        reader,
    }
}

// ── 1–4: the fetched teammate note surfaces on each default read path ─────────
//
// The note is published AFTER the reader inits, so init's own import cannot be
// what surfaces it — only the read-path import can. That isolates ADR-077 D1.

const MARKER: &str = "team-memory-read-path-marker";

fn one_note_reaches_reader(team: &Team) {
    // Reader configures the refspec (init), THEN a teammate publishes, THEN a
    // plain fetch lands it on the tracking ref only.
    run_init(&team.reader);
    publish_note(&team.author, MARKER, "the teammate's decision body");
    git(&team.reader, &["fetch", "-q", "origin"]);

    // Setup control: the fetch left it on the tracking ref, not the working one.
    assert!(
        git_out(&team.reader, &["rev-parse", "--verify", TRACKING_REF])
            .status
            .success(),
        "the fetch must populate the tracking ref"
    );
}

#[test]
fn read_path_surfaces_fetched_teammate_note_via_default_sqlite_backend() {
    let team = setup_team();
    one_note_reaches_reader(&team);

    let out = read_memory(&team.reader, &["list", "--local-only"]);
    assert!(
        stdout_of(&out).contains(MARKER),
        "memory list on the DEFAULT backend must surface the fetched teammate note"
    );
}

#[test]
fn search_surfaces_fetched_teammate_note_on_default_path() {
    let team = setup_team();
    one_note_reaches_reader(&team);

    // Text mode needs no embedder (no server here).
    let out = read_memory(
        &team.reader,
        &["search", MARKER, "--mode", "text", "--local-only"],
    );
    assert!(
        stdout_of(&out).contains(MARKER),
        "memory search --mode text on the DEFAULT backend must surface the fetched note"
    );
}

#[test]
fn show_surfaces_fetched_teammate_note_on_default_path() {
    let team = setup_team();
    one_note_reaches_reader(&team);

    // The imported note is the first (and only) row, so it takes id 1. `show`
    // must itself trigger the import — nothing read the store before this.
    let out = read_memory(&team.reader, &["show", "1"]);
    assert!(
        stdout_of(&out).contains(MARKER),
        "memory show on the DEFAULT backend must surface the fetched note by id"
    );
}

#[test]
fn context_surfaces_fetched_teammate_note_on_default_path() {
    let team = setup_team();
    one_note_reaches_reader(&team);

    let out = read_context(&team.reader);
    assert!(
        stdout_of(&out).contains(MARKER),
        "context on the DEFAULT backend must surface the fetched teammate decision"
    );
}

// ── 5: a single init after clone hydrates teammate memory (ADR-077 D3) ────────

#[test]
fn single_init_after_clone_imports_without_manual_refetch_reinit() {
    let team = setup_team();

    // Teammate publishes BEFORE the reader ever runs init: the fresh-clone case.
    publish_note(&team.author, MARKER, "published before the reader inits");

    // Exactly one init — which now configures the refspec, fetches, and imports.
    run_init(&team.reader);

    // No manual `git fetch`, no second init: a plain default-backend read shows it.
    let out = read_memory(&team.reader, &["list", "--local-only"]);
    assert!(
        stdout_of(&out).contains(MARKER),
        "a single init after clone must hydrate teammate memory (no fetch→reinit dance)"
    );
}

// ── 6: a read with the notes ref unchanged does not re-import ─────────────────

#[test]
fn read_skips_import_when_notes_ref_unchanged() {
    register_sqlite_vec();
    let team = setup_team();
    one_note_reaches_reader(&team);

    // First read imports the note and records the working-ref OID marker.
    assert!(
        stdout_of(&read_memory(&team.reader, &["list", "--local-only"])).contains(MARKER),
        "the first read must import the note"
    );

    // Remove the imported row directly. A gate that correctly skips leaves it
    // gone; a gate that re-walks the ref would resurrect it.
    let memdb = mem_db(&team.reader);
    {
        let conn = rusqlite::Connection::open(&memdb).expect("open memory.db");
        let deleted = conn
            .execute(
                "DELETE FROM notes WHERE title = ?1",
                rusqlite::params![MARKER],
            )
            .expect("delete note");
        assert_eq!(deleted, 1, "exactly the imported row must be removed");
    }

    // Second read, notes ref unchanged (no fetch since): must NOT re-import.
    let out = read_memory(&team.reader, &["list", "--local-only"]);
    let stdout = stdout_of(&out);
    assert!(
        !stdout.contains(MARKER),
        "an unchanged notes ref must not trigger a re-import, got:\n{stdout}"
    );
}

// ── 7: a read after the notes ref advances imports the new entry ──────────────

#[test]
fn read_imports_after_notes_ref_advances() {
    let team = setup_team();
    one_note_reaches_reader(&team);

    // Import the first note and settle the marker.
    assert!(stdout_of(&read_memory(&team.reader, &["list", "--local-only"])).contains(MARKER));

    // A second teammate entry is published and fetched: the notes ref advances.
    const SECOND: &str = "team-memory-second-entry-marker";
    publish_note(&team.author, SECOND, "a later teammate decision");
    git(&team.reader, &["fetch", "-q", "origin"]);

    let out = read_memory(&team.reader, &["list", "--local-only"]);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains(SECOND),
        "a read after the notes ref advanced must import the new entry, got:\n{stdout}"
    );
    assert!(
        stdout.contains(MARKER),
        "the first entry must still be present"
    );
}

// ── 8: the read-path import performs no network ───────────────────────────────

#[test]
fn read_path_import_does_no_network() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo_with_commit(&repo);
    run_init(&repo);

    // No `origin` remote at all, so no fetch/network is even possible.
    assert!(
        !git_out(&repo, &["remote", "get-url", "origin"])
            .status
            .success(),
        "this repo must have no origin remote"
    );

    // A teammate entry, exactly as a prior `git fetch` would have left it: on
    // the tracking ref only. The read must reach it with zero network.
    const THEIRS: &str = r#"{"schema_version":1,"id":1,"kind":"decision","title":"team-memory-no-network-marker","body":"b","tags":[],"linked_files":[],"created_at":100,"status":"active"}"#;
    add_note_on_ref(&repo, TRACKING_REF, THEIRS);

    let out = read_memory(&repo, &["list", "--local-only"]);
    assert!(
        stdout_of(&out).contains("team-memory-no-network-marker"),
        "the read must surface the locally-fetched tracking-ref entry with no network"
    );
}

// ── 9: the import marker is persisted in memory.db and survives reopen ────────

#[test]
fn import_marker_persisted_in_memory_db_and_survives_reopen() {
    let team = setup_team();
    one_note_reaches_reader(&team);

    // A read imports and records the working-ref OID marker.
    assert!(stdout_of(&read_memory(&team.reader, &["list", "--local-only"])).contains(MARKER));

    let memdb = mem_db(&team.reader);
    let read_marker = || -> Option<String> {
        let conn = rusqlite::Connection::open(&memdb).expect("open memory.db");
        conn.query_row(
            "SELECT last_imported_working_oid FROM notes_import_state WHERE id = 0",
            [],
            |r| r.get::<_, Option<String>>(0),
        )
        .expect("query marker")
    };

    let first = read_marker();
    assert!(
        first.as_deref().is_some_and(|s| s.len() == 40),
        "the working-ref OID marker must be persisted, got: {first:?}"
    );
    // The working ref the marker records must be the live one.
    let working = git_stdout(&team.reader, &["rev-parse", "refs/notes/spelunk"]);
    assert_eq!(
        first.as_deref(),
        Some(working.as_str()),
        "marker must equal the working-ref OID"
    );

    // A second open (fresh connection) still sees it: it lives in the store.
    assert_eq!(
        read_marker(),
        first,
        "the marker must survive a store reopen"
    );
}

// ── 10: offline init still succeeds and configures the refspec ────────────────

#[test]
fn init_offline_succeeds_and_configures_refspec() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo_with_commit(&repo);

    // An `origin` pointing at a path that does not exist: `git remote get-url`
    // succeeds (so the refspec is configured), but the best-effort fetch fails
    // (offline). init must still exit 0.
    let bogus = tmp.path().join("nonexistent-origin.git");
    git(&repo, &["remote", "add", "origin", bogus.to_str().unwrap()]);

    let stdout = run_init(&repo); // asserts exit 0 internally

    let fetch = git_stdout(&repo, &["config", "--get-all", "remote.origin.fetch"]);
    assert!(
        fetch.lines().any(|l| l.trim() == NOTES_REFSPEC),
        "offline init must still configure the notes fetch refspec, got:\n{fetch}"
    );
    assert!(
        stdout.contains("spelunk initialised for"),
        "offline init must complete its success summary, got:\n{stdout}"
    );
}

// ── 11: init writes config.toml but makes no git change ───────────────────────

#[test]
fn init_writes_config_toml_but_makes_no_git_change() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo_with_commit(&repo);

    let commits_before = git_stdout(&repo, &["rev-list", "--count", "HEAD"]);

    run_init(&repo);

    // 1. The file is on disk.
    assert!(
        repo.join(".spelunk").join("config.toml").exists(),
        "init must write .spelunk/config.toml"
    );
    // 2. Nothing is staged: `git diff --cached` mentions no config.toml.
    let staged = git_stdout(&repo, &["diff", "--cached", "--name-only"]);
    assert!(
        !staged.contains("config.toml"),
        "init must not stage .spelunk/config.toml, staged:\n{staged}"
    );
    // 3. No init-authored commit: HEAD is unchanged.
    let commits_after = git_stdout(&repo, &["rev-list", "--count", "HEAD"]);
    assert_eq!(
        commits_before, commits_after,
        "init must not author a commit"
    );
    // The file shows up as untracked, confirming it exists but is unstaged.
    let status = git_stdout(&repo, &["status", "--porcelain", ".spelunk/config.toml"]);
    assert!(
        status.starts_with("??"),
        "config.toml must be untracked (not staged), status:\n{status}"
    );
}

// ── 12: import dedups colliding ids across two authors ────────────────────────

#[test]
fn import_dedups_colliding_ids_across_two_authors() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo_with_commit(&repo);
    run_init(&repo);

    // Two authors recorded the same decision: identical kind/title/body, but
    // their own local rowids and created_at. `cat_sort_uniq` keeps both lines;
    // the import must collapse them to ONE row by content-addressed entity_id.
    let two_authors = concat!(
        r#"{"schema_version":1,"id":1,"kind":"decision","title":"team-memory-collision","body":"same body","tags":[],"linked_files":[],"created_at":100,"status":"active"}"#,
        "\n",
        r#"{"schema_version":1,"id":2,"kind":"decision","title":"team-memory-collision","body":"same body","tags":[],"linked_files":[],"created_at":200,"status":"active"}"#,
    );
    add_note_on_ref(&repo, TRACKING_REF, two_authors);

    let out = read_memory(&repo, &["list", "--format", "jsonl", "--local-only"]);
    let stdout = stdout_of(&out);
    let count = stdout
        .lines()
        .filter(|l| l.contains("team-memory-collision"))
        .count();
    assert_eq!(
        count, 1,
        "two authors' colliding-id copies of one decision must import as ONE row, got:\n{stdout}"
    );
}

// ── 13: a read outside a git repo makes no import attempt ─────────────────────

#[test]
fn no_git_repo_read_makes_no_import_attempt() {
    // A plain directory (no `.git` ancestor), with a seeded memory.db.
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let memdb = dir.join(".spelunk").join("memory.db");

    // Seed one local note via the write path (no git involved).
    let cfg = write_config(dir, false);
    spelunk_bin()
        .current_dir(dir)
        .env("HOME", dir)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(&cfg)
        .arg("memory")
        .arg("--db")
        .arg(&memdb)
        .args([
            "add",
            "--kind",
            "note",
            "--title",
            "local-only-seed",
            "--body",
            "b",
        ])
        .output()
        .map(|o| {
            assert!(
                o.status.success(),
                "seed add failed: {}",
                String::from_utf8_lossy(&o.stderr)
            )
        })
        .expect("spawn add");

    let out = read_memory(dir, &["list", "--local-only"]);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("local-only-seed"),
        "the seeded local note must list"
    );

    // The import gate returned before touching the marker: no marker row exists,
    // proving no import machinery ran outside a git repo.
    let conn = rusqlite::Connection::open(&memdb).expect("open memory.db");
    let markers: i64 = conn
        .query_row("SELECT count(*) FROM notes_import_state", [], |r| r.get(0))
        .expect("count markers");
    assert_eq!(
        markers, 0,
        "a read outside a git repo must make no import attempt (no marker written)"
    );
}
