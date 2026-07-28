use proptest::prelude::*;
use spelunk_core::indexer::chunker::{MAX_CHUNK_TOKENS, sliding_window};
use spelunk_core::search::tokens::estimate_tokens;

proptest! {
    // Every chunk's content must be a substring of the original source.
    #[test]
    fn chunks_are_substrings_of_source(source in "([a-z ]+\n){1,50}") {
        let chunks = sliding_window(&source, "test.txt", "text", None, None, None);
        for chunk in &chunks {
            prop_assert!(
                source.contains(chunk.content.trim()),
                "chunk content not found in source"
            );
        }
    }

    // Every window is within the token budget unless it is a single line that
    // alone exceeds it (the forward-progress escape hatch).
    #[test]
    fn windows_respect_token_budget(source in "([a-zA-Z0-9 ]{0,200}\n){1,120}") {
        let chunks = sliding_window(&source, "test.txt", "text", None, None, None);
        for chunk in &chunks {
            let over_budget = estimate_tokens(&chunk.content) > MAX_CHUNK_TOKENS;
            let single_line = chunk.content.lines().count() <= 1;
            prop_assert!(
                !over_budget || single_line,
                "window {}-{} exceeds the cap but is not a lone line",
                chunk.start_line,
                chunk.end_line,
            );
        }
    }

    // Windows always make forward progress and cover the source contiguously:
    // start lines are strictly increasing and never leave a gap wider than an
    // overlap (each next window starts on or before the previous window's end).
    #[test]
    fn windows_advance_and_stay_contiguous(source in "([a-z ]{0,120}\n){2,80}") {
        let chunks = sliding_window(&source, "test.txt", "text", None, None, None);
        for pair in chunks.windows(2) {
            prop_assert!(pair[1].start_line > pair[0].start_line, "windows must advance");
            prop_assert!(
                pair[1].start_line <= pair[0].end_line + 1,
                "gap between windows: {} after {}",
                pair[1].start_line,
                pair[0].end_line,
            );
        }
    }

    // Empty source always yields no chunks.
    #[test]
    fn empty_source_yields_no_chunks(_ in 0u8..=0) {
        let chunks = sliding_window("", "test.txt", "text", None, None, None);
        prop_assert!(chunks.is_empty());
    }
}
