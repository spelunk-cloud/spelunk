//! Adversarial / coverage-gap hardening for the token-aware re-window +
//! identity re-attach change (`sliding_window`, `push_windowed`,
//! `parse_markdown`'s oversized-section path).
//!
//! The engineer's own suite (`unit_chunker.rs`, `prop_chunker.rs`) proves the
//! cap-bound, forward-progress, overlap, and single-node identity-threading
//! behaviour. This file probes what a single-node happy path cannot catch:
//! cross-contamination between sibling oversized nodes in one file, the
//! genuinely-anonymous (no name/docstring) case, per-section attribution in
//! markdown with multiple oversized sections, and the worst documented
//! estimate/real-token bias.

use spelunk_core::indexer::chunker::{MAX_CHUNK_TOKENS, sliding_window};
use spelunk_core::indexer::{Chunk, ChunkKind, SourceParser};
use spelunk_core::search::tokens::estimate_tokens;

/// A Rust function whose body is `body_lines` short statements tagged with
/// `marker`, long enough in aggregate to exceed `MAX_CHUNK_TOKENS`, preceded
/// by a `///` doc comment containing `marker` so windows can be attributed
/// back to the right function by content alone.
fn big_rust_fn_with_doc(name: &str, marker: &str, body_lines: usize) -> String {
    let mut s = format!("/// {marker} docstring\nfn {name}() {{\n");
    for i in 0..body_lines {
        s.push_str(&format!("    let {marker}_{i} = {i};\n"));
    }
    s.push_str("}\n\n");
    s
}

// ── Multiple oversized siblings in one file ─────────────────────────────────

#[test]
fn multiple_oversized_siblings_keep_own_identity_no_cross_contamination() {
    // Two oversized functions with a small, non-oversized function sandwiched
    // between them. The middle function exercises the "stale value from a
    // previous iteration" risk: if identity were threaded through any shared
    // mutable state instead of being recomputed per node, it would leak here.
    let mut src = String::new();
    src.push_str(&big_rust_fn_with_doc("alpha_fn", "alpha", 600));
    src.push_str("fn middle_fn() {\n    let x = 1;\n}\n\n");
    src.push_str(&big_rust_fn_with_doc("gamma_fn", "gamma", 600));

    let chunks = SourceParser::parse(&src, "siblings.rs", "rust").unwrap();

    let alpha_windows: Vec<&Chunk> = chunks
        .iter()
        .filter(|c| c.content.contains("alpha_"))
        .collect();
    let gamma_windows: Vec<&Chunk> = chunks
        .iter()
        .filter(|c| c.content.contains("gamma_"))
        .collect();
    let middle: Vec<&Chunk> = chunks
        .iter()
        .filter(|c| c.name.as_deref() == Some("middle_fn"))
        .collect();

    assert!(alpha_windows.len() > 1, "alpha_fn fixture must be windowed");
    assert!(gamma_windows.len() > 1, "gamma_fn fixture must be windowed");
    assert_eq!(
        middle.len(),
        1,
        "middle_fn must remain a single un-windowed chunk"
    );

    for c in &alpha_windows {
        assert_eq!(
            c.name.as_deref(),
            Some("alpha_fn"),
            "alpha window mislabeled"
        );
        let doc = c.docstring.as_deref().unwrap_or("");
        assert!(
            doc.contains("alpha"),
            "alpha window lost its own docstring: {doc:?}"
        );
        assert!(
            !doc.contains("gamma"),
            "alpha window carries gamma's docstring: {doc:?}"
        );
        assert!(
            !c.content.contains("gamma_"),
            "alpha window leaked gamma's content"
        );
    }
    for c in &gamma_windows {
        assert_eq!(
            c.name.as_deref(),
            Some("gamma_fn"),
            "gamma window mislabeled"
        );
        let doc = c.docstring.as_deref().unwrap_or("");
        assert!(
            doc.contains("gamma"),
            "gamma window lost its own docstring: {doc:?}"
        );
        assert!(
            !doc.contains("alpha"),
            "gamma window carries alpha's docstring: {doc:?}"
        );
        assert!(
            !c.content.contains("alpha_"),
            "gamma window leaked alpha's content"
        );
    }
    assert_eq!(
        middle[0].docstring, None,
        "middle_fn must not inherit a sibling's docstring"
    );
}

// ── Anonymous oversized node (no name, no docstring) ────────────────────────

#[test]
fn anonymous_oversized_node_gets_none_identity_not_a_literal_none_string() {
    // A Rust `impl` block is a built-in case of a genuinely anonymous node:
    // `node_specs("rust")` maps `impl_item` with `name_field: None`, and rust
    // has no language-specific fallback in `extract_name` — an impl block's
    // `name` is always `None`. Fill one with only comments (no `fn`/`const`
    // children for the container recursion to match) and oversize it, so the
    // *impl block itself* is what re-windows, with no preceding doc comment
    // either. Confirms the windowing path degrades to `None`/`title: none`
    // gracefully rather than panicking or leaking a stray `Option::None`
    // debug artifact into the embedding text.
    let mut src = String::from("impl Foo {\n");
    for i in 0..1200 {
        src.push_str(&format!("    // filler comment line {i}\n"));
    }
    src.push_str("}\n");
    assert!(
        estimate_tokens(&src) > MAX_CHUNK_TOKENS,
        "fixture must exceed the cap"
    );

    let chunks = SourceParser::parse(&src, "anon.rs", "rust").unwrap();
    let windows: Vec<&Chunk> = chunks
        .iter()
        .filter(|c| matches!(c.kind, ChunkKind::Verbatim))
        .collect();
    assert!(
        !windows.is_empty(),
        "anonymous impl block body should be windowed"
    );

    for c in &windows {
        assert_eq!(c.name, None, "anonymous node must not synthesize a name");
        assert_eq!(c.docstring, None, "anonymous node has no preceding comment");
        // No panic constructing the embedding text (this would already have
        // panicked above if it did), and it degrades to the documented
        // lowercase `none` sentinel rather than a `Debug`-formatted `None`.
        let text = c.embedding_text();
        assert!(text.starts_with("title: none |"), "got: {text:?}");
        assert!(
            !text.contains("None"),
            "must not leak a literal Option::None debug artifact: {text:?}"
        );
    }
}

// ── Markdown: multiple oversized sections at different heading levels ──────

#[test]
fn markdown_multiple_oversized_sections_each_keep_own_heading() {
    let mut src = String::from("# Intro\nshort intro\n\n## Section Alpha\n");
    for i in 0..1200 {
        src.push_str(&format!("alpha filler line {i} with alphamarker text\n"));
    }
    src.push_str("\n### Section Beta\n");
    for i in 0..1200 {
        src.push_str(&format!("beta filler line {i} with betamarker text\n"));
    }
    assert!(
        estimate_tokens(&src) > MAX_CHUNK_TOKENS * 2,
        "fixture must give both sections room to exceed the cap"
    );

    let chunks = SourceParser::parse(&src, "doc.md", "markdown").unwrap();

    let alpha_windows: Vec<&Chunk> = chunks
        .iter()
        .filter(|c| c.content.contains("alphamarker"))
        .collect();
    let beta_windows: Vec<&Chunk> = chunks
        .iter()
        .filter(|c| c.content.contains("betamarker"))
        .collect();

    assert!(alpha_windows.len() > 1, "Section Alpha should window");
    assert!(beta_windows.len() > 1, "Section Beta should window");

    for c in &alpha_windows {
        assert_eq!(
            c.name.as_deref(),
            Some("Section Alpha"),
            "alpha window attributed to the wrong heading: {:?}",
            c.name
        );
        assert!(
            !c.content.contains("betamarker"),
            "alpha window leaked beta content"
        );
    }
    for c in &beta_windows {
        assert_eq!(
            c.name.as_deref(),
            Some("Section Beta"),
            "beta window attributed to the wrong heading: {:?}",
            c.name
        );
        assert!(
            !c.content.contains("alphamarker"),
            "beta window leaked alpha content"
        );
    }
}

// ── Worst-case estimate/real-token bias ─────────────────────────────────────

#[test]
fn worst_case_estimate_bias_still_bounds_windows_far_below_old_overshoot() {
    // `estimate_tokens` is `chars/4` with no tokenizer, so it cannot see a
    // real tokenizer's corpus-dependent bias — the architect's spike measured
    // up to 1.387 real/estimate on MDX. A window's *true* token count can
    // therefore run ~1.3-1.4x over the cap in the worst documented case; that
    // is the accepted trade-off (see `sliding_window`'s doc comment). What
    // must not happen is a return to the pre-fix behaviour, where a single
    // 120-line window measured 10_341 tokens against the 2_048 cap (~5x
    // overshoot) because nothing bounded it by content length at all.
    const WORST_CASE_BIAS: f64 = 1.387;
    const OLD_BUG_OVERSHOOT_TOKENS: usize = 10_341;

    // Dense, symbol-heavy, short-token content in the shape the bias applies
    // to (a real BPE tokenizer splits punctuation into extra tokens that
    // chars/4 has no way to see).
    let mut src = String::new();
    for i in 0..2000 {
        src.push_str(&format!(
            "<Prop key=\"k{i}\" value={{v{i}}} flag={{true}} data-x=\"{i}\"/>\n"
        ));
    }

    let chunks = sliding_window(&src, "dense.mdx", "mdx", None, None, None);
    assert!(
        chunks.len() > 1,
        "dense fixture must exceed a single window"
    );

    for c in &chunks {
        let est = estimate_tokens(&c.content) as f64;
        let projected_real = est * WORST_CASE_BIAS;
        assert!(
            projected_real <= MAX_CHUNK_TOKENS as f64 * 1.5,
            "window {}-{} projects {projected_real:.0} real tokens at the documented \
             worst-case bias — exceeds the accepted ~1.3-1.4x cap multiple",
            c.start_line,
            c.end_line,
        );
        assert!(
            (projected_real as usize) < OLD_BUG_OVERSHOOT_TOKENS,
            "window {}-{} projects {projected_real:.0} real tokens — must stay far below \
             the pre-fix 10_341-token single-window overshoot",
            c.start_line,
            c.end_line,
        );
    }
}

// ── Regression: non-oversized paths are unaffected ──────────────────────────

#[test]
fn small_function_is_not_windowed_and_keeps_direct_identity() {
    let src = "/// Adds one\nfn add_one(x: i32) -> i32 {\n    x + 1\n}\n";
    let chunks = SourceParser::parse(src, "small.rs", "rust").unwrap();
    assert_eq!(
        chunks.len(),
        1,
        "small function must not be routed through sliding_window"
    );
    assert_eq!(chunks[0].name.as_deref(), Some("add_one"));
    assert!(matches!(chunks[0].kind, ChunkKind::Function));
    // `preceding_comment` includes the trailing newline of the comment node's
    // own span; trim before comparing so the assertion is about content, not
    // that pre-existing (out of scope for this task) exact-span behaviour.
    assert_eq!(
        chunks[0].docstring.as_deref().map(str::trim_end),
        Some("/// Adds one")
    );
}
