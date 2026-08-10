#![no_main]

use libfuzzer_sys::fuzz_target;
use spelunk_core::indexer::chunker::sliding_window;

// Fuzz the sliding-window chunker with arbitrary text input.
//
// Run with:
//   cargo +nightly fuzz run fuzz_chunker -- -max_total_time=600
//
// Goal: find panics or out-of-bounds accesses in the chunking logic. Window and
// overlap budgets are derived internally from the token cap, so the source text
// is the only fuzzed input; the name/docstring/parent_scope metadata is just
// copied onto each emitted chunk and passed as None here.
fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    let _ = sliding_window(s, "fuzz_input", "text", None, None, None);
});
