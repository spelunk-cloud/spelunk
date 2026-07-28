//! Unit tests for the chunker module (no I/O, no SQLite).

use spelunk_core::indexer::chunker::MAX_CHUNK_TOKENS;
use spelunk_core::indexer::{Chunk, ChunkKind, SourceParser};
use spelunk_core::search::tokens::estimate_tokens;

// ── sliding_window (token-aware) ─────────────────────────────────────────────

use spelunk_core::indexer::sliding_window;

/// One line of `chars` visible characters (no trailing newline).
fn line_of(chars: usize) -> String {
    "x".repeat(chars)
}

#[test]
fn sliding_window_single_chunk_when_file_fits() {
    let src = "line1\nline2\nline3";
    let chunks = sliding_window(src, "test.txt", "text", None, None, None);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].start_line, 1);
    assert_eq!(chunks[0].end_line, 3);
    assert_eq!(chunks[0].content, "line1\nline2\nline3");
}

#[test]
fn sliding_window_empty_source_returns_no_chunks() {
    let chunks = sliding_window("", "test.txt", "text", None, None, None);
    assert!(chunks.is_empty());
}

#[test]
fn sliding_window_all_chunks_are_verbatim() {
    // Long-line content forces multiple windows.
    let src = vec![line_of(400); 60].join("\n");
    let chunks = sliding_window(&src, "f.txt", "text", None, None, None);
    assert!(
        chunks.len() > 1,
        "long-line content must split into >1 window"
    );
    for c in &chunks {
        assert!(matches!(c.kind, ChunkKind::Verbatim));
    }
}

#[test]
fn sliding_window_multi_line_windows_respect_token_budget() {
    // 60 lines × 400 chars = 24_000 chars ≈ 6_000 tokens; a fixed 120-line window
    // would emit one ~6k-token chunk. Token-aware windowing must keep every
    // multi-line window at or under the cap; only a lone over-budget line may
    // exceed it (none here — each line is ~100 tokens).
    let src = vec![line_of(400); 60].join("\n");
    let chunks = sliding_window(&src, "gen.ts", "typescript", None, None, None);
    for c in &chunks {
        let toks = estimate_tokens(&c.content);
        let single_line = c.content.lines().count() <= 1;
        assert!(
            toks <= MAX_CHUNK_TOKENS || single_line,
            "window {}-{} has {toks} tokens (> cap) but is not a lone line",
            c.start_line,
            c.end_line,
        );
    }
}

#[test]
fn sliding_window_over_budget_single_line_becomes_its_own_window() {
    // One 20_000-char line ≈ 5_000 tokens, far over the 2_048 cap. It cannot be
    // split on line boundaries, so it must be emitted as a single window rather
    // than looping forever — forward progress is the guarantee under test.
    let src = line_of(20_000);
    let chunks = sliding_window(&src, "min.js", "javascript", None, None, None);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].start_line, 1);
    assert_eq!(chunks[0].end_line, 1);
    assert!(estimate_tokens(&chunks[0].content) > MAX_CHUNK_TOKENS);
}

#[test]
fn sliding_window_adjacent_windows_overlap() {
    // Lines short relative to the cap (so the overlap budget, 1/8th of the
    // window budget, still spans several lines regardless of the configured
    // cap) and enough of them to span several windows. Each window after the
    // first must start on or before the previous window's last line
    // (overlap), and start strictly after the previous window's start
    // (forward progress).
    let line_len = 40;
    let total_lines = (MAX_CHUNK_TOKENS * 4 / line_len) * 5;
    let src = vec![line_of(line_len); total_lines].join("\n");
    let chunks = sliding_window(&src, "f.txt", "text", None, None, None);
    assert!(
        chunks.len() >= 3,
        "expected several windows, got {}",
        chunks.len()
    );
    for pair in chunks.windows(2) {
        let (prev, next) = (&pair[0], &pair[1]);
        assert!(
            next.start_line <= prev.end_line,
            "window at {} does not overlap previous ending at {}",
            next.start_line,
            prev.end_line,
        );
        assert!(
            next.start_line > prev.start_line,
            "windows must advance: {} !> {}",
            next.start_line,
            prev.start_line,
        );
    }
}

#[test]
fn sliding_window_threads_identity_onto_every_subchunk() {
    let src = vec![line_of(400); 40].join("\n");
    let chunks = sliding_window(
        &src,
        "f.rs",
        "rust",
        Some("my_fn"),
        Some("/// does a thing"),
        Some("impl Foo"),
    );
    assert!(chunks.len() > 1, "fixture should span multiple windows");
    for c in &chunks {
        assert_eq!(c.name.as_deref(), Some("my_fn"));
        assert_eq!(c.docstring.as_deref(), Some("/// does a thing"));
        assert_eq!(c.parent_scope.as_deref(), Some("impl Foo"));
        // Identity reaches the embedding text — no `title: none`.
        assert!(c.embedding_text().starts_with("title: my_fn |"));
        assert!(!c.embedding_text().contains("title: none"));
    }
}

// ── tree-sitter docstring extraction (preceding_comment) ────────────────────

#[test]
fn docstring_captured_for_plain_function() {
    let src = "/// does a thing\nfn plain() {}\n";
    let chunks = SourceParser::parse(src, "f.rs", "rust").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("plain"))
        .unwrap();
    assert_eq!(f.docstring.as_deref(), Some("/// does a thing\n"));
}

#[test]
fn docstring_captured_across_a_single_attribute() {
    let src = "/// does a thing\n#[allow(dead_code)]\nfn attributed() {}\n";
    let chunks = SourceParser::parse(src, "f.rs", "rust").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("attributed"))
        .unwrap();
    assert_eq!(f.docstring.as_deref(), Some("/// does a thing\n"));
}

#[test]
fn docstring_captured_across_stacked_attributes() {
    let src = "/// does a thing\n#[derive(Debug)]\n#[allow(dead_code)]\nstruct Stacked;\n";
    let chunks = SourceParser::parse(src, "f.rs", "rust").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("Stacked"))
        .unwrap();
    assert_eq!(f.docstring.as_deref(), Some("/// does a thing\n"));
}

#[test]
fn no_docstring_when_attribute_has_none_above_it() {
    let src = "fn other() {}\n#[allow(dead_code)]\nfn attributed() {}\n";
    let chunks = SourceParser::parse(src, "f.rs", "rust").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("attributed"))
        .unwrap();
    assert_eq!(f.docstring, None);
}

// ── Chunk::embedding_text ────────────────────────────────────────────────────

fn make_chunk(name: Option<&str>, docstring: Option<&str>, content: &str) -> Chunk {
    Chunk {
        file_path: "src/lib.rs".into(),
        language: "rust".into(),
        kind: ChunkKind::Function,
        name: name.map(str::to_string),
        start_line: 1,
        end_line: 5,
        content: content.to_string(),
        docstring: docstring.map(str::to_string),
        parent_scope: None,
        summary: None,
    }
}

#[test]
fn embedding_text_with_name() {
    let c = make_chunk(Some("my_fn"), None, "fn my_fn() {}");
    assert_eq!(c.embedding_text(), "title: my_fn | text: fn my_fn() {}");
}

#[test]
fn embedding_text_without_name_uses_none() {
    let c = make_chunk(None, None, "let x = 1;");
    assert_eq!(c.embedding_text(), "title: none | text: let x = 1;");
}

#[test]
fn embedding_text_prepends_docstring() {
    let c = make_chunk(Some("foo"), Some("/// Does foo."), "fn foo() {}");
    assert_eq!(
        c.embedding_text(),
        "title: foo | text: /// Does foo.\nfn foo() {}"
    );
}

// ── MAX_CHUNK_TOKENS ceiling ─────────────────────────────────────────────────

/// A Rust function with `body_lines` short statements, guaranteed short enough
/// per line that any 120-line window stays under the cap.
fn big_rust_fn(name: &str, body_lines: usize) -> String {
    let mut s = format!("fn {name}() {{\n");
    for i in 0..body_lines {
        s.push_str(&format!("    let v{i} = {i};\n"));
    }
    s.push_str("}\n");
    s
}

#[test]
fn oversized_leaf_splits_into_capped_subchunks() {
    // 600 short lines ≈ 2.4k tokens for the function — over the cap.
    let src = big_rust_fn("huge", 600);
    assert!(
        estimate_tokens(&src) > MAX_CHUNK_TOKENS,
        "fixture must exceed cap"
    );

    let chunks = SourceParser::parse(&src, "huge.rs", "rust").unwrap();

    // No single whole-function chunk survives; it is re-windowed.
    assert!(
        chunks.len() > 1,
        "oversized leaf should split into >1 chunk"
    );
    assert!(
        chunks.iter().all(|c| matches!(c.kind, ChunkKind::Verbatim)),
        "re-windowed sub-chunks are Verbatim"
    );
    for c in &chunks {
        assert!(
            estimate_tokens(&c.content) <= MAX_CHUNK_TOKENS,
            "sub-chunk {}-{} over cap: {} tok",
            c.start_line,
            c.end_line,
            estimate_tokens(&c.content)
        );
        // Identity re-attached: the re-windowed function keeps its name, so it
        // embeds as `title: huge`, not `title: none`.
        assert_eq!(c.name.as_deref(), Some("huge"));
        assert!(c.embedding_text().starts_with("title: huge |"));
    }
    // Line offset preserved: the function starts at file line 1.
    assert_eq!(chunks[0].start_line, 1);
}

#[test]
fn oversized_markdown_section_windows_keep_heading_as_name() {
    // A single heading whose body is over the cap → windowed, each window named
    // after the heading rather than degrading to `title: none`.
    let mut src = String::from("# Big Section\n");
    for i in 0..1200 {
        src.push_str(&format!(
            "prose line number {i} with some filler words here\n"
        ));
    }
    assert!(
        estimate_tokens(&src) > MAX_CHUNK_TOKENS,
        "markdown fixture must exceed cap"
    );

    let chunks = SourceParser::parse(&src, "doc.md", "markdown").unwrap();
    assert!(chunks.len() > 1, "oversized section should window");
    for c in &chunks {
        assert_eq!(c.name.as_deref(), Some("Big Section"));
        assert!(c.embedding_text().starts_with("title: Big Section |"));
        let single_line = c.content.lines().count() <= 1;
        assert!(estimate_tokens(&c.content) <= MAX_CHUNK_TOKENS || single_line);
    }
}

#[test]
fn oversized_container_suppresses_own_chunk_keeps_children() {
    // A module whose whole text is over the cap, but each child fn is under
    // it. `body_lines` is derived from `MAX_CHUNK_TOKENS` (not a hardcoded
    // line count) so both fixture properties keep holding regardless of the
    // configured cap: one child alone must fit under it, five together must not.
    let body_lines = ((MAX_CHUNK_TOKENS / 3) / 5).max(10);
    assert!(
        estimate_tokens(&big_rust_fn("f0", body_lines)) <= MAX_CHUNK_TOKENS,
        "fixture assumption broken: a single child function must fit under the cap"
    );

    let mut src = String::from("mod tests {\n");
    for i in 0..5 {
        src.push_str(&big_rust_fn(&format!("f{i}"), body_lines));
    }
    src.push_str("}\n");
    assert!(
        estimate_tokens(&src) > MAX_CHUNK_TOKENS,
        "module fixture must exceed cap"
    );

    let chunks = SourceParser::parse(&src, "container.rs", "rust").unwrap();

    // Container's own Module chunk is suppressed.
    assert!(
        !chunks.iter().any(|c| matches!(c.kind, ChunkKind::Module)),
        "oversized container must not emit its own chunk"
    );
    // But per-fn child chunks are still emitted, each under the cap.
    let fns: Vec<&Chunk> = chunks
        .iter()
        .filter(|c| matches!(c.kind, ChunkKind::Function))
        .collect();
    assert_eq!(fns.len(), 5, "each child function should yield a chunk");
    for c in &fns {
        assert!(estimate_tokens(&c.content) <= MAX_CHUNK_TOKENS);
    }
    let names: Vec<&str> = fns.iter().filter_map(|c| c.name.as_deref()).collect();
    assert!(names.contains(&"f0") && names.contains(&"f4"));
}

#[test]
fn docstring_captured_across_a_python_decorator() {
    let src = "# does a thing\n@staticmethod\ndef attributed():\n    pass\n";
    let chunks = SourceParser::parse(src, "f.py", "python").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("attributed"))
        .unwrap();
    // Unlike Rust's `line_comment`, Python's `comment` token excludes the trailing newline.
    assert_eq!(f.docstring.as_deref(), Some("# does a thing"));
}

#[test]
fn docstring_captured_across_stacked_python_decorators() {
    // Stacked decorators share one `decorated_definition` wrapper, not one each.
    let src = "# does a thing\n@foo\n@bar\ndef attributed():\n    pass\n";
    let chunks = SourceParser::parse(src, "f.py", "python").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("attributed"))
        .unwrap();
    assert_eq!(f.docstring.as_deref(), Some("# does a thing"));
}

#[test]
fn no_docstring_when_python_decorator_has_none_above_it() {
    let src = "def other():\n    pass\n@staticmethod\ndef attributed():\n    pass\n";
    let chunks = SourceParser::parse(src, "f.py", "python").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("attributed"))
        .unwrap();
    assert_eq!(f.docstring, None);
}

// ── docstring on the first documented member of a block ──
//
// tree-sitter-python attaches a comment that leads the first statement of a
// class/function body as a child of the *enclosing* class_definition /
// function_definition node (a sibling of `body`), not as the first child
// inside the `block` itself. Only the first member is affected; once there's
// an earlier statement in the block, the comment is already a normal sibling
// within it.

#[test]
fn docstring_captured_for_first_member_of_class_body() {
    let src = "class Outer:\n    # inner\n    def attributed():\n        pass\n";
    let chunks = SourceParser::parse(src, "f.py", "python").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("attributed"))
        .unwrap();
    assert_eq!(f.docstring.as_deref(), Some("# inner"));
}

#[test]
fn docstring_captured_for_first_decorated_member_of_class_body() {
    let src = "class Outer:\n    # inner\n    @staticmethod\n    def attributed():\n        pass\n";
    let chunks = SourceParser::parse(src, "f.py", "python").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("attributed"))
        .unwrap();
    assert_eq!(f.docstring.as_deref(), Some("# inner"));
}

#[test]
fn docstring_captured_for_first_statement_of_function_body() {
    let src = "def outer():\n    # inner\n    def attributed():\n        pass\n";
    let chunks = SourceParser::parse(src, "f.py", "python").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("attributed"))
        .unwrap();
    assert_eq!(f.docstring.as_deref(), Some("# inner"));
}

#[test]
fn docstring_captured_for_first_decorated_statement_of_function_body() {
    let src = "def outer():\n    # inner\n    @staticmethod\n    def attributed():\n        pass\n";
    let chunks = SourceParser::parse(src, "f.py", "python").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("attributed"))
        .unwrap();
    assert_eq!(f.docstring.as_deref(), Some("# inner"));
}

#[test]
fn no_docstring_for_first_member_of_class_body_when_none_precedes_it() {
    // First member, no comment at all — must not pick up an unrelated node
    // one level up (e.g. the class name or `:` token).
    let src = "class Outer:\n    def attributed():\n        pass\n";
    let chunks = SourceParser::parse(src, "f.py", "python").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("attributed"))
        .unwrap();
    assert_eq!(f.docstring, None);
}

#[test]
fn docstring_scopes_to_immediate_enclosing_block_across_three_levels() {
    // Guards the "bounded to one level" invariant: Inner's own doc must not
    // leak down to m, and m must still find its own doc one level up rather
    // than falling through to Outer's.
    let src = "class Outer:\n    # outer doc\n    class Inner:\n        # inner doc\n        def m():\n            pass\n";
    let chunks = SourceParser::parse(src, "f.py", "python").unwrap();
    let inner = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("Inner"))
        .unwrap();
    let m = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("m"))
        .unwrap();
    assert_eq!(inner.docstring.as_deref(), Some("# outer doc"));
    assert_eq!(m.docstring.as_deref(), Some("# inner doc"));
}

#[test]
fn docstring_captured_for_first_member_of_ruby_class_body() {
    // Same grammar quirk as Python's class_definition/function_definition:
    // tree-sitter-ruby attaches the leading comment as a child of `class`
    // itself rather than inside `body_statement`. preceding_comment() is
    // shared across languages, so the fix applies here too.
    let src = "class Outer\n  # inner\n  def attributed\n  end\nend\n";
    let chunks = SourceParser::parse(src, "f.rb", "ruby").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("attributed"))
        .unwrap();
    assert_eq!(f.docstring.as_deref(), Some("# inner"));
}

#[test]
fn docstring_captured_across_a_ruby_private_def_one_liner() {
    let src = "# does a thing\nprivate def attributed\nend\n";
    let chunks = SourceParser::parse(src, "f.rb", "ruby").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("attributed"))
        .unwrap();
    assert_eq!(f.docstring.as_deref(), Some("# does a thing"));
}

#[test]
fn docstring_captured_across_a_ruby_protected_def_one_liner() {
    let src = "# does a thing\nprotected def attributed\nend\n";
    let chunks = SourceParser::parse(src, "f.rb", "ruby").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("attributed"))
        .unwrap();
    assert_eq!(f.docstring.as_deref(), Some("# does a thing"));
}

#[test]
fn docstring_captured_across_a_plain_ruby_def_no_visibility_wrapper() {
    let src = "# does a thing\ndef attributed\nend\n";
    let chunks = SourceParser::parse(src, "f.rb", "ruby").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("attributed"))
        .unwrap();
    assert_eq!(f.docstring.as_deref(), Some("# does a thing"));
}

#[test]
fn docstring_captured_across_a_ruby_def_wrapped_by_any_sole_arg_call() {
    // Generalizes beyond the `private`/`protected`/`public` keywords: any
    // single-argument call wrapping a `def` (e.g. the `memoize` idiom from
    // the memoist gem) should get the same treatment, since it parses to
    // the identical `call(argument_list(method))` shape.
    let src = "# memoized on first call\nmemoize def attributed\nend\n";
    let chunks = SourceParser::parse(src, "f.rb", "ruby").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("attributed"))
        .unwrap();
    assert_eq!(f.docstring.as_deref(), Some("# memoized on first call"));
}

#[test]
fn no_docstring_misattached_when_def_is_one_of_several_call_arguments() {
    // Over-reach guard: a `def` merely happens to be one of several
    // arguments to an unrelated call (rare, but syntactically legal Ruby
    // since `def` is an expression). The preceding comment describes the
    // *call* as a whole, not specifically the nested `def`, so it must not
    // be attached to the method's docstring the way the sole-argument
    // `private def foo; end` idiom is.
    let src = "# describes the call, not attributed specifically\nsome_call(other_arg, def attributed\nend)\n";
    let chunks = SourceParser::parse(src, "f.rb", "ruby").unwrap();
    let f = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("attributed"))
        .unwrap();
    assert_eq!(f.docstring, None);
}
