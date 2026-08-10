//! Detached-child spawn and run-lock handoff machinery for `spelunk index`.
//!
//! Two sites in `index()` release the run lock and hand the rest of the work
//! to a re-exec'd child process (`--_embed-phases`, `--_background-phases`) so
//! the parent can return the prompt: the detached-embed spawn and the
//! phases-3–5 background spawn. This module owns the argv both share, the
//! embed-specific spawn helper, and the constants used to confirm a spawned
//! child actually became the run lock's new holder before reporting success.

use anyhow::Result;

use super::IndexArgs;
use crate::capability;

/// Log for the detached phases-3–5 child, beside the index it reports on.
pub(super) fn background_log_path(db_path: &std::path::Path) -> Option<std::path::PathBuf> {
    db_path.parent().map(|d| d.join("index-background.log"))
}

/// Point the detached child's stdout+stderr at `log`, returning the path
/// actually in use. Falls back to a null sink when the log cannot be opened,
/// since diagnostics are best-effort and must never fail the index.
///
/// Inheriting the parent's streams instead is not an option: a pipe reader
/// (`git commit`, CI) blocks until the detached child exits, and a reader that
/// closes first SIGPIPEs the child mid-phase.
pub(super) fn redirect_to_background_log<'a>(
    cmd: &mut std::process::Command,
    log: Option<&'a std::path::Path>,
) -> Option<&'a std::path::Path> {
    // stdout and stderr need independent handles onto the same file.
    let opened = log.and_then(|p| {
        let out = super::super::helpers::open_private_file_for_write(p).ok()?;
        let err = out.try_clone().ok()?;
        Some((p, out, err))
    });
    match opened {
        Some((path, out, err)) => {
            cmd.stdout(out).stderr(err);
            Some(path)
        }
        None => {
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            None
        }
    }
}

/// Outcome of the detached embed spawn.
pub(super) enum EmbedSpawn<'a> {
    /// Spawn failed; caller embeds inline.
    Inline,
    /// Running detached: the diagnostics log actually in use (if any) so the
    /// caller can point the user at it, and the child's pid so the caller can
    /// confirm it (and not a racing third process) became the lock's holder.
    Detached {
        log_in_use: Option<&'a std::path::Path>,
        child_pid: u32,
    },
}

/// Build the argv shared by every detached re-exec that continues indexing in
/// a child process: the child parses its own fresh `IndexArgs`/`Config` from
/// this argv rather than inheriting the parent's already-parsed values, so
/// anything the parent resolved that isn't a plain pass-through of `args`
/// itself (the global `--config` override) or on this list (`--no-summaries`,
/// `--summary-batch-size`) would otherwise silently reset to its default in
/// the child. `mode_flag` selects which internal phase-only mode the child
/// runs (`--_background-phases` or `--_embed-phases`); callers append any
/// mode-specific flags (e.g. `--batch-size` for the embed phase) afterwards.
///
/// Env vars and cwd are not part of this contract: `std::process::Command`
/// inherits both by default and nothing here calls `.env_clear()` or
/// `.current_dir()` to opt out (see the regression test below).
pub(super) fn build_detached_child_command(
    exe: &std::path::Path,
    mode_flag: &str,
    args: &IndexArgs,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("index");
    cmd.arg(&args.path);
    cmd.arg(mode_flag);
    if let Some(db_arg) = &args.db {
        cmd.args(["--db", &db_arg.to_string_lossy()]);
    }
    if let Some(cfg_path) = &args.config_path {
        cmd.args(["--config", &cfg_path.to_string_lossy()]);
    }
    if args.no_summaries {
        cmd.arg("--no-summaries");
    }
    cmd.args(["--summary-batch-size", &args.summary_batch_size.to_string()]);
    cmd.stdin(std::process::Stdio::null());
    cmd
}

/// Spawn a detached background process to run the embed phase (plus phases 3–5)
/// against the chunks the foreground run just parsed, reusing the internal
/// `--_embed-phases` mode. Mirrors the phases-3–5 background spawn: the parent
/// regains its prompt immediately and the child's diagnostics go to `log`.
pub(super) fn spawn_embed_subprocess<'a>(
    args: &IndexArgs,
    log: Option<&'a std::path::Path>,
) -> Result<EmbedSpawn<'a>> {
    let mut cmd = build_detached_child_command(&std::env::current_exe()?, "--_embed-phases", args);
    cmd.args(["--batch-size", &args.batch_size.to_string()]);
    let in_use = redirect_to_background_log(&mut cmd, log);
    match cmd.spawn() {
        Ok(child) => Ok(EmbedSpawn::Detached {
            log_in_use: in_use,
            child_pid: child.id(),
        }),
        Err(e) => {
            tracing::warn!("failed to spawn detached embed process; embedding inline: {e}");
            Ok(EmbedSpawn::Inline)
        }
    }
}

/// True when handing the embed pass to the detached worker can do useful work:
/// the embedder is `ready`, or still `loading` (the worker owns the readiness
/// wait, see [`super::phases::wait_for_embedder`]). `unavailable` and
/// `disabled` are terminal for this server process, and an older server that
/// never advertises `index.embed` has nothing to wait for.
pub(super) fn detach_embed_eligible(tier: &capability::Tier) -> bool {
    matches!(tier.caps(), Some(c) if c.index_embed)
        || matches!(
            tier.embedder_state(),
            Some(capability::EmbedderState::Loading)
        )
}

/// How long the parent waits, after releasing the run lock and spawning a
/// continuation child, to see it recorded as the lock's new holder before
/// reporting the handoff as a background success. The release-then-spawn gap
/// a racing `spelunk index` can win is normally low-single-digit
/// milliseconds, so this bounds well above that without delaying the common
/// case where the child wins on its first poll.
pub(super) const HANDOFF_CONFIRM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// Poll interval for `HANDOFF_CONFIRM_TIMEOUT`.
pub(super) const HANDOFF_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Minimal parser wrapper so we can exercise `IndexArgs` clap parsing in
    /// isolation without pulling in the whole top-level `Cli`.
    #[derive(clap::Parser, Debug)]
    struct TestCli {
        #[command(flatten)]
        index: IndexArgs,
    }

    fn sample_index_args() -> IndexArgs {
        TestCli::try_parse_from(["spelunk", "some/path"])
            .expect("parse")
            .index
    }

    // ── build_detached_child_command: shared re-exec contract ───────────────────

    #[test]
    fn detached_child_command_inherits_cwd_and_env() {
        // `std::process::Command` inherits both by default; this only breaks
        // if a future edit adds `.current_dir(...)` or `.env_clear()`/`.env(...)`
        // to the shared builder.
        let cmd = build_detached_child_command(
            std::path::Path::new("/usr/bin/spelunk"),
            "--_background-phases",
            &sample_index_args(),
        );
        assert!(
            cmd.get_current_dir().is_none(),
            "must inherit the parent's cwd rather than pin one"
        );
        assert!(
            cmd.get_envs().next().is_none(),
            "must inherit the parent's environment rather than clear or override it"
        );
    }

    #[test]
    fn detached_child_command_forwards_config_path_when_resolved() {
        // Before the fix, `IndexArgs` had no config-path field at all, so
        // neither spawn could forward a resolved `--config` override and the
        // child re-resolved the default config instead.
        let mut args = sample_index_args();
        args.config_path = Some(std::path::PathBuf::from("/tmp/custom-config.toml"));
        let cmd = build_detached_child_command(
            std::path::Path::new("/usr/bin/spelunk"),
            "--_background-phases",
            &args,
        );
        let argv: Vec<_> = cmd.get_args().collect();
        let pos = argv
            .iter()
            .position(|a| *a == "--config")
            .expect("--config must be forwarded when the parent resolved an override");
        assert_eq!(argv[pos + 1], "/tmp/custom-config.toml");
    }

    #[test]
    fn detached_child_command_omits_config_flag_when_not_resolved() {
        // A default-config run must not force an explicit `--config` onto the
        // child: `config_path` is `None` when the user passed no override, and
        // an unconditional `--config` would stop the child from resolving its
        // own default the way the parent did.
        let args = sample_index_args();
        assert!(args.config_path.is_none());
        let cmd = build_detached_child_command(
            std::path::Path::new("/usr/bin/spelunk"),
            "--_background-phases",
            &args,
        );
        let argv: Vec<_> = cmd.get_args().collect();
        assert!(
            !argv.iter().any(|a| *a == "--config"),
            "must not add --config when the parent had no override"
        );
    }

    #[test]
    fn detached_child_command_forwards_no_summaries_to_both_spawn_sites() {
        // Before the fix the phases-3-5 background spawn built its argv
        // independently and never included `--no-summaries` at all (only the
        // embed-phase spawn did), so disabling summaries still let the
        // background child generate them.
        let mut args = sample_index_args();
        args.no_summaries = true;
        for mode_flag in ["--_background-phases", "--_embed-phases"] {
            let cmd = build_detached_child_command(
                std::path::Path::new("/usr/bin/spelunk"),
                mode_flag,
                &args,
            );
            let argv: Vec<_> = cmd.get_args().collect();
            assert!(
                argv.iter().any(|a| *a == "--no-summaries"),
                "--no-summaries must reach the {mode_flag} child"
            );
        }
    }

    #[test]
    fn detached_child_command_forwards_configured_summary_batch_size_to_both_spawn_sites() {
        // Before the fix neither spawn forwarded `--summary-batch-size`, so a
        // custom value silently reset to the default (10) in whichever child
        // ran phase 4.
        let args = TestCli::try_parse_from(["spelunk", "some/path", "--summary-batch-size", "42"])
            .expect("parse")
            .index;
        assert_eq!(args.summary_batch_size, 42);
        for mode_flag in ["--_background-phases", "--_embed-phases"] {
            let cmd = build_detached_child_command(
                std::path::Path::new("/usr/bin/spelunk"),
                mode_flag,
                &args,
            );
            let argv: Vec<_> = cmd.get_args().collect();
            let pos = argv
                .iter()
                .position(|a| *a == "--summary-batch-size")
                .expect("--summary-batch-size must be forwarded");
            assert_eq!(argv[pos + 1], "42");
        }
    }

    // ── detach_embed_eligible: the spawn gate must include `loading` ────────────

    fn tier_with(embed_ready: bool, state: capability::EmbedderState) -> capability::Tier {
        let mut caps = capability::Capabilities::all();
        caps.index_embed = embed_ready;
        capability::Tier::Server {
            url: "http://127.0.0.1:7777".to_string(),
            caps,
            auto_discovered: true,
            embedder_state: state,
            server_limits: None,
        }
    }

    #[test]
    fn detach_eligible_when_embedder_ready() {
        assert!(detach_embed_eligible(&tier_with(
            true,
            capability::EmbedderState::Ready
        )));
    }

    #[test]
    fn detach_eligible_when_embedder_still_loading() {
        // The cold-start case ADR-070 D1/D2 exists for: a server started
        // moments ago advertises no index.embed yet, but the worker can wait
        // it out. Gating the spawn on readiness alone is the recorded no-op.
        assert!(detach_embed_eligible(&tier_with(
            false,
            capability::EmbedderState::Loading
        )));
    }

    #[test]
    fn detach_not_eligible_for_terminal_embedder_states() {
        for state in [
            capability::EmbedderState::Unavailable,
            capability::EmbedderState::Disabled,
            capability::EmbedderState::Unknown,
        ] {
            assert!(
                !detach_embed_eligible(&tier_with(false, state)),
                "state {state:?} is terminal; spawning a worker would wait forever"
            );
        }
    }

    #[test]
    fn detach_not_eligible_offline() {
        assert!(!detach_embed_eligible(&capability::Tier::Offline));
    }

    #[test]
    fn handoff_confirm_timeout_is_2s() {
        assert_eq!(HANDOFF_CONFIRM_TIMEOUT.as_secs(), 2);
    }

    #[test]
    fn handoff_poll_interval_is_20ms() {
        assert_eq!(HANDOFF_POLL_INTERVAL.as_millis(), 20);
    }
}
