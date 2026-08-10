// Deterministic crash-point synchronisation for the crash-safety integration
// suite (`crates/spelunk-cli/tests/crash_safety.rs`): landing a real SIGKILL
// inside a specific write window by racing wall-clock sleeps against another
// process is inherently flaky, so the harness instead waits for this
// process to print a marker proving it reached the exact window, then kills
// it while it is parked here. Reading from a pipe the harness never writes
// to blocks until the harness closes it (by killing us) or writes a byte (to
// release us without a crash, used by tests that need a held write window
// rather than a kill).
//
// Gated on `debug_assertions` rather than `cfg(test)`: the harness spawns the
// real `spelunk` binary as a subprocess (see `assert_cmd::cargo_bin` in
// crash_safety.rs), which never gets `cfg(test)` even under `cargo test`.
// `debug_assertions` is the one signal both builds agree on: on for the dev
// profile the test harness spawns, off for `--release`, so this body carries
// no reachable code path in a release binary.
#[cfg(debug_assertions)]
pub(super) fn pause_at(point: &str, subject: &str) {
    let Ok(target) = std::env::var("SPELUNK_TEST_CRASH_POINT") else {
        return;
    };
    if target != format!("{point}:{subject}") {
        return;
    }
    println!("SPELUNK_TEST_CRASH_POINT_REACHED:{target}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut buf = [0u8; 1];
    let _ = std::io::Read::read(&mut std::io::stdin(), &mut buf);
}

#[cfg(not(debug_assertions))]
pub(super) fn pause_at(_point: &str, _subject: &str) {}
