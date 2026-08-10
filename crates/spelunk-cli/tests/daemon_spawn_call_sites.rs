// Every caller of `ensure_server_running` must forward a config it was given.
//
// Both production callers (`init`, and the outbox nudge after a local_first
// write) are gated on an interactive stdin, and `assert_cmd` hands its children
// piped stdin, so no integration test in this crate can reach either one. The
// gate is deliberate and worth keeping, but it leaves the last hop of the
// endpoint's journey with no runtime proof available to it: swapping both
// arguments for a freshly defaulted `Config` leaves the whole crate green while
// disconnecting the personal config from every auto-started daemon.
//
// The hop below it is covered at runtime (`ensure_server_running` itself is
// driven against a recording stand-in, and `server start` end to end in
// `llm_daemon_spawn_e2e`). This is the one link that has to be pinned
// lexically instead: constructing a config at the call site, rather than
// forwarding one, is the whole failure mode.

use std::path::{Path, PathBuf};

const CALL: &str = "ensure_server_running(";

// A config argument that came from somewhere else. Anything constructed in
// place is what this guard exists to catch.
const FORWARDED: [&str; 2] = ["&cfg", "cfg"];

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("reading a CLI source directory") {
        let path = entry.expect("reading a directory entry").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

// The config argument as written, for a line that calls the function.
fn config_argument(line: &str) -> Option<String> {
    let after = line.split_once(CALL)?.1;
    let inside = after.split_once(')')?.0;
    let (_port, cfg) = inside.split_once(',')?;
    Some(cfg.trim().to_string())
}

#[test]
fn every_auto_start_call_site_forwards_the_loaded_config() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);

    let mut call_sites = 0;
    let mut offenders = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("reading a CLI source file");
        for (lineno, line) in text.lines().enumerate() {
            if !line.contains(CALL) || line.contains("fn ensure_server_running(") {
                continue;
            }
            call_sites += 1;
            match config_argument(line) {
                Some(arg) if FORWARDED.contains(&arg.as_str()) => {}
                other => offenders.push(format!(
                    "{}:{}: config argument is {:?}",
                    file.display(),
                    lineno + 1,
                    other.unwrap_or_else(|| line.trim().to_string())
                )),
            }
        }
    }

    assert!(
        call_sites >= 2,
        "expected to find the auto-start call sites and found {call_sites}; if the call moved \
         or was renamed, this guard is scanning for nothing"
    );
    assert!(
        offenders.is_empty(),
        "an auto-start call site builds its own config instead of forwarding the loaded one, \
         so the personal config's llm_url would never reach the daemon it starts. Found: \
         {offenders:#?}"
    );
}
