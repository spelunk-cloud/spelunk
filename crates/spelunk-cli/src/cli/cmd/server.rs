//! `spelunk server` subcommand — manage a local spelunk-server daemon.
//!
//! ## Subcommands
//!
//! - `spelunk server start`  — daemonise spelunk-server; write PID/port/log files.
//! - `spelunk server stop`   — terminate the running daemon (SIGTERM, then
//!   SIGKILL if it won't exit) and verify it is gone.
//! - `spelunk server status` — print PID, port, instance_id, and uptime.
//! - `spelunk server logs`   — print the last N lines from the server log.
//!
//! ## State directory
//!
//! All runtime state lives under `~/.local/state/spelunk/` (or
//! `SPELUNK_STATE_DIR` when set; see `capability::spelunk_state_dir`, the
//! single resolver every reader and writer of this directory shares):
//! - `server.pid`  — PID of the running daemon process
//! - `server.port`: TCP port the daemon is listening on (read by `capability/probe.rs`)
//! - `server.log`  — stdout + stderr of the daemon process
//!
//! The port file is read by `capability/probe.rs` for loopback auto-discovery
//! (spelunk#316).  The writer here **must** use the same path, enforced by
//! both going through the shared resolver rather than each defining their own.
//!
//! ## Spawned-binary resolution (PATH vs. sibling/absolute)
//!
//! `spelunk-server` is resolved preferring a path next to the running
//! `spelunk` executable, falling back to a `$PATH` walk only if no sibling
//! binary is found (see [`which_spelunk_server`]) — this avoids a
//! PATH/CWD-hijack where a malicious `spelunk-server` earlier on `$PATH`
//! (or in an untrusted repo's local tooling dir) gets executed instead of
//! the real one.
//!
//! Other external tools spawned elsewhere in the CLI (`git`, `gh`, `bun`,
//! `$EDITOR`, and `taskkill` on Windows — see `memory/add.rs`,
//! `memory/harvest.rs`, `memory/mod.rs`, and the `stop` command below) are
//! **not** given the same treatment: they are resolved via the bare name on
//! `$PATH` as is conventional for CLI-invoked developer tools (the same way
//! `git`, shell, and editor integrations normally work), and the user is
//! trusted to control their own `$PATH`. This is a deliberate scope
//! decision, not an oversight — `spelunk-server` is different because it is
//! a first-party binary spelunk itself ships and auto-spawns without the
//! user typing a command, so a bundled/co-located binary is both available
//! and the more trustworthy choice by default.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::color::cprintln;
use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use crate::capability::spelunk_state_dir;

// ── State dir helpers ─────────────────────────────────────────────────────────

fn pid_path(state_dir: &Path) -> PathBuf {
    state_dir.join("server.pid")
}
fn port_path(state_dir: &Path) -> PathBuf {
    state_dir.join("server.port")
}
fn log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("server.log")
}

/// Create `dir` (and parents) with `0700` permissions on Unix so only the
/// owner can read the PID/port/log files inside it. A no-op permission
/// tightening on platforms without Unix perms.
pub(super) fn create_state_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating state dir {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting 0700 permissions on {}", dir.display()))?;
    }
    Ok(())
}

/// Write `contents` to a state file, creating it `0600` and refusing to
/// follow an existing symlink at `path` (see
/// [`super::helpers::open_private_file_for_write`]).
pub(super) fn write_state_file(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    let mut f = super::helpers::open_private_file_for_write(path)?;
    f.write_all(contents.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Open a state file for daemon-log append, creating it `0600` and refusing
/// to follow an existing symlink at `path`.
fn open_log_file_for_append(path: &Path) -> Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(super::helpers::libc_o_nofollow())
            .open(path)
            .with_context(|| format!("opening {}", path.display()))
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))
    }
}

/// Read PID from the state file. Returns `None` if absent or unparseable.
fn read_pid(state_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_path(state_dir))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// Read port from the state file. Returns `None` if absent or unparseable.
fn read_port(state_dir: &Path) -> Option<u16> {
    std::fs::read_to_string(port_path(state_dir))
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
}

/// Return `true` when `pid` names a currently-running process.
pub(super) fn pid_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // kill(pid, 0) checks existence without sending a signal.
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let rc = unsafe { kill(pid as i32, 0) };
        rc == 0
    }
    #[cfg(windows)]
    {
        // OpenProcess with PROCESS_QUERY_LIMITED_INFORMATION is sufficient to
        // call GetExitCodeProcess.  A NULL handle means the process does not
        // exist (or we have no access — treated as "not alive").
        unsafe extern "system" {
            fn OpenProcess(desired_access: u32, inherit_handle: i32, pid: u32) -> *mut ();
            fn CloseHandle(handle: *mut ()) -> i32;
            fn GetExitCodeProcess(handle: *mut (), exit_code: *mut u32) -> i32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const STILL_ACTIVE: u32 = 259;
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
        unsafe { CloseHandle(handle) };
        ok != 0 && exit_code == STILL_ACTIVE
    }
    #[cfg(not(any(unix, windows)))]
    {
        // Unknown platform: conservatively return false so stale PIDs do not
        // block a fresh server start.
        let _ = pid;
        false
    }
}

// ── Process lifecycle helpers ──────────────────────────────────────────────────

/// Grace period for a `SIGTERM`ed daemon to exit before escalation.
const GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Extra window for the process to disappear after `SIGKILL` (Unix).
const FORCE_KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// `server.db-path` records the DB the running daemon was started against, so a
/// second `start` can refuse to point a new server at a different DB.
fn db_path_file(state_dir: &Path) -> PathBuf {
    state_dir.join("server.db-path")
}

/// Read the DB path recorded for the running daemon. `None` if absent/empty.
fn read_db_path(state_dir: &Path) -> Option<PathBuf> {
    std::fs::read_to_string(db_path_file(state_dir))
        .ok()
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty())
}

/// Best-effort path equality that tolerates symlinks / `.` / `..` by
/// canonicalising each side when it exists, falling back to the raw path.
fn same_path(a: &Path, b: &Path) -> bool {
    let ca = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

/// Return `true` when `pid`'s command line looks like a `spelunk-server`.
///
/// This is the identity signal used when `/v1/health` does *not* respond: a
/// wedged/hung daemon still exists as a `spelunk-server` process, so we can
/// safely terminate it, whereas a PID reused by an unrelated process after a
/// crash must not be killed. Uses `ps` (Unix) / `tasklist` (Windows).
fn process_matches_server(pid: u32) -> bool {
    #[cfg(unix)]
    {
        match std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "args="])
            .output()
        {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).contains("spelunk-server")
            }
            _ => false,
        }
    }
    #[cfg(windows)]
    {
        match std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
        {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .to_lowercase()
                .contains("spelunk-server"),
            _ => false,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Classification of a live PID recorded in the state dir.
enum RunningServer {
    /// `/v1/health` responded on the recorded port — a healthy daemon.
    Healthy { port: u16 },
    /// Alive and a `spelunk-server` process, but `/v1/health` is silent — our
    /// wedged daemon. Safe to terminate/reclaim.
    HungOurs,
    /// Alive but neither healthy nor a `spelunk-server` — the PID was almost
    /// certainly reused by an unrelated process after a crash. Do not signal it.
    Foreign,
}

/// Classify the live process `pid` recorded in `state_dir`.
///
/// Health probe first (definitive "ours + reachable"); on no response, fall
/// back to a process-command identity check so a *hung* daemon is still
/// recognised as ours and can be reclaimed — the previous health-only check
/// refused to stop a wedged server, which is the core bug this fixes.
async fn classify_running_server(state_dir: &Path, pid: u32) -> RunningServer {
    if let Some(port) = read_port(state_dir)
        && probe_health(port).await.is_some()
    {
        return RunningServer::Healthy { port };
    }
    if process_matches_server(pid) {
        return RunningServer::HungOurs;
    }
    RunningServer::Foreign
}

/// `SIGKILL` on Unix. Tolerates a process that already exited (`ESRCH`).
#[cfg(unix)]
fn force_kill(pid: u32) -> Result<()> {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    let rc = unsafe { kill(pid as i32, SIGKILL) };
    if rc != 0 && pid_is_alive(pid) {
        anyhow::bail!("kill({pid}, SIGKILL) failed");
    }
    Ok(())
}

/// Poll until `pid` is gone or `timeout` elapses. Returns `true` if gone.
async fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !pid_is_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    !pid_is_alive(pid)
}

/// Terminate `pid` and confirm it is gone. Returns `Ok(true)` only when the
/// process has actually exited.
///
/// Unix: `SIGTERM`, wait [`GRACEFUL_STOP_TIMEOUT`], then escalate to `SIGKILL`
/// and wait [`FORCE_KILL_TIMEOUT`]. Windows: `taskkill /F` (already forceful),
/// then wait. Never reports success on a still-running process.
async fn terminate_and_wait(pid: u32) -> Result<bool> {
    // Graceful signal. If it errored only because the process already exited
    // (a race between classify and here), treat that as success.
    if let Err(e) = terminate_process(pid) {
        if !pid_is_alive(pid) {
            return Ok(true);
        }
        return Err(e);
    }
    if wait_for_exit(pid, GRACEFUL_STOP_TIMEOUT).await {
        return Ok(true);
    }
    #[cfg(unix)]
    if pid_is_alive(pid) {
        force_kill(pid)?;
        if wait_for_exit(pid, FORCE_KILL_TIMEOUT).await {
            return Ok(true);
        }
    }
    Ok(!pid_is_alive(pid))
}

/// Held for the duration of a `start` sequence so two concurrent
/// `spelunk server start` invocations can't both spawn a daemon against the
/// same state dir / DB. The lock is advisory (`flock`, Unix) and releases when
/// the guard drops (the CLI process exits or `start` returns).
#[cfg(unix)]
struct StartLock {
    _file: std::fs::File,
}

/// Acquire the single-instance `start` lock. Fails fast if another start is in
/// progress. No-op guard on non-Unix platforms.
#[cfg(unix)]
fn acquire_start_lock(state_dir: &Path) -> Result<StartLock> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let path = state_dir.join("server.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;

    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if rc != 0 {
        anyhow::bail!(
            "another `spelunk server start` is already in progress for this machine. \
             Wait for it to finish, or check `spelunk server status`."
        );
    }
    Ok(StartLock { _file: file })
}

#[cfg(not(unix))]
struct StartLock;

#[cfg(not(unix))]
fn acquire_start_lock(_state_dir: &Path) -> Result<StartLock> {
    Ok(StartLock)
}

// ── CLI types ─────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct ServerArgs {
    #[command(subcommand)]
    pub command: ServerCommand,
}

#[derive(Subcommand, Debug)]
pub enum ServerCommand {
    /// Start a local spelunk-server daemon (idempotent)
    Start(ServerStartArgs),
    /// Stop the running local spelunk-server daemon
    Stop,
    /// Show status of the local spelunk-server daemon
    Status,
    /// Print the last N lines of the server log
    Logs(ServerLogsArgs),
}

#[derive(Args, Debug)]
pub struct ServerStartArgs {
    /// Port to bind (default 7777). Explicit `start` does not drift to another
    /// port: if this one is held by an unrelated process, start fails loudly.
    #[arg(long, default_value = "7777")]
    pub port: u16,

    /// Path to the spelunk-server binary (default: the `spelunk-server` in PATH)
    #[arg(long)]
    pub bin: Option<PathBuf>,

    /// Path to the server SQLite database (default: ~/.local/state/spelunk/server.db)
    #[arg(long)]
    pub db: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ServerLogsArgs {
    /// Number of lines to show (default: 50)
    #[arg(short = 'n', long, default_value = "50")]
    pub lines: usize,
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

pub async fn server(args: ServerArgs) -> Result<()> {
    match args.command {
        ServerCommand::Start(a) => cmd_start(a).await,
        ServerCommand::Stop => cmd_stop().await,
        ServerCommand::Status => cmd_status().await,
        ServerCommand::Logs(a) => cmd_logs(a),
    }
}

// ── Public bootstrap API ──────────────────────────────────────────────────────

/// Probe for an already-running local spelunk-server daemon (the one
/// `spelunk server start`/[`ensure_server_running`] manages), without
/// starting one. Returns its port if `/v1/health` responds.
///
/// This is the non-starting half of ADR-037 P2's D6 auto-start gate: a
/// `local_first` write nudges the reconciler only if this returns `Some`, or
/// (when interactive) after first calling [`ensure_server_running`] itself —
/// this function never spawns anything on its own.
pub(crate) async fn probe_local_relay_port() -> Option<u16> {
    let state_dir = spelunk_state_dir().ok()?;
    let port = read_port(&state_dir)?;
    probe_health(port).await?;
    Some(port)
}

/// Ensure a local spelunk-server is running.
///
/// Returns `(port, freshly_started)`. Idempotent: if the server is already
/// healthy, returns immediately with `freshly_started = false`.
///
/// Called by `spelunk init` to auto-spawn the server when running interactively.
pub async fn ensure_server_running(start_port: u16) -> Result<(u16, bool)> {
    let state_dir = spelunk_state_dir()?;
    create_state_dir(&state_dir)?;

    // Serialise against a concurrent `server start` so we don't race two
    // daemons onto the same DB.
    let _start_lock = acquire_start_lock(&state_dir)?;

    // Inspect any recorded daemon before spawning. A wedged ("hung") daemon
    // must be reclaimed, not left running while we bind a *different* port —
    // that leaves two servers on one DB (the leaked-process + port-drift bug).
    if let Some(pid) = read_pid(&state_dir)
        && pid_is_alive(pid)
    {
        match classify_running_server(&state_dir, pid).await {
            RunningServer::Healthy { port } => return Ok((port, false)),
            RunningServer::HungOurs => {
                tracing::warn!("reclaiming unresponsive spelunk-server (pid={pid}) before restart");
                let _ = terminate_and_wait(pid).await;
                cleanup_state_files(&state_dir);
            }
            RunningServer::Foreign => {
                // PID reused by an unrelated process; recorded state is stale.
                cleanup_state_files(&state_dir);
            }
        }
    }

    let bin = which_spelunk_server()?;
    let db = state_dir.join("server.db");
    const PORT_RANGE: u16 = 11;
    let port = find_available_port(start_port, PORT_RANGE)?;

    let log_file = open_log_file_for_append(&log_path(&state_dir))?;

    #[cfg(unix)]
    let child = spawn_daemon_unix(&bin, &db, port, log_file)?;
    #[cfg(windows)]
    let child = spawn_daemon_windows(&bin, &db, port, log_file)?;

    let pid = child.id();
    write_state_file(&pid_path(&state_dir), &format!("{pid}\n")).context("writing server.pid")?;
    write_state_file(&port_path(&state_dir), &format!("{port}\n"))
        .context("writing server.port")?;
    write_state_file(&db_path_file(&state_dir), &format!("{}\n", db.display()))
        .context("writing server.db-path")?;

    // Wait for *liveness* (the port binds, /v1/health responds) — not model
    // readiness. Health now goes live at bind, before the model download, so
    // 30 s comfortably covers a cold listener bind even on Windows; it only
    // bounds the give-up time and is free in the happy path (200 ms poll,
    // returns on first success).
    let ready = wait_for_health(port, Duration::from_secs(30)).await;
    if !ready {
        // Liveness genuinely not achieved within the timeout — most commonly a
        // firewall blocking the loopback listener. Don't warn merely because the
        // model is still loading (health is live before that).
        tracing::warn!(
            "spelunk-server started (pid={pid}) but /v1/health did not respond within 30 s. \
             A firewall may be blocking the local server (allow it, e.g. accept the Windows \
             Defender Firewall prompt), or the process failed to start — check \
             `spelunk server logs`."
        );
    }

    Ok((port, true))
}

// ── start ─────────────────────────────────────────────────────────────────────

async fn cmd_start(args: ServerStartArgs) -> Result<()> {
    let state_dir = spelunk_state_dir()?;
    create_state_dir(&state_dir)?;

    // Single-instance guard: block a concurrent `server start` from racing us
    // into a second daemon on the same DB.
    let _start_lock = acquire_start_lock(&state_dir)?;

    // ── Default DB path ──────────────────────────────────────────────────────
    let db = args
        .db
        .clone()
        .unwrap_or_else(|| state_dir.join("server.db"));

    // ── Reclaim / idempotency ────────────────────────────────────────────────
    // The previous code fell through to `find_available_port` whenever a
    // recorded PID was alive-but-unhealthy, silently binding a *new* port and
    // leaving the wedged daemon holding the old one — two servers on one DB.
    // Instead: return early if healthy, reclaim if wedged, clear stale state.
    if let Some(pid) = read_pid(&state_dir) {
        if pid_is_alive(pid) {
            match classify_running_server(&state_dir, pid).await {
                RunningServer::Healthy { port } => {
                    // Refuse to start a second server against a *different* DB —
                    // the single state dir tracks one daemon; clobbering it would
                    // orphan the running one.
                    if let Some(running_db) = read_db_path(&state_dir)
                        && !same_path(&running_db, &db)
                    {
                        anyhow::bail!(
                            "a spelunk-server is already running (pid={pid}, port={port}) against \
                             {}. Stop it first with `spelunk server stop` before starting one \
                             against {}.",
                            running_db.display(),
                            db.display()
                        );
                    }
                    println!("spelunk-server is already running (pid={pid}, port={port}).");
                    return Ok(());
                }
                RunningServer::HungOurs => {
                    println!("Reclaiming unresponsive spelunk-server (pid={pid})...");
                    if !terminate_and_wait(pid).await? {
                        anyhow::bail!(
                            "could not stop the unresponsive spelunk-server (pid={pid}); it \
                             survived SIGTERM and SIGKILL. Kill it manually and retry."
                        );
                    }
                    cleanup_state_files(&state_dir);
                }
                RunningServer::Foreign => {
                    tracing::warn!(
                        "recorded pid={pid} is not a spelunk-server (PID reused); clearing stale state"
                    );
                    cleanup_state_files(&state_dir);
                }
            }
        } else {
            // Dead PID — clear stale state before starting fresh.
            cleanup_state_files(&state_dir);
        }
    }

    // ── Find the binary ──────────────────────────────────────────────────────
    let bin = match &args.bin {
        Some(p) => {
            if !p.exists() {
                anyhow::bail!("spelunk-server binary not found at {}", p.display());
            }
            p.clone()
        }
        None => which_spelunk_server()?,
    };

    // ── Port (no silent drift) ───────────────────────────────────────────────
    // Any wedged daemon of ours was reclaimed above, freeing its port. If the
    // requested port is still occupied, it belongs to an unrelated process —
    // fail loudly rather than binding elsewhere.
    let port = args.port;
    ensure_port_available_for_start(port).await?;

    // ── Spawn daemonised process ─────────────────────────────────────────────
    let log_file = open_log_file_for_append(&log_path(&state_dir))?;

    #[cfg(unix)]
    let child = spawn_daemon_unix(&bin, &db, port, log_file)?;
    #[cfg(windows)]
    let child = spawn_daemon_windows(&bin, &db, port, log_file)?;

    let pid = child.id();

    // Write state files.
    write_state_file(&pid_path(&state_dir), &format!("{pid}\n")).context("writing server.pid")?;
    write_state_file(&port_path(&state_dir), &format!("{port}\n"))
        .context("writing server.port")?;
    write_state_file(&db_path_file(&state_dir), &format!("{}\n", db.display()))
        .context("writing server.db-path")?;

    // Wait up to 30 s for the server to become reachable (liveness, not model
    // readiness — /v1/health is live at bind, before any model download).
    let ready = wait_for_health(port, Duration::from_secs(30)).await;
    if ready {
        println!("spelunk-server started (pid={pid}, port={port}).");
        println!("  Log: {}", log_path(&state_dir).display());
    } else {
        // Fires only on genuine liveness-timeout — typically a firewall blocking
        // the loopback listener, or a process that failed to start.
        eprintln!(
            "warning: spelunk-server process started (pid={pid}) but /v1/health did not \
             respond on port {port} within 30 s. A firewall may be blocking the local \
             server (allow it, e.g. accept the Windows Defender Firewall prompt), or the \
             process failed to start. Check the log: {}",
            log_path(&state_dir).display()
        );
    }

    Ok(())
}

/// Locate the `spelunk-server` binary.
///
/// Priority: next to the current executable → PATH.
fn which_spelunk_server() -> Result<PathBuf> {
    // On Windows executables carry a `.exe` suffix; on Unix there is no suffix.
    #[cfg(windows)]
    let bin_name = "spelunk-server.exe";
    #[cfg(not(windows))]
    let bin_name = "spelunk-server";

    // 1. Same directory as the running `spelunk` binary.
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(bin_name);
        if sibling.exists() {
            return Ok(sibling);
        }
    }

    // 2. PATH lookup.
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(bin_name))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "spelunk-server binary not found. \
                 Install it alongside `spelunk` or pass --bin <path>."
            )
        })
}

/// Verify the requested `start` port is bindable, failing loudly if not.
///
/// Explicit `server start` never drifts to a different port (a silent drift is
/// what leaves a stale daemon on the old port and a new one elsewhere). A short
/// bounded retry absorbs the brief window after reclaiming our own daemon while
/// the OS releases its listening socket.
async fn ensure_port_available_for_start(port: u16) -> Result<()> {
    for attempt in 0..10 {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(());
        }
        if attempt < 9 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    anyhow::bail!(
        "port {port} is already in use by another process. If it is a spelunk-server not \
         managed here, stop it; otherwise free the port or pass `--port <N>`."
    );
}

/// Walk ports `start..start+range` to find the first unbound one.
fn find_available_port(start: u16, range: u16) -> Result<u16> {
    for offset in 0..range {
        let port = start.saturating_add(offset);
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    anyhow::bail!(
        "No free port found in {}–{}.  Stop another service or pass --port.",
        start,
        start.saturating_add(range - 1),
    )
}

/// Build the argument list passed to `spelunk-server` when auto-spawning the daemon.
///
/// Extracted from the spawn helpers so that unit tests can verify the args
/// without actually launching a process.
///
/// The returned `Vec` contains every argument **after** the binary path, in
/// order, as it would be appended to `std::process::Command`.
pub(super) fn build_daemon_args(db: &Path, port: u16) -> Vec<std::ffi::OsString> {
    vec![
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string().into(),
        "--db".into(),
        db.as_os_str().into(),
    ]
}

/// Spawn the server on Unix.
///
/// Uses a single `fork`+`exec` via `std::process::Command::spawn()`.  The
/// child process inherits the log file handles and runs independently; the
/// CLI process exits after writing the PID/port state files, at which point
/// the child is reparented to init/launchd and becomes fully detached.
///
/// `--host 127.0.0.1` is always passed explicitly. The auto-spawned daemon is
/// unauthenticated, so it must only ever bind loopback; passing the flag keeps
/// that true regardless of spelunk-server's own default.
#[cfg(unix)]
fn spawn_daemon_unix(
    bin: &Path,
    db: &Path,
    port: u16,
    log_file: std::fs::File,
) -> Result<std::process::Child> {
    let log_file_err = log_file.try_clone().context("cloning log file handle")?;

    let mut cmd = std::process::Command::new(bin);
    for arg in build_daemon_args(db, port) {
        cmd.arg(arg);
    }
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(log_file)
        .stderr(log_file_err)
        .spawn()
        .with_context(|| format!("spawning {}", bin.display()))?;

    Ok(child)
}

/// Spawn the server on Windows with `CREATE_NEW_PROCESS_GROUP`.
///
/// `--host 127.0.0.1` is always passed explicitly. The auto-spawned daemon is
/// unauthenticated, so it must only ever bind loopback; passing the flag keeps
/// that true regardless of spelunk-server's own default.
#[cfg(windows)]
fn spawn_daemon_windows(
    bin: &Path,
    db: &Path,
    port: u16,
    log_file: std::fs::File,
) -> Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    let mut cmd = std::process::Command::new(bin);
    for arg in build_daemon_args(db, port) {
        cmd.arg(arg);
    }
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .with_context(|| format!("spawning {}", bin.display()))?;

    Ok(child)
}

/// Poll `GET http://127.0.0.1:{port}/v1/health` until it responds or timeout.
async fn wait_for_health(port: u16, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if probe_health(port).await.is_some() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// Single non-retrying health probe. Returns the `instance_id` on success.
async fn probe_health(port: u16) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .ok()?;
    let url = format!("http://127.0.0.1:{port}/v1/health");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    #[derive(serde::Deserialize)]
    struct H {
        instance_id: Option<String>,
    }
    let body: H = resp.json().await.ok()?;
    body.instance_id.or_else(|| Some("unknown".into()))
}

// ── stop ──────────────────────────────────────────────────────────────────────

async fn cmd_stop() -> Result<()> {
    let state_dir = spelunk_state_dir()?;
    let pid = read_pid(&state_dir)
        .ok_or_else(|| anyhow::anyhow!("no server.pid found — is spelunk-server running?"))?;

    if !pid_is_alive(pid) {
        println!("spelunk-server (pid={pid}) is not running. Cleaning up state files.");
        cleanup_state_files(&state_dir);
        return Ok(());
    }

    // ── Identity check ───────────────────────────────────────────────────────
    // A liveness check alone is not enough: PIDs are reused, so after a
    // crash/reboot the recorded PID may belong to an unrelated process. But a
    // *health*-only check (the previous behaviour) is too strict — it refused
    // to stop a wedged daemon whose `/v1/health` had stopped responding, which
    // is exactly the hang this command must handle. Classify instead: a healthy
    // *or* a hung-but-still-`spelunk-server` process is ours to kill; only a
    // truly foreign process is refused.
    match classify_running_server(&state_dir, pid).await {
        RunningServer::Healthy { .. } | RunningServer::HungOurs => {}
        RunningServer::Foreign => {
            anyhow::bail!(
                "refusing to stop pid={pid}: it does not look like the spelunk-server recorded \
                 in {}. If the server crashed and this PID was reused by an unrelated process, \
                 remove the stale state files manually (under {}) and retry.",
                pid_path(&state_dir).display(),
                state_dir.display()
            );
        }
    }

    // Terminate (SIGTERM → SIGKILL on Unix) and confirm the process is gone
    // before reporting success — never claim a stop that didn't happen.
    if terminate_and_wait(pid).await? {
        println!("spelunk-server stopped.");
        cleanup_state_files(&state_dir);
        Ok(())
    } else {
        anyhow::bail!(
            "spelunk-server (pid={pid}) is still running after SIGTERM and SIGKILL. State files \
             left in place; retry `spelunk server stop` or kill the process manually."
        );
    }
}

fn terminate_process(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGTERM: i32 = 15;
        let rc = unsafe { kill(pid as i32, SIGTERM) };
        if rc != 0 {
            anyhow::bail!("kill({pid}, SIGTERM) failed");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        // On Windows, use taskkill.
        let status = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status()
            .context("running taskkill")?;
        if !status.success() {
            anyhow::bail!("taskkill /PID {pid} /F failed");
        }
        Ok(())
    }
}

fn cleanup_state_files(state_dir: &Path) {
    let _ = std::fs::remove_file(pid_path(state_dir));
    let _ = std::fs::remove_file(port_path(state_dir));
    let _ = std::fs::remove_file(db_path_file(state_dir));
}

// ── status ────────────────────────────────────────────────────────────────────

async fn cmd_status() -> Result<()> {
    let state_dir = spelunk_state_dir()?;
    let pid = read_pid(&state_dir);
    let port = read_port(&state_dir);

    match (pid, port) {
        (Some(pid), Some(port)) if pid_is_alive(pid) => {
            cprintln!("spelunk-server  \x1b[32mrunning\x1b[0m");
            println!("  PID:   {pid}");
            println!("  Port:  {port}");
            println!("  Log:   {}", log_path(&state_dir).display());

            // Fetch extended info from /v1/health.
            match probe_health_verbose(port).await {
                Some(info) => {
                    println!("  URL:   http://127.0.0.1:{port}");
                    if let Some(id) = info.instance_id {
                        println!("  ID:    {id}");
                    }
                    if let Some(ver) = info.version {
                        println!("  Ver:   {ver}");
                    }
                }
                None => {
                    cprintln!("  URL:   http://127.0.0.1:{port}  \x1b[31m(unreachable)\x1b[0m");
                }
            }
        }
        (Some(pid), _) if pid_is_alive(pid) => {
            cprintln!("spelunk-server  \x1b[33mrunning\x1b[0m (port unknown)");
            println!("  PID: {pid}");
        }
        (Some(pid), _) => {
            cprintln!("spelunk-server  \x1b[31mstopped\x1b[0m (stale pid={pid})");
            println!("  Run `spelunk server start` to start.");
        }
        (None, _) => {
            cprintln!("spelunk-server  \x1b[31mnot started\x1b[0m");
            println!("  Run `spelunk server start` to start.");
        }
    }
    Ok(())
}

struct HealthInfo {
    instance_id: Option<String>,
    version: Option<String>,
}

async fn probe_health_verbose(port: u16) -> Option<HealthInfo> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()?;
    let url = format!("http://127.0.0.1:{port}/v1/health");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    #[derive(serde::Deserialize)]
    struct H {
        instance_id: Option<String>,
        version: Option<String>,
    }
    let body: H = resp.json().await.ok()?;
    Some(HealthInfo {
        instance_id: body.instance_id,
        version: body.version,
    })
}

// ── logs ──────────────────────────────────────────────────────────────────────

fn cmd_logs(args: ServerLogsArgs) -> Result<()> {
    let state_dir = spelunk_state_dir()?;
    let log = log_path(&state_dir);

    if !log.exists() {
        anyhow::bail!(
            "No log file at {}. Start the server first with `spelunk server start`.",
            log.display()
        );
    }

    let content =
        std::fs::read_to_string(&log).with_context(|| format!("reading {}", log.display()))?;

    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(args.lines);
    for line in &lines[start..] {
        println!("{line}");
    }

    Ok(())
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    // ── spelunk_state_dir ────────────────────────────────────────────────────

    #[test]
    #[serial(server_state_dir_env)]
    fn state_dir_contains_spelunk() {
        let dir = spelunk_state_dir().expect("state dir");
        assert!(
            dir.to_string_lossy().contains("spelunk"),
            "state dir should contain 'spelunk', got {dir:?}"
        );
    }

    // ── find_available_port ──────────────────────────────────────────────────

    #[test]
    fn find_available_port_succeeds() {
        // Port 0 triggers OS assignment; we use a high ephemeral range that is
        // very likely free in CI.
        let port = find_available_port(19700, 20).expect("should find a free port");
        assert!((19700..19720).contains(&port));
    }

    #[test]
    fn find_available_port_fails_when_all_bound() {
        // Bind to every port in a tiny range, then verify we get an error.
        let range: u16 = 3;
        let start: u16 = 19750;
        let _listeners: Vec<std::net::TcpListener> = (start..start + range)
            .filter_map(|p| std::net::TcpListener::bind(("127.0.0.1", p)).ok())
            .collect();
        // Only error if we actually managed to bind all three.
        if _listeners.len() == range as usize {
            assert!(
                find_available_port(start, range).is_err(),
                "expected error when all ports are bound"
            );
        }
    }

    // ── pid_is_alive ─────────────────────────────────────────────────────────

    #[test]
    fn current_process_is_alive() {
        let pid = std::process::id();
        assert!(pid_is_alive(pid), "current process should be alive");
    }

    // ── read_pid / read_port ─────────────────────────────────────────────────

    #[test]
    fn read_pid_returns_none_for_missing_file() {
        let tmp = TempDir::new().unwrap();
        assert!(read_pid(tmp.path()).is_none());
    }

    #[test]
    fn read_pid_round_trips() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(pid_path(tmp.path()), b"12345\n").unwrap();
        assert_eq!(read_pid(tmp.path()), Some(12345));
    }

    #[test]
    fn read_port_round_trips() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(port_path(tmp.path()), b"7777\n").unwrap();
        assert_eq!(read_port(tmp.path()), Some(7777));
    }

    // ── which_spelunk_server ─────────────────────────────────────────────────

    /// Restores the `PATH` env var to its captured value when dropped, so a
    /// panic mid-test cannot leak a mutated `PATH` into other tests.
    struct PathGuard(std::ffi::OsString);

    impl PathGuard {
        /// Capture the current `PATH` so it can be restored on drop.
        fn capture() -> Self {
            PathGuard(std::env::var_os("PATH").unwrap_or_default())
        }
    }

    impl Drop for PathGuard {
        fn drop(&mut self) {
            // SAFETY: the `#[serial(path_env)]` attribute guarantees no other
            // test that reads or writes `PATH` runs concurrently.
            unsafe { std::env::set_var("PATH", &self.0) };
        }
    }

    // NOTE: both `which_spelunk_server_*` tests mutate the process-global `PATH`,
    // including setting it to "" entirely. Cargo runs unit tests multi-threaded
    // by default, so they are pinned to the `path_env` serial group, along with
    // every test that spawns a `DummyProc::graceful()`/`ignores_sigterm()`
    // subprocess: those resolve the bare command name "sleep" via PATH, so an
    // empty PATH from a concurrently-running sibling makes the spawn itself
    // fail with ENOENT, not just the assertion under test.

    #[test]
    #[serial(path_env)]
    fn which_spelunk_server_finds_sibling_binary() {
        // Create a fake `spelunk-server[.exe]` next to the current executable.
        let tmp = TempDir::new().unwrap();
        // On Windows the binary must have the .exe extension to be recognised
        // as a file by the PATH search in `which_spelunk_server`.
        #[cfg(windows)]
        let fake_bin = tmp.path().join("spelunk-server.exe");
        #[cfg(not(windows))]
        let fake_bin = tmp.path().join("spelunk-server");
        std::fs::write(&fake_bin, b"").unwrap();

        // Temporarily redirect PATH so only our fake bin is discoverable and
        // pretend current_exe lives in tmp.
        //
        // We can't override current_exe() at runtime, so just verify the PATH
        // fallback path: put tmp on PATH and confirm discovery succeeds.
        //
        // SAFETY: `#[serial(path_env)]` serialises this test against every other
        // PATH-mutating test, so no other thread reads or writes PATH while this
        // runs. The `PathGuard` restores PATH even if the assertion below panics.
        let _guard = PathGuard::capture();
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        // Use the platform PATH separator (`;` on Windows, `:` on Unix).
        #[cfg(windows)]
        let new_path = format!("{};{}", tmp.path().display(), old_path.to_string_lossy());
        #[cfg(not(windows))]
        let new_path = format!("{}:{}", tmp.path().display(), old_path.to_string_lossy());
        unsafe { std::env::set_var("PATH", &new_path) };
        let result = which_spelunk_server();

        assert!(result.is_ok(), "should discover binary on PATH: {result:?}");
    }

    #[test]
    #[serial(path_env)]
    fn which_spelunk_server_fails_when_not_on_path() {
        // SAFETY: see note in which_spelunk_server_finds_sibling_binary; the
        // `#[serial(path_env)]` group serialises this against the sibling test,
        // and the `PathGuard` restores PATH even if the assertion panics.
        let _guard = PathGuard::capture();
        unsafe { std::env::set_var("PATH", "") };
        let result = which_spelunk_server();
        assert!(result.is_err(), "should fail when binary is not on PATH");
    }

    // ── spawn_daemon arg list: loopback-only bind ────────────────────────────
    //
    // Security invariant: the auto-spawned spelunk-server daemon is
    // unauthenticated, so it MUST only ever bind the loopback interface.
    //
    // These tests pin the arg list produced by `build_daemon_args` — the
    // single source of truth for both the Unix and Windows spawn helpers —
    // so that a future refactor cannot silently drop the flag.

    /// `--host 127.0.0.1` must appear in the daemon arg list.
    #[test]
    fn spawn_daemon_args_bind_loopback() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("test.db");
        let args = build_daemon_args(&db, 7777);

        // Collect as strings for readable assertions.
        let args_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        // `--host` flag must be present.
        assert!(
            args_str.contains(&"--host".to_string()),
            "daemon must bind loopback: --host flag missing from daemon args: {args_str:?}"
        );

        // The value immediately following `--host` must be `127.0.0.1`.
        let host_idx = args_str
            .iter()
            .position(|a| a == "--host")
            .expect("--host must be present");
        let host_value = args_str
            .get(host_idx + 1)
            .expect("--host must be followed by a value");
        assert_eq!(
            host_value, "127.0.0.1",
            "daemon must bind 127.0.0.1 only, got {host_value:?}"
        );
    }

    /// `0.0.0.0` must NOT appear in the daemon arg list.
    #[test]
    fn spawn_daemon_args_do_not_bind_wildcard() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("test.db");
        let args = build_daemon_args(&db, 7777);

        let args_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert!(
            !args_str.contains(&"0.0.0.0".to_string()),
            "daemon args must not contain 0.0.0.0 (wildcard bind): {args_str:?}"
        );
    }

    /// `--port` and the supplied port value must appear in the daemon arg list.
    #[test]
    fn spawn_daemon_args_include_port() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("test.db");
        let port: u16 = 7780;
        let args = build_daemon_args(&db, port);

        let args_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        let port_idx = args_str
            .iter()
            .position(|a| a == "--port")
            .expect("--port must be present in daemon args");
        let port_value = args_str
            .get(port_idx + 1)
            .expect("--port must be followed by a value");
        assert_eq!(
            port_value,
            &port.to_string(),
            "daemon arg --port value should match requested port"
        );
    }

    /// `--db` and the supplied path must appear in the daemon arg list.
    #[test]
    fn spawn_daemon_args_include_db_path() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("server.db");
        let args = build_daemon_args(&db, 7777);

        let args_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        let db_idx = args_str
            .iter()
            .position(|a| a == "--db")
            .expect("--db must be present in daemon args");
        let db_value = args_str
            .get(db_idx + 1)
            .expect("--db must be followed by a value");
        assert_eq!(
            db_value,
            &db.to_string_lossy().into_owned(),
            "daemon arg --db value should match supplied db path"
        );
    }

    // ── probe_local_relay_port: non-starting local-daemon detection (D6) ─────

    /// Restores `SPELUNK_STATE_DIR` on drop, so a panic mid-test can't leak a
    /// mutated env var into other tests. Mirrors `PathGuard` above.
    struct StateDirGuard(Option<std::ffi::OsString>);
    impl StateDirGuard {
        fn set(dir: &Path) -> Self {
            let prev = std::env::var_os("SPELUNK_STATE_DIR");
            unsafe { std::env::set_var("SPELUNK_STATE_DIR", dir) };
            Self(prev)
        }
    }
    impl Drop for StateDirGuard {
        fn drop(&mut self) {
            // SAFETY: `#[serial(server_state_dir_env)]` on every test using
            // this guard serialises against all others touching the var.
            unsafe {
                match &self.0 {
                    Some(v) => std::env::set_var("SPELUNK_STATE_DIR", v),
                    None => std::env::remove_var("SPELUNK_STATE_DIR"),
                }
            }
        }
    }

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn probe_local_relay_port_none_when_no_state_dir_at_all() {
        let tmp = TempDir::new().unwrap();
        let _guard = StateDirGuard::set(&tmp.path().join("nonexistent"));
        // No port file written at all: must return None without any network call.
        assert_eq!(probe_local_relay_port().await, None);
    }

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn probe_local_relay_port_none_when_port_file_present_but_unhealthy() {
        let tmp = TempDir::new().unwrap();
        let _guard = StateDirGuard::set(tmp.path());
        // A stale port file (nothing listening) must not be reported as running.
        std::fs::write(port_path(tmp.path()), b"19999\n").unwrap();
        assert_eq!(probe_local_relay_port().await, None);
    }

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn probe_local_relay_port_some_when_health_responds() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"instance_id": "x"})),
            )
            .mount(&server)
            .await;

        let tmp = TempDir::new().unwrap();
        let _guard = StateDirGuard::set(tmp.path());
        let port = server.address().port();
        std::fs::write(port_path(tmp.path()), format!("{port}\n")).unwrap();

        assert_eq!(probe_local_relay_port().await, Some(port));
    }

    // ── classify_running_server (PID-reuse + hung-server handling) ───────────

    /// A PID with no recorded port and no matching process command classifies
    /// as `Foreign` — `stop` must refuse to signal it (possible PID reuse).
    #[tokio::test]
    async fn classify_foreign_when_no_port_and_no_match() {
        let tmp = TempDir::new().unwrap();
        // No server.port written; PID 999_999 is not a spelunk-server process.
        let class = classify_running_server(tmp.path(), 999_999).await;
        assert!(
            matches!(class, RunningServer::Foreign),
            "expected Foreign when nothing identifies the PID as our server"
        );
    }

    /// An unreachable recorded port plus a non-matching process command is
    /// still `Foreign` (health silent AND not a spelunk-server process).
    #[tokio::test]
    async fn classify_foreign_when_unhealthy_and_no_match() {
        let tmp = TempDir::new().unwrap();
        // Bind then free an ephemeral port so it's real but not serving HTTP.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        std::fs::write(port_path(tmp.path()), format!("{port}\n")).unwrap();

        let class = classify_running_server(tmp.path(), 999_999).await;
        assert!(
            matches!(class, RunningServer::Foreign),
            "expected Foreign when /v1/health is silent and the PID isn't spelunk-server"
        );
    }

    /// A responding `/v1/health` on the recorded port classifies as `Healthy`
    /// regardless of the PID — the positive case mirroring a live server.
    #[tokio::test]
    async fn classify_healthy_when_health_responds() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "instance_id": "abc123" })),
            )
            .mount(&server)
            .await;

        let tmp = TempDir::new().unwrap();
        let port = server.address().port();
        std::fs::write(port_path(tmp.path()), format!("{port}\n")).unwrap();

        let class = classify_running_server(tmp.path(), 999_999).await;
        assert!(
            matches!(class, RunningServer::Healthy { .. }),
            "expected Healthy when /v1/health responds on the recorded port"
        );
    }

    // ── db-path state file (single-instance / different-DB guard) ────────────

    #[test]
    fn read_db_path_round_trips() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(db_path_file(tmp.path()), "/some/where/server.db\n").unwrap();
        assert_eq!(
            read_db_path(tmp.path()),
            Some(PathBuf::from("/some/where/server.db"))
        );
    }

    #[test]
    fn read_db_path_none_when_missing_or_empty() {
        let tmp = TempDir::new().unwrap();
        assert!(read_db_path(tmp.path()).is_none());
        std::fs::write(db_path_file(tmp.path()), "\n").unwrap();
        assert!(read_db_path(tmp.path()).is_none());
    }

    #[test]
    fn cleanup_removes_db_path_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(pid_path(tmp.path()), "1\n").unwrap();
        std::fs::write(port_path(tmp.path()), "7777\n").unwrap();
        std::fs::write(db_path_file(tmp.path()), "/x/server.db\n").unwrap();
        cleanup_state_files(tmp.path());
        assert!(!db_path_file(tmp.path()).exists());
        assert!(!pid_path(tmp.path()).exists());
        assert!(!port_path(tmp.path()).exists());
    }

    // ── start lock (single-instance guard) ───────────────────────────────────

    /// Polls `acquire_start_lock` until it succeeds or `timeout` elapses,
    /// returning the last `Result`. `cargo test` compiles every `#[cfg(test)]`
    /// module in the crate into one binary, so a `fork()` in an unrelated,
    /// untagged test elsewhere in that binary can transiently duplicate this
    /// process's fd table (including an already-released lock fd) and delay
    /// when `flock`'s refcount actually reaches zero. That window is bounded
    /// (milliseconds), so a short bounded retry reflects the lock's real
    /// contract without requiring crate-wide serialization.
    #[cfg(unix)]
    fn retry_acquire_start_lock(state_dir: &Path, timeout: Duration) -> Result<StartLock> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match acquire_start_lock(state_dir) {
                Ok(lock) => return Ok(lock),
                Err(e) if std::time::Instant::now() >= deadline => return Err(e),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    /// A second `acquire_start_lock` on the same state dir must fail while the
    /// first guard is still held (serialises concurrent `server start`).
    ///
    /// `#[serial(server_start_lock)]`: this test asserts on `flock` release
    /// timing, which a concurrent `fork()+exec()` in another test can delay (a
    /// forked child transiently inherits the lock fd until it execs). Grouped
    /// with the process-spawning tests below so they never overlap each
    /// other, though untagged subprocess-spawning tests elsewhere in the
    /// crate's single test binary can still race this one; see
    /// `retry_acquire_start_lock`.
    #[cfg(unix)]
    #[test]
    #[serial(server_start_lock)]
    fn start_lock_is_exclusive_while_held() {
        let tmp = TempDir::new().unwrap();
        let first = acquire_start_lock(tmp.path()).expect("first lock acquires");
        assert!(
            acquire_start_lock(tmp.path()).is_err(),
            "second lock must fail while the first is held"
        );
        drop(first);
        // Released, but tolerate the bounded fork-fd race documented above
        // instead of asserting success on the very first attempt.
        assert!(
            retry_acquire_start_lock(tmp.path(), Duration::from_millis(500)).is_ok(),
            "lock frees on drop"
        );
    }

    /// `retry_acquire_start_lock` must still report failure when the lock
    /// genuinely never frees, not silently pass once the timeout elapses.
    #[cfg(unix)]
    #[test]
    #[serial(server_start_lock)]
    fn retry_acquire_start_lock_fails_when_lock_never_frees() {
        let tmp = TempDir::new().unwrap();
        let _held = acquire_start_lock(tmp.path()).expect("first lock acquires");
        assert!(
            retry_acquire_start_lock(tmp.path(), Duration::from_millis(50)).is_err(),
            "must fail when the lock genuinely never frees within the timeout"
        );
    }

    // ── state file / dir permissions (unix-gated) ───────────────────────────

    #[cfg(unix)]
    #[test]
    fn create_state_dir_sets_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("state");
        create_state_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "state dir should be 0700, got {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn write_state_file_sets_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("server.pid");
        write_state_file(&file, "12345\n").unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "state file should be 0600, got {mode:o}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "12345\n");
    }

    #[cfg(unix)]
    #[test]
    fn open_log_file_for_append_sets_0600() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("server.log");
        {
            let mut f = open_log_file_for_append(&file).unwrap();
            f.write_all(b"line one\n").unwrap();
        }
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "log file should be 0600, got {mode:o}");
        // Append semantics: opening again and writing should not truncate.
        {
            let mut f = open_log_file_for_append(&file).unwrap();
            f.write_all(b"line two\n").unwrap();
        }
        let contents = std::fs::read_to_string(&file).unwrap();
        assert_eq!(contents, "line one\nline two\n");
    }

    /// `write_state_file` must refuse to follow a pre-existing symlink at the
    /// target path rather than writing through it (O_NOFOLLOW).
    #[cfg(unix)]
    #[test]
    fn write_state_file_refuses_to_follow_symlink() {
        let tmp = TempDir::new().unwrap();
        let outside_target = tmp.path().join("outside.txt");
        std::fs::write(&outside_target, "do not overwrite me").unwrap();

        let link_path = tmp.path().join("server.pid");
        std::os::unix::fs::symlink(&outside_target, &link_path).unwrap();

        let result = write_state_file(&link_path, "12345\n");
        assert!(
            result.is_err(),
            "write_state_file must refuse to follow a symlink at the target path"
        );
        // The symlink target must be untouched.
        assert_eq!(
            std::fs::read_to_string(&outside_target).unwrap(),
            "do not overwrite me"
        );
    }

    // ── same_path (different-DB start guard predicate) ───────────────────────
    //
    // `cmd_start` refuses to start a second server against a *different* DB by
    // comparing the recorded db-path against the requested one via `same_path`.
    // The full decision runs against the real home state dir + a live daemon
    // (an e2e-only path), so these cover the load-bearing predicate directly.

    #[test]
    fn same_path_true_for_identical() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("server.db");
        std::fs::write(&p, b"x").unwrap();
        assert!(same_path(&p, &p));
    }

    #[test]
    fn same_path_false_for_distinct() {
        let tmp = TempDir::new().unwrap();
        // Non-existent distinct paths fall back to raw comparison → not equal.
        assert!(!same_path(
            &tmp.path().join("a.db"),
            &tmp.path().join("b.db")
        ));
    }

    /// A symlink and its target name the same DB — the guard must treat a
    /// `start` against either as the same server, not a different DB.
    #[cfg(unix)]
    #[test]
    fn same_path_true_across_symlink() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("real.db");
        std::fs::write(&target, b"x").unwrap();
        let link = tmp.path().join("link.db");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(
            same_path(&target, &link),
            "a symlink and its target are the same DB"
        );
    }

    // ── ensure_port_available_for_start (no silent port drift) ───────────────

    /// A free port passes — `start` binds the exact requested port.
    #[tokio::test]
    async fn ensure_port_available_for_start_ok_when_free() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(ensure_port_available_for_start(port).await.is_ok());
    }

    /// A port held by an unrelated process makes `start` fail loudly (naming
    /// the port) instead of drifting to a different one.
    #[tokio::test]
    async fn ensure_port_available_for_start_fails_when_port_held() {
        // Hold the listener for the whole call so the bounded retry never frees.
        let _held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = _held.local_addr().unwrap().port();
        let err = ensure_port_available_for_start(port)
            .await
            .expect_err("must fail while the port is held");
        assert!(
            err.to_string().contains(&format!("port {port}")),
            "error should name the occupied port, got: {err}"
        );
    }

    // ── Live-process helpers: identity + termination ─────────────────────────
    //
    // These spawn a real short-lived process to exercise the Unix signal /
    // identity paths that only a real PID can drive. Every spawned process is
    // reaped: a background thread `wait()`s it (so a killed process can't linger
    // as a zombie — a zombie still answers `kill(pid, 0)` and would fool
    // `pid_is_alive`), and `Drop` SIGKILLs any still-live helper.

    #[cfg(unix)]
    struct DummyProc {
        pid: u32,
        done: std::sync::Arc<std::sync::atomic::AtomicBool>,
        reaper: Option<std::thread::JoinHandle<()>>,
    }

    #[cfg(unix)]
    impl DummyProc {
        /// Spawn `cmd` detached from stdio and start reaping it immediately.
        fn spawn(cmd: &mut std::process::Command) -> Self {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, Ordering};

            let mut child = cmd
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn dummy process");
            let pid = child.id();
            let done = Arc::new(AtomicBool::new(false));
            let done_reaper = Arc::clone(&done);
            let reaper = std::thread::spawn(move || {
                let _ = child.wait();
                done_reaper.store(true, Ordering::SeqCst);
            });
            DummyProc {
                pid,
                done,
                reaper: Some(reaper),
            }
        }

        /// A `sleep`-style process that responds normally to SIGTERM.
        fn graceful() -> Self {
            DummyProc::spawn(std::process::Command::new("sleep").arg("30"))
        }

        /// A process that ignores SIGTERM from birth (only SIGKILL reaps it) —
        /// a wedged daemon. `pre_exec` sets SIGTERM to `SIG_IGN`, which is
        /// preserved across the `exec` into `sleep` (POSIX), so there is no
        /// trap-install race and no shell child to orphan on SIGKILL.
        fn ignores_sigterm() -> Self {
            use std::os::unix::process::CommandExt;
            let mut cmd = std::process::Command::new("sleep");
            cmd.arg("30");
            // SAFETY: the closure only calls `signal`, which is async-signal-safe.
            unsafe {
                cmd.pre_exec(|| {
                    unsafe extern "C" {
                        fn signal(signum: i32, handler: usize) -> usize;
                    }
                    const SIGTERM: i32 = 15;
                    const SIG_IGN: usize = 1;
                    signal(SIGTERM, SIG_IGN);
                    Ok(())
                });
            }
            DummyProc::spawn(&mut cmd)
        }

        /// A live process whose command line contains `spelunk-server`, so
        /// `process_matches_server` recognises it as ours.
        fn named_server() -> (Self, TempDir) {
            use std::os::unix::fs::PermissionsExt;
            let dir = TempDir::new().unwrap();
            let bin = dir.path().join("spelunk-server");
            std::fs::write(&bin, "#!/bin/sh\nsleep 30\n").unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            (DummyProc::spawn(&mut std::process::Command::new(&bin)), dir)
        }
    }

    #[cfg(unix)]
    impl Drop for DummyProc {
        fn drop(&mut self) {
            use std::sync::atomic::Ordering;
            // SIGKILL only if it hasn't already exited, to avoid signalling a
            // reused PID after the reaper collected ours.
            if !self.done.load(Ordering::SeqCst) {
                let _ = force_kill(self.pid);
            }
            if let Some(h) = self.reaper.take() {
                let _ = h.join();
            }
        }
    }

    // ── process_matches_server (hung-server identity signal) ─────────────────

    #[cfg(unix)]
    #[test]
    #[serial(server_start_lock)]
    fn process_matches_server_true_for_named_process() {
        let (proc, _dir) = DummyProc::named_server();
        assert!(
            process_matches_server(proc.pid),
            "a process whose command line contains 'spelunk-server' must match"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial(server_start_lock, path_env)]
    fn process_matches_server_false_for_unrelated_process() {
        let proc = DummyProc::graceful();
        assert!(
            !process_matches_server(proc.pid),
            "an unrelated process must not be mistaken for a spelunk-server"
        );
    }

    // ── classify_running_server: HungOurs (reclaimable wedged daemon) ────────

    /// A live `spelunk-server` process with a silent `/v1/health` (no recorded
    /// port) is our wedged daemon — classified `HungOurs`, not `Foreign`, so
    /// `stop`/`start` reclaim it instead of refusing. This is the core of the
    /// fix: the old health-only check gave up on a hung server.
    #[cfg(unix)]
    #[tokio::test]
    #[serial(server_start_lock)]
    async fn classify_hung_ours_when_process_matches_but_health_silent() {
        let (proc, _dir) = DummyProc::named_server();
        let tmp = TempDir::new().unwrap(); // no server.port → health probe skipped
        let class = classify_running_server(tmp.path(), proc.pid).await;
        assert!(
            matches!(class, RunningServer::HungOurs),
            "expected HungOurs for a live spelunk-server process with silent health"
        );
    }

    // ── terminate_and_wait / force_kill / wait_for_exit ──────────────────────

    /// `wait_for_exit` must NOT report a still-running process as gone — the
    /// guard that keeps `terminate_and_wait` from claiming a stop that didn't
    /// happen.
    #[cfg(unix)]
    #[tokio::test]
    #[serial(server_start_lock, path_env)]
    async fn wait_for_exit_false_for_live_process() {
        let proc = DummyProc::graceful();
        assert!(
            !wait_for_exit(proc.pid, Duration::from_millis(300)).await,
            "a live process must not be reported as exited"
        );
    }

    /// Graceful path: a SIGTERM-responsive process is terminated and only
    /// reported stopped once the PID is confirmed gone.
    #[cfg(unix)]
    #[tokio::test]
    #[serial(server_start_lock, path_env)]
    async fn terminate_and_wait_stops_graceful_process() {
        let proc = DummyProc::graceful();
        assert!(pid_is_alive(proc.pid));
        let stopped = terminate_and_wait(proc.pid).await.expect("terminate");
        assert!(stopped, "graceful process should be reported stopped");
        assert!(!pid_is_alive(proc.pid), "process must actually be gone");
    }

    /// Escalation seam: SIGKILL reaps a process that ignores SIGTERM, and
    /// `wait_for_exit` confirms it is gone — the mechanism `terminate_and_wait`
    /// falls back to for a wedged daemon.
    #[cfg(unix)]
    #[tokio::test]
    #[serial(server_start_lock, path_env)]
    async fn force_kill_reaps_sigterm_ignoring_process() {
        let proc = DummyProc::ignores_sigterm();
        assert!(pid_is_alive(proc.pid));
        // SIGTERM alone leaves it running (trap ignores it).
        terminate_process(proc.pid).expect("SIGTERM");
        assert!(
            !wait_for_exit(proc.pid, Duration::from_millis(400)).await,
            "SIGTERM-ignoring process should survive SIGTERM"
        );
        // SIGKILL cannot be trapped; it must go.
        force_kill(proc.pid).expect("SIGKILL");
        assert!(
            wait_for_exit(proc.pid, FORCE_KILL_TIMEOUT).await,
            "SIGKILL must reap the process"
        );
    }

    /// Integrated wedged-stop: `terminate_and_wait` on a daemon that ignores
    /// SIGTERM escalates to SIGKILL and reports success only once the PID is
    /// gone. Slow (spans the graceful-stop timeout) but captures the exact
    /// behaviour the fix is about — previously only exercised by hand.
    #[cfg(unix)]
    #[tokio::test]
    #[serial(server_start_lock, path_env)]
    async fn terminate_and_wait_escalates_when_sigterm_ignored() {
        let proc = DummyProc::ignores_sigterm();
        assert!(pid_is_alive(proc.pid));
        let stopped = terminate_and_wait(proc.pid)
            .await
            .expect("terminate should not error");
        assert!(
            stopped,
            "a SIGTERM-ignoring daemon must still be stopped via SIGKILL"
        );
        assert!(
            !pid_is_alive(proc.pid),
            "success must mean the PID is actually gone"
        );
    }
}
