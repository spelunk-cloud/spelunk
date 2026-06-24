#![no_main]

use libfuzzer_sys::fuzz_target;

/// Fuzz JSONL parsing used throughout the `spelunk spelunk` plumbing layer.
///
/// Run with:
///   cargo +nightly fuzz run fuzz_jsonl_parse -- -max_total_time=600
///
/// Goal: confirm `serde_json` doesn't panic on malformed or adversarial input
/// when lines are fed one at a time as JSONL.
fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    for line in s.lines() {
        let _ = serde_json::from_str::<serde_json::Value>(line);
    }
});
