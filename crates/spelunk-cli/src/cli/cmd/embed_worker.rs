//! Liveness and progress-baseline state for the background embed worker, so
//! `spelunk status` reports what it knows about its own subprocess instead of
//! guessing from chunk counts (which cannot distinguish a running job from an
//! abandoned one).
//!
//! Mirrors the server's own pid-file shape: one small state file per datum,
//! written 0600 into the same state directory as the server's pid/port files
//! (`capability::spelunk_state_dir`, the single resolver both share, so an
//! `SPELUNK_STATE_DIR` override applies to worker and server files alike), a
//! `pid_is_alive` liveness check, and a foreign-pid classification so a
//! recycled pid is never misreported as a live worker. Files are keyed by a
//! hash of the index path because workers are per-project while the state
//! dir is per-machine.
//!
//! Two files per project:
//! - `embed-worker-<key>.pid`: pid of the process running the embed phase
//! - `embed-worker-<key>.baseline`: `<started_at_unix> <pending_tokens>` at
//!   worker start, letting `status` derive a measured-this-run,
//!   token-weighted ETA (tokens drained since start over elapsed time). The
//!   rate is never persisted across runs; a cached rate is a defect because
//!   the token estimate's bias is corpus-dependent.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::server::{create_state_dir, pid_is_alive, write_state_file};
use crate::capability::spelunk_state_dir;
use crate::storage::Database;

/// Per-project key for the worker state files: hash of the canonicalised
/// index path (two projects must never share a liveness file).
fn worker_key(db_path: &Path) -> String {
    let canonical = spelunk_core::utils::canonicalize(db_path);
    let hash = blake3::hash(canonical.to_string_lossy().as_bytes());
    hash.to_hex()[..16].to_string()
}

fn pid_file(state_dir: &Path, key: &str) -> PathBuf {
    state_dir.join(format!("embed-worker-{key}.pid"))
}

fn baseline_file(state_dir: &Path, key: &str) -> PathBuf {
    state_dir.join(format!("embed-worker-{key}.baseline"))
}

/// RAII liveness marker held by whichever process runs the embed phase (the
/// detached worker, or a foreground `spelunk index` resume). Best-effort:
/// state-file failures must never fail the embed itself.
///
/// Dropped on clean exit; a killed worker leaves the files behind, which the
/// next `status` classifies as a dead pid and cleans up.
pub(super) struct EmbedWorkerGuard {
    pid_path: PathBuf,
    baseline_path: PathBuf,
}

impl EmbedWorkerGuard {
    /// Record this process as the live embed worker for `db_path`. `None`
    /// when the state dir is unusable (embedding proceeds unrecorded).
    pub(super) fn acquire(db: &Database, db_path: &Path) -> Option<Self> {
        let state_dir = spelunk_state_dir().ok()?;
        create_state_dir(&state_dir).ok()?;
        let key = worker_key(db_path);
        let pid_path = pid_file(&state_dir, &key);
        let baseline_path = baseline_file(&state_dir, &key);

        write_state_file(&pid_path, &format!("{}\n", std::process::id())).ok()?;

        // Baseline for the token-weighted ETA. Failure to write it only costs
        // the ETA, not the liveness record.
        let pending = db
            .embed_token_stats()
            .map(|s| s.pending_tokens)
            .unwrap_or(0);
        let now = chrono::Utc::now().timestamp();
        let _ = write_state_file(&baseline_path, &format!("{now} {pending}\n"));

        Some(Self {
            pid_path,
            baseline_path,
        })
    }
}

impl Drop for EmbedWorkerGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.pid_path);
        let _ = std::fs::remove_file(&self.baseline_path);
    }
}

/// What `status` knows about the worker for a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkerLiveness {
    /// The recorded pid is alive and its command line looks like a spelunk
    /// index run.
    Alive,
    /// No recorded worker, a dead pid, or a pid recycled by an unrelated
    /// process (foreign). Never reported as running.
    NotRunning,
}

/// Classify a recorded worker pid. Pure so the alive/foreign matrix is unit
/// testable without real processes; `looks_like_worker` is the command-line
/// identity check (a pid can be recycled by an unrelated process after a
/// crash, and that must not read as a live embed run).
fn classify_worker_pid(alive: bool, looks_like_worker: bool) -> WorkerLiveness {
    if alive && looks_like_worker {
        WorkerLiveness::Alive
    } else {
        WorkerLiveness::NotRunning
    }
}

/// Return `true` when `pid`'s command line looks like a spelunk index run
/// (the detached `--_embed-phases` worker or a foreground resume).
fn process_looks_like_index_run(pid: u32) -> bool {
    #[cfg(unix)]
    {
        match std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "args="])
            .output()
        {
            Ok(out) if out.status.success() => {
                let args = String::from_utf8_lossy(&out.stdout);
                args.contains("spelunk") && args.contains("index")
            }
            _ => false,
        }
    }
    #[cfg(windows)]
    {
        // tasklist exposes only the image name, so match on the binary; the
        // worst case of the coarser match is reporting another spelunk
        // process's pid as a live worker, never a foreign process's.
        match std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
        {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .to_lowercase()
                .contains("spelunk"),
            _ => false,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Read and classify the recorded worker for `db_path`. Stale state (dead or
/// foreign pid) is cleaned up so it cannot be re-read as live later.
pub(super) fn worker_liveness(db_path: &Path) -> WorkerLiveness {
    let Ok(state_dir) = spelunk_state_dir() else {
        return WorkerLiveness::NotRunning;
    };
    let key = worker_key(db_path);
    let pid_path = pid_file(&state_dir, &key);
    let Some(pid) = std::fs::read_to_string(&pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    else {
        return WorkerLiveness::NotRunning;
    };
    match classify_worker_pid(pid_is_alive(pid), process_looks_like_index_run(pid)) {
        WorkerLiveness::Alive => WorkerLiveness::Alive,
        WorkerLiveness::NotRunning => {
            // Dead or foreign: the recorded state is stale, remove it.
            let _ = std::fs::remove_file(&pid_path);
            let _ = std::fs::remove_file(baseline_file(&state_dir, &key));
            WorkerLiveness::NotRunning
        }
    }
}

/// Token-weighted ETA for a live worker: tokens drained since the recorded
/// baseline over elapsed wall time, applied to the tokens still pending.
/// `None` before any measurable progress (calibrating) or without a baseline.
pub(super) fn worker_eta(db_path: &Path, pending_tokens_now: i64) -> Option<Duration> {
    let state_dir = spelunk_state_dir().ok()?;
    let key = worker_key(db_path);
    let contents = std::fs::read_to_string(baseline_file(&state_dir, &key)).ok()?;
    let mut parts = contents.split_whitespace();
    let started_at: i64 = parts.next()?.parse().ok()?;
    let pending_at_start: i64 = parts.next()?.parse().ok()?;
    eta_from_baseline(
        started_at,
        pending_at_start,
        chrono::Utc::now().timestamp(),
        pending_tokens_now,
    )
}

/// Pure ETA math over the baseline: measured this run, token-weighted.
fn eta_from_baseline(
    started_at: i64,
    pending_at_start: i64,
    now: i64,
    pending_now: i64,
) -> Option<Duration> {
    let elapsed = now.checked_sub(started_at)?;
    let drained = pending_at_start.checked_sub(pending_now)?;
    if elapsed <= 0 || drained <= 0 || pending_now <= 0 {
        return None;
    }
    let rate = drained as f64 / elapsed as f64; // tokens per second
    let secs = pending_now as f64 / rate;
    if !secs.is_finite() {
        return None;
    }
    Some(Duration::from_secs_f64(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── classify_worker_pid: liveness × identity matrix ─────────────────────

    #[test]
    fn alive_and_matching_command_is_a_live_worker() {
        assert_eq!(classify_worker_pid(true, true), WorkerLiveness::Alive);
    }

    #[test]
    fn dead_pid_is_not_running() {
        assert_eq!(classify_worker_pid(false, true), WorkerLiveness::NotRunning);
        assert_eq!(
            classify_worker_pid(false, false),
            WorkerLiveness::NotRunning
        );
    }

    #[test]
    fn foreign_pid_is_never_reported_as_a_live_worker() {
        // A pid recycled by an unrelated process after a crash: alive, but the
        // command line is not a spelunk index run. Reporting it as "Embedding
        // in progress" is exactly the guess D4 removes.
        assert_eq!(classify_worker_pid(true, false), WorkerLiveness::NotRunning);
    }

    // ── worker_key: per-project isolation ───────────────────────────────────

    #[test]
    fn worker_key_differs_per_project() {
        let a = worker_key(Path::new("/proj-a/.spelunk/index.db"));
        let b = worker_key(Path::new("/proj-b/.spelunk/index.db"));
        assert_ne!(a, b, "two projects must never share a liveness file");
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn worker_key_is_stable_for_the_same_path() {
        // Writer (worker) and reader (status) derive the key independently;
        // any nondeterminism here silently severs status from its worker.
        let p = Path::new("/proj-a/.spelunk/index.db");
        assert_eq!(worker_key(p), worker_key(p));
    }

    // ── eta_from_baseline: measured-this-run token rate ─────────────────────

    #[test]
    fn eta_scales_with_pending_tokens_at_the_measured_rate() {
        // 1000 tokens drained in 100 s → 10 tokens/s; 5000 pending → 500 s.
        let eta = eta_from_baseline(0, 6000, 100, 5000).expect("measurable progress");
        assert_eq!(eta, Duration::from_secs(500));
    }

    #[test]
    fn eta_is_none_while_calibrating() {
        // No tokens drained yet: no measured rate, no ETA (never a guess).
        assert!(eta_from_baseline(0, 6000, 100, 6000).is_none());
        // Zero elapsed time.
        assert!(eta_from_baseline(100, 6000, 100, 5000).is_none());
        // Nothing pending.
        assert!(eta_from_baseline(0, 6000, 100, 0).is_none());
    }

    #[test]
    fn eta_tolerates_a_clock_step_or_regressed_baseline() {
        // A baseline from the future or pending that grew (concurrent
        // re-index) must yield None, not a negative/panicking duration.
        assert!(eta_from_baseline(200, 6000, 100, 5000).is_none());
        assert!(eta_from_baseline(0, 5000, 100, 6000).is_none());
    }

    #[test]
    fn eta_survives_extreme_token_counts_without_panicking() {
        // i64::MAX-scale token sums (a corrupt baseline file is user-writable
        // input) must not overflow checked_sub or Duration::from_secs_f64.
        let eta = eta_from_baseline(0, i64::MAX, 1, i64::MAX - 1000);
        assert!(eta.is_some(), "a huge but finite ETA is still an ETA");
        // Underflow direction: pending_at_start negative, pending_now MAX.
        assert!(eta_from_baseline(0, i64::MIN, 1, i64::MAX).is_none());
    }
}
