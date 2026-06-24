#![no_main]

use libfuzzer_sys::fuzz_target;
use spelunk_core::indexer::parser::SourceParser;

/// Fuzz `SourceParser::parse` with arbitrary bytes across all supported languages.
///
/// Run with:
///   cargo +nightly fuzz run fuzz_parser -- -max_total_time=600
///
/// The fuzzer looks for panics or OOM. Tree-sitter is memory-safe by design,
/// but this catches regressions in our own chunking logic.
fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };

    // Rotate through all supported languages based on a prefix byte so every
    // grammar gets exercised without needing separate fuzz targets.
    // Kept in sync with SUPPORTED_LANGUAGES in src/indexer/parser/mod.rs.
    let languages = &[
        "rust", "python", "javascript", "jsx", "typescript", "tsx",
        "go", "java", "c", "cpp", "json", "html", "css", "hcl",
        "sql", "proto", "markdown", "text", "notebook",
    ];
    let lang_idx = data.first().copied().unwrap_or(0) as usize % languages.len();
    let language = languages[lang_idx];

    // We only care that the parser doesn't panic or abort — return value ignored.
    let _ = SourceParser::parse(s, "fuzz_input", language);
});
