#![no_main]

use libfuzzer_sys::fuzz_target;
use spelunk_core::indexer::chunker::sliding_window;

/// Fuzz the sliding-window chunker with arbitrary text input.
///
/// Run with:
///   cargo +nightly fuzz run fuzz_chunker -- -max_total_time=600
///
/// Goal: find panics or out-of-bounds accesses in the chunking logic across
/// a range of window sizes and overlap values.
fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    // Vary window_size and overlap using prefix bytes so many combinations
    // are exercised without needing separate targets.
    let window = 40 + (data.first().copied().unwrap_or(0) as usize % 200);
    let overlap = data.get(1).copied().unwrap_or(0) as usize % (window / 2).max(1);
    let _ = sliding_window(s, "fuzz_input", "text", window, overlap);
});
