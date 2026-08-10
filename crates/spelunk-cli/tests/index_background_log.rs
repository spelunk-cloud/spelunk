//! Diagnostics from the *detached* index phases must reach the user.
//!
//! The child spawned above the 100-file threshold used to close its stderr, so
//! everything it emitted - including the summary-failure warning - went to
//! `/dev/null`. A warning existing in the code is not evidence the user sees
//! it, so every test here drives the real spawn and asserts on what lands on
//! stderr or in the log file. None asserts on source.
//!
//! The sibling suite `index_summary_completion.rs` invokes `--_background-phases`
//! directly, which never exercises the spawn and so cannot catch this.

mod plumbing_helpers;

use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer};

/// Above the `indexed > 100` threshold that selects the background spawn.
const FILE_COUNT: usize = 120;

/// Failsafe only: hit solely when the detached child never finishes.
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);

fn log_path(spelunk_dir: &Path) -> std::path::PathBuf {
    spelunk_dir.join("index-background.log")
}

/// A project over the threshold: one chunk per file, so `indexed` tracks
/// file count.
fn write_big_fixture(dir: &Path) {
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("create src dir");
    for i in 0..FILE_COUNT {
        std::fs::write(
            src.join(format!("m{i}.rs")),
            format!("pub fn func_{i}(x: i32) -> i32 {{\n    x + {i}\n}}\n"),
        )
        .expect("write module");
    }
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"big-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
}

/// Mock server whose `/llm/complete` always 500s: a dead inference server.
async fn dead_llm_server() -> MockServer {
    let server = MockServer::start().await;
    plumbing_helpers::mount_health(&server).await;
    plumbing_helpers::mount_index_embed(&server).await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/llm/complete$"))
        .respond_with(wiremock::ResponseTemplate::new(500))
        .mount(&server)
        .await;
    server
}

/// `spelunk index` exactly as a user in the project runs it: cwd inside the
/// project, config found by discovery at `.spelunk/config.toml`.
///
/// Discovery rather than `--config` is deliberate. The spawn does not pass
/// `--config` to the child, so a test that used an explicit one would have the
/// child silently find no `server_url` and skip the summary pass the test is
/// about.
///
/// `SPELUNK_MODE=cloud_first`: every test in this file drives its fixture's
/// explicit `server_url` (2026-07-23 ADR-004 revision).
/// `index/summaries.rs::generate_summaries` calls `ServerInferenceClient::
/// from_config` directly on the loaded `Config` with no loopback
/// auto-discovery bridging (its pre-existing `cfg.server_url.is_none()` skip
/// already meant an auto-discovered server was never used here either), so
/// under the default `local_first` mode a bare `server_url` no longer
/// resolves to any inference target at all. `cloud_first` is what makes this
/// fixture's premise — an explicit `server_url` IS used for summaries — hold.
fn index_command(project: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("spelunk"));
    cmd.current_dir(project)
        .env("SPELUNK_SECRET_STORE", "file")
        .env("HOME", project)
        // Derived from this test's own `HOME` rather than inherited: an ambient
        // `SPELUNK_CONFIG_DIR` wins over `HOME`, so without this every test in
        // the file would share one config and secret store instead of getting
        // an isolated one.
        .env(
            "SPELUNK_CONFIG_DIR",
            project.join(".config").join("spelunk"),
        )
        .env("SPELUNK_MODE", "cloud_first")
        .env_remove("XDG_CONFIG_HOME")
        .arg("index")
        .arg(".");
    cmd
}

/// The spawn is detached, so the parent returns before the child has written
/// anything. Poll until the child's last phase lands, rather than sleeping.
fn wait_for_log_containing(path: &Path, needle: &str) -> String {
    let deadline = Instant::now() + CHILD_TIMEOUT;
    let mut last = String::new();
    while Instant::now() < deadline {
        last = std::fs::read_to_string(path).unwrap_or_default();
        if last.contains(needle) {
            return last;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!(
        "timed out waiting for {needle:?} in {}:\n{last}",
        path.display()
    );
}

/// Wait for the detached child to exit, so an assertion about a file it may
/// still write cannot pass by racing it.
fn wait_for_child_to_settle(path: &Path) {
    let deadline = Instant::now() + CHILD_TIMEOUT;
    while Instant::now() < deadline {
        // "Conventions:" is the last thing phase 5 prints.
        if std::fs::read_to_string(path)
            .unwrap_or_default()
            .contains("Conventions:")
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Wait for the child via the DB, for the cases where the log is unwritable and
/// so cannot report progress. Phase 5 is the child's last act, so a conventions
/// row means it finished; without this the temp dir can be torn down under it.
fn wait_for_child_via_db(db: &Path) {
    let deadline = Instant::now() + CHILD_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(conn) = rusqlite::Connection::open(db)
            && let Ok(n) = conn.query_row("SELECT COUNT(*) FROM conventions", [], |r| {
                r.get::<_, i64>(0)
            })
            && n > 0
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

struct Fixture {
    _project: TempDir,
    server: MockServer,
    db: std::path::PathBuf,
    project_path: std::path::PathBuf,
    spelunk_dir: std::path::PathBuf,
}

/// A >100-file project pointed at a dead LLM, laid out as a real one: config
/// and index under `.spelunk/`, so the detached child discovers the same
/// config the parent used.
fn fixture(rt: &tokio::runtime::Runtime) -> Fixture {
    let project = TempDir::new().expect("temp project dir");
    write_big_fixture(project.path());
    let spelunk_dir = project.path().join(".spelunk");
    std::fs::create_dir_all(&spelunk_dir).expect("create .spelunk dir");
    let db = spelunk_dir.join("index.db");

    let server = rt.block_on(dead_llm_server());
    let uri = server.uri();
    std::fs::write(
        spelunk_dir.join("config.toml"),
        format!(
            "db_path = {:?}\napi_base_url = {:?}\n\
             llm_model = \"test-chat\"\nserver_url = {:?}\nproject_id = \"test-org/test-project\"\n",
            db, uri, uri,
        ),
    )
    .expect("write config");

    Fixture {
        project_path: project.path().to_path_buf(),
        _project: project,
        server,
        db,
        spelunk_dir,
    }
}

/// The headline: on the path a real repo takes, a failed summary pass must be
/// discoverable. The parent points at the log; the log carries the warning.
#[test]
fn background_spawn_routes_diagnostics_to_log_and_points_at_it() {
    let rt = tokio::runtime::Runtime::new().expect("build test runtime");
    let f = fixture(&rt);

    let out = index_command(&f.project_path)
        .output()
        .expect("run spelunk index");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        out.status.success(),
        "diagnostics are best-effort: a dead LLM must never fail the index ({}):\n{stderr}",
        out.status
    );
    assert!(
        stderr.contains("Spawning background job"),
        "fixture must be over the threshold that selects the background spawn:\n{stderr}"
    );

    let log = log_path(&f.spelunk_dir);
    // The path is printed as given, so it is relative when the path argument was.
    assert!(
        stderr.contains("Log: ") && stderr.contains("index-background.log"),
        "the spawning command must name the log, or the diagnostics are undiscoverable:\n{stderr}"
    );

    let contents = wait_for_log_containing(&log, "produced no summary");
    assert!(
        contents.contains("summary batch(es) produced no summary"),
        "the failure warning must reach the log:\n{contents}"
    );
    assert!(
        contents.contains("--force"),
        "the warning must keep naming the remedy that actually retries:\n{contents}"
    );
    // The warning's own remedy text promises RUST_LOG shows the cause, which is
    // only true if the child's stderr goes somewhere.
    assert!(
        !stderr.contains("produced no summary"),
        "the detached child's warning must not reach the parent's stderr:\n{stderr}"
    );
    drop(f.server);
}

/// Every diagnostic the child emits is captured, not just the summary warning:
/// the fix is scoped to the path, not to one message.
#[test]
fn background_log_captures_all_child_phases_not_just_summaries() {
    let rt = tokio::runtime::Runtime::new().expect("build test runtime");
    let f = fixture(&rt);

    index_command(&f.project_path)
        .output()
        .expect("run spelunk index");

    let contents = wait_for_log_containing(&log_path(&f.spelunk_dir), "Conventions:");
    for phase in [
        "Computing graph rank",
        "Generating summaries",
        "Extracting conventions",
    ] {
        assert!(
            contents.contains(phase),
            "the log must capture {phase:?}, not only the summary warning:\n{contents}"
        );
    }
    drop(f.server);
}

/// The log is a full-content write per run, so a repo indexed daily cannot grow
/// an unbounded diagnostics file.
#[test]
fn background_log_is_truncated_per_run_not_appended() {
    let rt = tokio::runtime::Runtime::new().expect("build test runtime");
    let f = fixture(&rt);
    let log = log_path(&f.spelunk_dir);

    let mut sizes = Vec::new();
    for _ in 0..2 {
        index_command(&f.project_path)
            .arg("--force")
            .output()
            .expect("run spelunk index");
        wait_for_log_containing(&log, "produced no summary");
        wait_for_child_to_settle(&log);
        let contents = std::fs::read_to_string(&log).expect("read log");
        assert_eq!(
            contents.matches("produced no summary").count(),
            1,
            "each run must replace the log, not append to it:\n{contents}"
        );
        sizes.push(contents.len());
    }
    assert_eq!(
        sizes[0], sizes[1],
        "an identical re-run must not grow the log: {sizes:?}"
    );
    drop(f.server);
}

/// Best-effort contract: an unopenable log degrades to silence, never to a
/// failed index. `exit 0` here is relied on for git-hook use.
#[test]
fn index_still_succeeds_when_the_log_cannot_be_opened() {
    let rt = tokio::runtime::Runtime::new().expect("build test runtime");
    let f = fixture(&rt);

    // A directory at the log path: the open fails, nothing can be written.
    std::fs::create_dir_all(log_path(&f.spelunk_dir)).expect("create dir at log path");

    let out = index_command(&f.project_path)
        .output()
        .expect("run spelunk index");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        out.status.success(),
        "an unopenable log must not fail the index ({}):\n{stderr}",
        out.status
    );
    assert!(
        !stderr.contains("Log:"),
        "no log pointer may be printed when no log was opened:\n{stderr}"
    );

    wait_for_child_via_db(&f.db);
    let conn = rusqlite::Connection::open(&f.db).expect("open db");
    let chunks: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
        .expect("count chunks");
    assert!(chunks > 0, "the index must still have been built");
    drop(f.server);
}

/// The log lives at a fixed, predictable path inside the repo. A symlink
/// planted there must not turn `spelunk index` into an overwrite primitive.
#[cfg(unix)]
#[test]
fn a_symlink_at_the_log_path_is_refused_and_does_not_fail_the_index() {
    let rt = tokio::runtime::Runtime::new().expect("build test runtime");
    let f = fixture(&rt);

    let victim = f.spelunk_dir.join("victim.txt");
    std::fs::write(&victim, "ORIGINAL").expect("write victim");
    std::os::unix::fs::symlink(&victim, log_path(&f.spelunk_dir)).expect("plant symlink");

    let out = index_command(&f.project_path)
        .output()
        .expect("run spelunk index");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        out.status.success(),
        "a refused log open must not fail the index ({}):\n{stderr}",
        out.status
    );
    wait_for_child_via_db(&f.db);
    assert_eq!(
        std::fs::read_to_string(&victim).expect("read victim"),
        "ORIGINAL",
        "O_NOFOLLOW must stop the symlink target being written through"
    );
    assert!(
        !stderr.contains("Log:"),
        "no log pointer may be printed when the open was refused:\n{stderr}"
    );
    drop(f.server);
}

/// The other detached child. `--detach-embed` runs the same phases (and so the
/// same summary pass) in a subprocess whose stderr was closed identically.
///
/// Routing alone is not enough: a log nobody is told about leaves this path's
/// output identical to the bug, which is what "written and reaches nobody"
/// means. The pointer is as much the fix as the redirect.
#[test]
fn detached_embed_child_routes_diagnostics_to_the_log_and_points_at_it() {
    let rt = tokio::runtime::Runtime::new().expect("build test runtime");
    let f = fixture(&rt);

    let out = index_command(&f.project_path)
        .arg("--detach-embed")
        .output()
        .expect("run spelunk index --detach-embed");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "--detach-embed must not fail the index ({})",
        out.status
    );
    assert!(
        stdout.contains("Log: ") && stdout.contains("index-background.log"),
        "the detached embed child's log must be named, or its diagnostics are \
         undiscoverable:\n{stdout}"
    );
    // The pointer belongs after the status line, matching the other path's
    // parent-then-indented-child shape.
    let status_line = stdout
        .find("Run `spelunk status`")
        .expect("embed path must keep its status line");
    let pointer = stdout.find("Log: ").expect("pointer present");
    assert!(
        status_line < pointer,
        "the log pointer must follow the status line, not precede it:\n{stdout}"
    );

    let contents = wait_for_log_containing(&log_path(&f.spelunk_dir), "produced no summary");
    assert!(
        contents.contains("summary batch(es) produced no summary"),
        "the detached embed child's warning must reach the log too:\n{contents}"
    );
    drop(f.server);
}

/// The pointer is conditional on a log having actually opened, so the embed
/// path must stay silent rather than name a log that does not exist.
#[test]
fn detached_embed_prints_no_pointer_when_the_log_cannot_be_opened() {
    let rt = tokio::runtime::Runtime::new().expect("build test runtime");
    let f = fixture(&rt);

    std::fs::create_dir_all(log_path(&f.spelunk_dir)).expect("create dir at log path");

    let out = index_command(&f.project_path)
        .arg("--detach-embed")
        .output()
        .expect("run spelunk index --detach-embed");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();

    assert!(
        out.status.success(),
        "an unopenable log must not fail the index on the embed path ({})",
        out.status
    );
    assert!(
        !stdout.contains("Log:"),
        "no log may be named when none was opened:\n{stdout}"
    );
    assert!(
        stdout.contains("Run `spelunk status`"),
        "the embed path must still report normally:\n{stdout}"
    );

    wait_for_child_via_db(&f.db);
    let conn = rusqlite::Connection::open(&f.db).expect("open db");
    let chunks: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
        .expect("count chunks");
    assert!(chunks > 0, "the index must still have been built");
    drop(f.server);
}
