#![no_main]

use libfuzzer_sys::fuzz_target;
use spelunk_core::indexer::secrets::contains_secret;

/// Fuzz `contains_secret` with arbitrary UTF-8 text.
///
/// Run with:
///   cargo +nightly fuzz run fuzz_secret_scanner -- -max_total_time=600
///
/// Goal: find panics or infinite loops in regex matching.
fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    let _ = contains_secret(s);
});
