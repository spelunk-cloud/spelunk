//! Integration tests for `spelunk init` configuring the `origin` git-notes
//! fetch refspec so teammates' `refs/notes/spelunk` (spelunk's memory) travels
//! on clone/fetch (ADR-068, corrected by ADR-069 D4/D5).
//!
//! The refspec fetches into a **tracking** ref (`refs/notes/origin/spelunk`),
//! never over the working ref. Fetching straight onto `refs/notes/spelunk`
//! force-updates it and silently destroys local unpushed notes, and the
//! non-glob form makes plain `git fetch` exit 128 until someone pushes notes.
//! Travel is therefore fetch + merge: spelunk merges the tracking ref on its
//! own read paths (D5).
//!
//! Covered:
//! - origin present: `remote.origin.fetch` gains the tracking refspec and
//!   init announces the configured line.
//! - origin absent: init still exits 0 and prints the exact manual hint.
//! - idempotent: two inits leave exactly ONE notes refspec + "already
//!   configured" announce on the second run.
//! - push preserved: `remote.origin.push` stays unset (branch-push default).
//! - plain git preserved: `git fetch`/`git pull` exit 0 with no notes on the
//!   remote (D4 regression).
//! - no clobber: a local unpushed note survives a fetch when the remote has
//!   notes (D4 regression).
//! - round-trip: notes pushed to a bare origin reach a fresh clone's tracking
//!   ref on fetch, and the read-path merge makes them visible (D5).
//! - call sites: `context` and `init` merge the tracking ref too, not just
//!   `memory list` (D5).
//! - non-TTY: piped-stdin init completes without prompting/hanging.
//!
//! Every spawned `spelunk` uses `spelunk_bin` (pins `SPELUNK_SECRET_STORE=file`),
//! `SPELUNK_NO_SERVER=1`, and `init --no-index` for an offline, fast run.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin;

use predicates::prelude::*;
use std::path::Path;
use std::process::Output;
use tempfile::tempdir;

const NOTES_REFSPEC: &str = "+refs/notes/spelunk*:refs/notes/origin/spelunk*";

/// The ref a fetch lands teammates' notes on, per [`NOTES_REFSPEC`].
const TRACKING_REF: &str = "refs/notes/origin/spelunk";

/// Run `git args` in `dir`, asserting success. Isolated identity + config so it
/// works hermetically on a machine with (or without) a global git config.
fn git(dir: &Path, args: &[&str]) {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Like [`git`] but returns the captured `Output` without asserting success.
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

/// `stdout` of `git args` as a trimmed `String`.
fn git_stdout(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&git_out(dir, args).stdout)
        .trim()
        .to_string()
}

/// A git repo with a real identity + one commit. Returns nothing; caller owns
/// the dir. Local identity is set so spawned `git` (and spelunk's inner git)
/// can commit without inheriting the test-runner's global config.
fn init_repo_with_commit(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "# test\n").unwrap();
    git(dir, &["add", "README.md"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

/// Write an empty spelunk config (init needs `--config` but no values here).
fn empty_config(dir: &Path) -> std::path::PathBuf {
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, "").unwrap();
    cfg
}

/// Run `spelunk init --no-index` in `dir` (offline, non-TTY) and return stdout.
fn run_init(dir: &Path) -> String {
    let cfg = empty_config(dir);
    let out = spelunk_bin()
        .current_dir(dir)
        .env("HOME", dir)
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

/// (1) With an `origin` remote: init adds the notes fetch refspec and announces it.
#[test]
fn init_configures_notes_refspec_when_origin_present() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let origin = tmp.path().join("origin.git");
    std::fs::create_dir_all(&repo).unwrap();

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
    init_repo_with_commit(&repo);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    let stdout = run_init(&repo);

    let fetch = git_stdout(&repo, &["config", "--get-all", "remote.origin.fetch"]);
    assert!(
        fetch.lines().any(|l| l.trim() == NOTES_REFSPEC),
        "remote.origin.fetch should contain the notes refspec, got:\n{fetch}"
    );
    assert!(
        stdout.contains("Memory:") && stdout.contains("configured notes fetch refspec on 'origin'"),
        "init stdout should announce the configured refspec, got:\n{stdout}"
    );
}

/// (2) No `origin` remote: init still succeeds and prints the exact manual hint.
#[test]
fn init_no_origin_prints_hint_and_succeeds() {
    let tmp = tempdir().unwrap();
    init_repo_with_commit(tmp.path());

    let stdout = run_init(tmp.path()); // asserts exit 0 internally

    assert!(
        stdout.contains(&format!(
            "git config --add remote.origin.fetch '{NOTES_REFSPEC}'"
        )),
        "no-origin init should print the exact refspec hint, got:\n{stdout}"
    );
    // Publishing is opt-in, so init must name the hook that does it rather than
    // the old manual push, which orphaned notes on unpushed commits (D1/D3).
    assert!(
        stdout.contains("spelunk hooks install --pre-push"),
        "no-origin init should name the pre-push hook install command, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("push notes after each memory change"),
        "the retired per-change manual push hint must not reappear, got:\n{stdout}"
    );
    // And it must not have invented an `origin` remote.
    assert!(
        !git_out(tmp.path(), &["remote", "get-url", "origin"])
            .status
            .success(),
        "init must not create an origin remote when none exists"
    );
}

/// (2a) With the pre-push hook installed, init announces that memory publishes
/// rather than repeating that it stays local.
///
/// The announce is the only place a user learns whether their memory travels,
/// so a summary that ignores the installed hook is init telling them the
/// opposite of the truth.
#[test]
fn init_announces_the_pre_push_hook_once_installed() {
    let tmp = tempdir().unwrap();
    init_repo_with_commit(tmp.path());

    spelunk_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("SPELUNK_NO_SERVER", "1")
        .args(["hooks", "install", "--pre-push"])
        .assert()
        .success();

    let stdout = run_init(tmp.path());

    assert!(
        stdout.contains("pre-push hook installed: your memory publishes on `git push`"),
        "init must report the hook as installed, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("your memory stays local"),
        "init must not claim memory stays local once the hook publishes it, got:\n{stdout}"
    );
}

/// (2b) No `origin` remote: init still configures `notes.rewriteRef`, so memory
/// survives `git commit --amend` and `git rebase`. The carry is purely local and
/// must not be gated on having a remote: a remote-less repo is exactly where the
/// note is the only copy of an entry.
///
/// Read `--local` so the assertion is about what init wrote to this repo, not
/// about an ambient global value on the machine running the test.
#[test]
fn init_configures_notes_rewrite_ref_without_an_origin_remote() {
    let tmp = tempdir().unwrap();
    init_repo_with_commit(tmp.path());
    assert!(
        !git_out(tmp.path(), &["remote", "get-url", "origin"])
            .status
            .success(),
        "setup: this repo must have no origin remote"
    );

    let stdout = run_init(tmp.path());

    assert_eq!(
        git_stdout(
            tmp.path(),
            &["config", "--local", "--get-all", "notes.rewriteRef"]
        )
        .trim(),
        "refs/notes/spelunk",
        "init must configure the notes carry ref even with no origin, got:\n{stdout}"
    );
    assert!(
        stdout.contains("configured notes.rewriteRef"),
        "init should announce the carry config, got:\n{stdout}"
    );
}

/// (3) Idempotent: two inits leave exactly one notes refspec + "already
/// configured" announce on the second run.
#[test]
fn init_notes_refspec_is_idempotent() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let origin = tmp.path().join("origin.git");
    std::fs::create_dir_all(&repo).unwrap();

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
    init_repo_with_commit(&repo);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    run_init(&repo);
    let second = run_init(&repo);

    let count = git_stdout(&repo, &["config", "--get-all", "remote.origin.fetch"])
        .lines()
        .filter(|l| l.trim() == NOTES_REFSPEC)
        .count();
    assert_eq!(
        count, 1,
        "notes refspec must appear exactly once after two inits"
    );
    assert!(
        second.contains("already configured"),
        "second init should report the refspec is already configured, got:\n{second}"
    );
}

/// (4) Push default preserved: `remote.origin.push` stays unset so a normal
/// `git push` keeps pushing the current branch (the engineer set no push refspec).
#[test]
fn init_does_not_set_origin_push_refspec() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let origin = tmp.path().join("origin.git");
    std::fs::create_dir_all(&repo).unwrap();

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
    init_repo_with_commit(&repo);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    run_init(&repo);

    let push = git_out(&repo, &["config", "--get", "remote.origin.push"]);
    assert!(
        !push.status.success() && String::from_utf8_lossy(&push.stdout).trim().is_empty(),
        "remote.origin.push must remain unset, got: {:?}",
        String::from_utf8_lossy(&push.stdout)
    );
}

/// (5) Round-trip (the promise): a note pushed to the bare origin reaches a
/// fresh clone's tracking ref on a plain `git fetch`, and spelunk's read-path
/// merge is what makes it visible.
///
/// A. init in repo (configures the refspec) → add a decision (git note on
///    refs/notes/spelunk) → push the branch + notes ref to the bare origin.
/// B. clone origin → run init in the clone (adds the same fetch refspec) →
///    plain `git fetch origin` lands the notes on `refs/notes/origin/spelunk`
///    and deliberately NOT on the working ref → `spelunk memory list` merges
///    the tracking ref and surfaces the decision. This proves the ref is
///    publishable, that the init-configured refspec fetches it, and that
///    travel is fetch + merge rather than fetch alone.
#[test]
fn notes_round_trip_through_bare_origin() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let origin = tmp.path().join("origin.git");
    let clone = tmp.path().join("clone");
    std::fs::create_dir_all(&repo).unwrap();

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
    init_repo_with_commit(&repo);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    // A. Configure the refspec, then add a decision via `spelunk memory add`
    //    (store_in_git_notes = true → writes refs/notes/spelunk).
    run_init(&repo);

    let mem_db = repo.join(".spelunk").join("memory.db");
    let cfg = repo.join("mem-config.toml");
    std::fs::write(
        &cfg,
        format!(
            "db_path = {:?}\nllm_model = \"x\"\nstore_in_git_notes = true\n",
            mem_db
        ),
    )
    .unwrap();

    let unique = "notes travel via the origin refspec";
    spelunk_bin()
        .current_dir(&repo)
        .env("HOME", &repo)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(&cfg)
        .arg("memory")
        .arg("--db")
        .arg(&mem_db)
        .arg("add")
        .arg("--kind")
        .arg("decision")
        .arg("--title")
        .arg(unique)
        .arg("--body")
        .arg("Chosen so refs/notes/spelunk clone/fetch behaviour is observable.")
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [decision]"));

    // Sanity: the note exists locally on refs/notes/spelunk.
    assert!(
        !git_stdout(&repo, &["notes", "--ref=spelunk", "list"]).is_empty(),
        "expected a local spelunk note after memory add"
    );

    // Publish branch + notes to the bare origin.
    git(&repo, &["push", "-q", "origin", "main"]);
    git(&repo, &["push", "-q", "origin", "refs/notes/spelunk"]);

    // B. Fresh clone gets the branch but NOT notes by default…
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    // clone identity for its own inner git (init announces, no commit needed).
    git(&clone, &["config", "user.email", "clone@example.com"]);
    git(&clone, &["config", "user.name", "Clone"]);
    assert!(
        git_stdout(&clone, &["notes", "--ref=spelunk", "list"]).is_empty(),
        "a fresh clone should not have spelunk notes before fetch"
    );

    // …init in the clone configures the notes fetch refspec, and a plain fetch
    // then lands the notes ref — on the TRACKING ref, not the working one.
    //
    // `run_init` here also performs the first read-path merge, so drop the
    // tracking ref's content out of the working ref afterwards to observe the
    // fetch in isolation: assert on the tracking ref directly.
    run_init(&clone);
    git(&clone, &["fetch", "-q", "origin"]);

    assert!(
        git_out(&clone, &["rev-parse", "--verify", TRACKING_REF])
            .status
            .success(),
        "a plain fetch must populate {TRACKING_REF} via the init-configured refspec"
    );

    // The decision content travelled, not just an empty ref.
    let tracking_notes = git_stdout(&clone, &["notes", &format!("--ref={TRACKING_REF}"), "list"]);
    let annotated = tracking_notes
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .expect("note list line has an annotated object")
        .to_string();
    let shown = git_stdout(
        &clone,
        &[
            "notes",
            &format!("--ref={TRACKING_REF}"),
            "show",
            &annotated,
        ],
    );
    assert!(
        shown.contains(unique),
        "fetched note should contain the decision title, got:\n{shown}"
    );

    // And the read path surfaces it: `memory list` merges the tracking ref.
    let listed = spelunk_bin()
        .current_dir(&clone)
        .env("HOME", &clone)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
        .args(["memory", "--backend", "git-notes", "list"])
        .output()
        .expect("spawn spelunk memory list");
    assert!(
        listed.status.success(),
        "memory list should succeed in the clone: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains(unique),
        "the read-path merge should surface the fetched decision, got:\n{}",
        String::from_utf8_lossy(&listed.stdout)
    );
    // The merge is what moved it onto the working ref.
    assert!(
        git_stdout(&clone, &["notes", "--ref=spelunk", "show", &annotated]).contains(unique),
        "the read-path merge should have folded the tracking ref into refs/notes/spelunk"
    );
}

/// Write `body` as HEAD's note on `git_ref`, standing in for a `git fetch` that
/// landed a teammate's note on the tracking ref. No network, and no dependence
/// on the refspec under test in the tests that use it to set up.
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

/// The `refs/notes/spelunk` blob for HEAD, or `""` when there is no note.
fn working_note(dir: &Path) -> String {
    let out = git_out(dir, &["notes", "--ref=spelunk", "show", "HEAD"]);
    if out.status.success() {
        String::from_utf8_lossy(&out.stdout).into_owned()
    } else {
        String::new()
    }
}

/// (5b) Call site: `spelunk context` merges the tracking ref (ADR-069 D5).
///
/// `memory list` is covered by the round-trip above; `context` is a separate
/// call site with its own read path, and a fetched entry is invisible on it
/// unless it merges too. Asserted on a genuinely diverged pair, so the entry
/// arriving proves a union rather than a fast-forward.
#[test]
fn context_merges_the_tracking_ref_and_surfaces_a_fetched_entry() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo_with_commit(&repo);

    run_init(&repo);

    // A teammate's entry, as a fetch would have left it; plus one of my own, so
    // the refs genuinely diverge.
    const THEIRS: &str = r#"{"schema_version":1,"id":1,"kind":"decision","title":"their fetched decision","body":"b","tags":[],"linked_files":[],"created_at":100,"status":"active"}"#;
    const MINE: &str = r#"{"schema_version":1,"id":2,"kind":"decision","title":"my local decision","body":"b","tags":[],"linked_files":[],"created_at":200,"status":"active"}"#;
    add_note_on_ref(&repo, TRACKING_REF, THEIRS);
    add_note_on_ref(&repo, "refs/notes/spelunk", MINE);

    // Setup control: the fetched entry is not on the working ref yet, so
    // surfacing it below can only be the merge's doing.
    assert!(
        !working_note(&repo).contains("their fetched decision"),
        "setup: the fetched entry must start out on the tracking ref only"
    );

    let cfg = empty_config(&repo);
    let out = spelunk_bin()
        .current_dir(&repo)
        .env("HOME", &repo)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(&cfg)
        .args(["context", "--backend", "git-notes"])
        .output()
        .expect("spawn spelunk context");
    assert!(
        out.status.success(),
        "spelunk context should succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("their fetched decision"),
        "context must merge the tracking ref and surface the fetched entry, got:\n{stdout}"
    );
    assert!(
        stdout.contains("my local decision"),
        "the union must not drop my local entry, got:\n{stdout}"
    );
    assert!(
        working_note(&repo).contains("their fetched decision"),
        "context's merge should have folded the tracking ref into refs/notes/spelunk"
    );
}

/// (5c) Call site: `spelunk init` merges the tracking ref before importing
/// (ADR-069 D5).
///
/// init hydrates `memory.db` from git notes, so an entry still parked on the
/// tracking ref would be skipped by the import and stay missing from the
/// project's memory until some later read merged it. Nothing else in init
/// writes the working ref, so the entry landing there isolates init's merge.
#[test]
fn init_merges_the_tracking_ref_before_importing_git_notes() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo_with_commit(&repo);

    // A teammate's entry arrives on the tracking ref before this repo is ever
    // init'd: the fresh-clone case.
    const THEIRS: &str = r#"{"schema_version":1,"id":1,"kind":"decision","title":"their fetched decision","body":"b","tags":[],"linked_files":[],"created_at":100,"status":"active"}"#;
    add_note_on_ref(&repo, TRACKING_REF, THEIRS);
    assert!(
        working_note(&repo).is_empty(),
        "setup: nothing may be on the working ref yet"
    );

    let stdout = run_init(&repo);

    assert!(
        working_note(&repo).contains("their fetched decision"),
        "init must merge the tracking ref onto refs/notes/spelunk"
    );
    // The payoff: the merge fed the import, so the entry is in the project's
    // memory. Without the merge the import sees an empty working ref and
    // announces nothing.
    assert!(
        stdout.contains("imported 1 entries from git notes"),
        "init must import the fetched entry it merged, got:\n{stdout}"
    );
}

/// (6) Non-TTY: init run with piped stdin (as assert_cmd/Output does) must not
/// prompt or hang — it returns and exits 0. Explicit guard for the hook/CI path.
#[test]
fn init_non_tty_does_not_prompt_or_hang() {
    let tmp = tempdir().unwrap();
    init_repo_with_commit(tmp.path());

    // run_init spawns with piped (non-TTY) stdin and asserts exit 0; reaching
    // this line at all means init completed without blocking on input.
    let stdout = run_init(tmp.path());
    assert!(
        stdout.contains("spelunk initialised for"),
        "init should print its success summary in non-TTY mode, got:\n{stdout}"
    );
}

/// (7) D4 regression: `spelunk init` must not break plain git.
///
/// The shipped non-glob refspec (`+refs/notes/spelunk:refs/notes/spelunk`)
/// requires the remote ref to exist, so with no notes pushed yet — every repo
/// until someone shares memory — `git fetch origin` exited 128 and `git pull`
/// exited 1 with `fatal: couldn't find remote ref refs/notes/spelunk`. The glob
/// tolerates the missing remote ref.
#[test]
fn init_leaves_plain_fetch_and_pull_working_with_no_notes_on_the_remote() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let origin = tmp.path().join("origin.git");
    std::fs::create_dir_all(&repo).unwrap();

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
    init_repo_with_commit(&repo);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    // `-u` sets upstream so `git pull` has something to track; without it pull
    // exits 1 for an unrelated reason and the assertion would be meaningless.
    git(&repo, &["push", "-q", "-u", "origin", "main"]);

    // The origin deliberately has NO notes: the state that broke.
    run_init(&repo);

    let fetch = git_out(&repo, &["fetch", "origin"]);
    assert!(
        fetch.status.success(),
        "git fetch must still exit 0 after init when the remote has no notes, got {:?}: {}",
        fetch.status.code(),
        String::from_utf8_lossy(&fetch.stderr)
    );

    let pull = git_out(&repo, &["pull"]);
    assert!(
        pull.status.success(),
        "git pull must still exit 0 after init when the remote has no notes, got {:?}: {}",
        pull.status.code(),
        String::from_utf8_lossy(&pull.stderr)
    );
}

/// (8) D4 regression: a local unpushed note survives a fetch.
///
/// The shipped refspec fetched with a leading `+` straight onto the working
/// ref, so a plain `git fetch` force-updated it and silently replaced a local
/// unpushed note with the remote's — reported only as `(forced update)`, and
/// recoverable only via reflog. That is data loss of the product's core asset.
/// A glob alone does not fix it; only the tracking destination does.
#[test]
fn local_unpushed_note_survives_a_fetch_when_the_remote_has_notes() {
    let tmp = tempdir().unwrap();
    let teammate = tmp.path().join("teammate");
    let origin = tmp.path().join("origin.git");
    let mine = tmp.path().join("mine");
    std::fs::create_dir_all(&teammate).unwrap();

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
    init_repo_with_commit(&teammate);
    git(
        &teammate,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&teammate, &["push", "-q", "origin", "main"]);

    // A teammate publishes their note, so the remote ref exists and diverges.
    const THEIRS: &str = r#"{"schema_version":1,"id":1,"kind":"decision","title":"theirs"}"#;
    git(
        &teammate,
        &["notes", "--ref=spelunk", "add", "-f", "-m", THEIRS, "HEAD"],
    );
    git(&teammate, &["push", "-q", "origin", "refs/notes/spelunk"]);

    // I clone and record my own note locally, without pushing it.
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            mine.to_str().unwrap(),
        ],
    );
    git(&mine, &["config", "user.email", "mine@example.com"]);
    git(&mine, &["config", "user.name", "Mine"]);
    run_init(&mine);

    const MINE: &str = r#"{"schema_version":1,"id":2,"kind":"decision","title":"mine unpushed"}"#;
    git(
        &mine,
        &["notes", "--ref=spelunk", "add", "-f", "-m", MINE, "HEAD"],
    );

    git(&mine, &["fetch", "-q", "origin"]);

    // The fetch must not have touched my working ref.
    let after = git_stdout(&mine, &["notes", "--ref=spelunk", "show", "HEAD"]);
    assert!(
        after.contains("mine unpushed"),
        "a plain fetch must not clobber a local unpushed note, got:\n{after}"
    );
    // Their note is fetched, but parked on the tracking ref until spelunk merges.
    assert!(
        git_out(&mine, &["rev-parse", "--verify", TRACKING_REF])
            .status
            .success(),
        "the teammate's note should land on {TRACKING_REF}"
    );
}
