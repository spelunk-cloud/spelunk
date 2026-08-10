pub mod dates;

/// If `root` (or any ancestor) is a git linked worktree, returns the main
/// worktree root. Otherwise returns `root` itself.
///
/// Tries gix first (no subprocess), then falls back to reading the `.git` file
/// at `root` directly. The fallback covers the case where `root` is exactly the
/// worktree root even when gix cannot fully open the repository.
pub fn resolve_main_worktree_root(root: &std::path::Path) -> std::path::PathBuf {
    // ── gix path ─────────────────────────────────────────────────────────────
    // gix::discover walks up from root, so it works whether root is the
    // worktree root itself or a subdirectory inside it.
    if let Ok(repo) = gix::discover(root) {
        let git_dir = repo.git_dir();
        // A linked worktree has git_dir at: <main>/.git/worktrees/<name>
        // Parent of git_dir is the "worktrees" directory.
        if let Some(parent) = git_dir.parent()
            && parent.file_name() == Some(std::ffi::OsStr::new("worktrees"))
        {
            // parent.parent() == <main>/.git ; its parent == <main>
            if let Some(main_root) = parent.parent().and_then(|p| p.parent())
                && main_root != root
            {
                return main_root.to_path_buf();
            }
        }
        return root.to_path_buf();
    }

    // ── fallback: parse .git file at root directly ────────────────────────
    let git = root.join(".git");
    if !git.is_file() {
        return root.to_path_buf();
    }
    let Ok(content) = std::fs::read_to_string(&git) else {
        return root.to_path_buf();
    };
    // Format: "gitdir: /abs/path/to/main/.git/worktrees/<name>\n"
    let Some(gitdir_str) = content.strip_prefix("gitdir:") else {
        return root.to_path_buf();
    };
    let gitdir = std::path::PathBuf::from(gitdir_str.trim());
    let Some(worktrees_dir) = gitdir.parent() else {
        return root.to_path_buf();
    };
    if worktrees_dir.file_name() != Some(std::ffi::OsStr::new("worktrees")) {
        return root.to_path_buf();
    }
    let Some(main_root) = worktrees_dir.parent().and_then(|p| p.parent()) else {
        return root.to_path_buf();
    };
    if main_root == root {
        return root.to_path_buf();
    }
    main_root.to_path_buf()
}

/// Returns true when the process is running in agent mode (`AGENT=true`).
///
/// In agent mode all output defaults to structured JSON and progress spinners
/// are suppressed so that stdout is machine-readable.
pub fn is_agent_mode() -> bool {
    std::env::var("AGENT").as_deref() == Ok("true")
}

/// Return the effective output format string.
///
/// When agent mode is active, overrides `"text"` with `"json"` so that every
/// command with a `--format` flag produces machine-readable output without the
/// caller needing to pass `--format json` explicitly.
pub fn effective_format(format: &str) -> &str {
    if is_agent_mode() && format == "text" {
        "json"
    } else {
        format
    }
}

/// Quote a user-supplied search term as a literal FTS5 string, so it is
/// always treated as plain text rather than FTS5 query syntax.
///
/// FTS5 `MATCH` interprets punctuation like `"`, `:`, `(`, `)`, `*`, `-`,
/// `OR`/`AND`/`NOT` as query-language syntax. Passing a user's raw search
/// term straight through can throw a parse error (which would otherwise leak
/// as a raw internal error to the caller) or silently change the query's
/// meaning. Wrapping the whole term in double quotes forces FTS5 to treat it
/// as one literal string token; any internal `"` is escaped by doubling it
/// (`"` → `""`), per FTS5's string-literal escaping rule. Advanced FTS query
/// syntax (column filters, boolean operators, prefix matching, …) is
/// intentionally not exposed — every term is always literal.
///
/// An empty term quotes to `""`, which is a valid (empty) FTS5 string that
/// simply matches nothing.
///
/// Embedded NUL bytes (`\0`) are stripped before quoting. FTS5's query-string
/// parser (independent of SQLite's NUL-safe TEXT binding) treats `\0` as an
/// early string terminator, so the closing `"` this function appends would
/// never be seen by FTS5 and a raw "unterminated string" parse error would
/// leak to the caller instead of being treated as part of the literal.
pub fn fts5_quote_literal(term: &str) -> String {
    let mut out = String::with_capacity(term.len() + 2);
    out.push('"');
    for c in term.chars().filter(|&c| c != '\0') {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Build an FTS5 `MATCH` expression that scores a multi-word query's terms
/// **independently**, so it ranks documents that contain the terms regardless
/// of their order or adjacency (BM25 over a bag of words) — rather than
/// requiring the whole query to appear as one contiguous, ordered phrase.
///
/// The query is split into whitespace-separated terms; each term is quoted as a
/// literal via [`fts5_quote_literal`] (so FTS5 query punctuation and the
/// boolean/`NEAR` keywords inside user input stay literal and never parse as
/// query syntax), and the quoted terms are combined with the FTS5 `OR`
/// operator. `OR` — not `AND` — keeps a document that matches only some of the
/// terms in the candidate set, so BM25 can rank a document matching more of the
/// query terms above one matching fewer, instead of dropping the partial match
/// entirely.
///
/// Tokenisation and stemming are exactly whatever the target table's FTS5
/// tokenizer already applies — the default `unicode61` (case-folded, no
/// stemming) — because every term is matched through that same tokenizer; this
/// function never invents its own token rules.
///
/// A query with no terms (empty or all-whitespace) yields `""`, a valid FTS5
/// literal that simply matches nothing — never a parse error.
pub fn fts5_match_query(query: &str) -> String {
    let mut terms = query.split_whitespace();
    let Some(first) = terms.next() else {
        return String::from("\"\"");
    };
    let mut out = fts5_quote_literal(first);
    for term in terms {
        out.push_str(" OR ");
        out.push_str(&fts5_quote_literal(term));
    }
    out
}

/// Strip ANSI escape sequences and unsafe control characters from a string.
///
/// Allows newline, carriage return, and tab. Strips all other C0 control
/// characters, DEL, and ANSI/VT escape sequences (CSI, OSC, two-char).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                match chars.peek().copied() {
                    Some('[') => {
                        // CSI sequence: ESC [ <params> <final 0x40–0x7E>
                        chars.next();
                        for c2 in chars.by_ref() {
                            if ('\x40'..='\x7e').contains(&c2) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        // OSC sequence: ESC ] <text> ST  (ST = BEL or ESC \)
                        chars.next();
                        loop {
                            match chars.next() {
                                None | Some('\x07') => break,
                                Some('\x1b') => {
                                    chars.next();
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {
                        // Two-char sequence: ESC <char>
                        chars.next();
                    }
                }
            }
            '\n' | '\r' | '\t' => out.push(c),
            c if (c as u32) < 0x20 || c == '\x7f' => { /* drop */ }
            c => out.push(c),
        }
    }
    out
}

/// Canonicalize a path to its most compatible absolute form.
///
/// Wraps [`dunce::canonicalize`], which resolves symlinks like
/// `std::fs::canonicalize` but returns the de-UNC'd path on Windows — without the
/// `\\?\` verbatim prefix that `std::fs::canonicalize` adds (and correctly keeps
/// the prefix for genuine UNC paths). Canonical project paths are stored in the
/// registry; the lookup side derives the current project root via gix
/// ([`resolve_main_worktree_root`]), which yields plain `C:\…` paths, so a
/// `\\?\`-prefixed registry entry would never match on Windows. Falls back to the
/// input path if canonicalization fails (e.g. the path does not exist yet). On
/// macOS/Linux this behaves exactly like `std::fs::canonicalize`.
pub fn canonicalize(path: &std::path::Path) -> std::path::PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Normalize a file path's separators to forward slashes for indexing + lookup.
///
/// The index stores root-relative paths so it is portable across machines. On
/// Windows `Path::to_string_lossy()` yields backslash separators (`src\lib.rs`),
/// which would not match the forward-slash paths used everywhere else — CLI
/// arguments, `LIKE` lookups (where `\` is also the SQL escape char), and
/// indexes built on other OSes. Canonicalizing to `/` keeps the on-disk index
/// identical regardless of the indexing host and makes path lookups
/// OS-independent. On Unix this is a no-op for ordinary paths.
pub fn normalize_index_path(path: &str) -> String {
    path.replace('\\', "/")
}

/// Format a Unix timestamp as a human-readable age string (e.g. "3 min ago").
pub fn format_age(created_at: i64) -> String {
    let secs = (chrono::Utc::now().timestamp() - created_at).max(0);
    if secs < 90 {
        format!("{secs} sec ago")
    } else if secs < 3600 {
        format!("{} min ago", secs / 60)
    } else if secs < 86400 {
        format!("{} hr ago", secs / 3600)
    } else {
        format!("{} days ago", secs / 86400)
    }
}

/// Collect files modified or untracked relative to HEAD using gix.
/// Returns an empty set on any error (graceful degradation).
pub fn worktree_modified_files() -> std::collections::HashSet<String> {
    let Ok(repo) = gix::discover(".") else {
        return std::collections::HashSet::new();
    };
    let Ok(platform) = repo.status(gix::progress::Discard) else {
        return std::collections::HashSet::new();
    };
    let Ok(iter) = platform.into_iter(Vec::<gix::bstr::BString>::new()) else {
        return std::collections::HashSet::new();
    };
    iter.filter_map(|res| res.ok())
        .map(|item| item.location().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── resolve_main_worktree_root ────────────────────────────────────────────

    #[test]
    fn resolve_main_worktree_root_linked_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let main_root = tmp.path().join("main");
        let wt_root = tmp.path().join("feat-branch");
        std::fs::create_dir_all(&main_root).unwrap();
        std::fs::create_dir_all(&wt_root).unwrap();

        // Simulate: main_root/.git/worktrees/feat-branch (the gitdir entry)
        let gitdir_entry = main_root.join(".git").join("worktrees").join("feat-branch");
        std::fs::create_dir_all(&gitdir_entry).unwrap();

        // wt_root/.git is a file pointing to the worktrees entry
        std::fs::write(
            wt_root.join(".git"),
            format!("gitdir: {}\n", gitdir_entry.display()),
        )
        .unwrap();

        let resolved = resolve_main_worktree_root(&wt_root);
        assert_eq!(resolved, main_root);
    }

    #[test]
    fn resolve_main_worktree_root_normal_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("myrepo");
        // Normal repo: .git is a directory, not a file
        std::fs::create_dir_all(repo_root.join(".git")).unwrap();

        let resolved = resolve_main_worktree_root(&repo_root);
        assert_eq!(resolved, repo_root);
    }

    #[test]
    fn resolve_main_worktree_root_no_git() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_main_worktree_root(tmp.path());
        assert_eq!(resolved, tmp.path());
    }

    // ── canonicalize ───────────────────────────────────────────────────────

    /// `canonicalize`'s whole reason to exist is de-UNCing: it must never
    /// return the verbatim `\\?\`-prefixed form that `std::fs::canonicalize`
    /// produces on Windows for a real, existing directory. A caller that
    /// builds one path through this wrapper and another through
    /// `std::fs::canonicalize`/`Path::canonicalize` directly is comparing two
    /// different spellings of the same directory - `PathBuf` equality (and
    /// `assert_eq!`) then fails even though both name the identical real
    /// path. This is exactly the mismatch that broke the hooks-dir tests in
    /// `crates/spelunk-cli/src/cli/cmd/hooks.rs`: a test built its "expected"
    /// value via the raw std canonicalize while production went through this
    /// wrapper.
    #[test]
    #[cfg(windows)]
    fn canonicalize_never_returns_the_verbatim_unc_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("real");
        std::fs::create_dir_all(&dir).unwrap();

        let std_form = dir.canonicalize().unwrap();
        let wrapped_form = canonicalize(&dir);

        assert!(
            std_form.to_string_lossy().starts_with(r"\\?\"),
            "setup: std::fs::canonicalize is expected to add the verbatim \
             prefix on Windows, got: {}",
            std_form.display()
        );
        assert!(
            !wrapped_form.to_string_lossy().starts_with(r"\\?\"),
            "canonicalize() must strip the verbatim prefix, got: {}",
            wrapped_form.display()
        );
        // Same real directory, so re-canonicalizing the std form through the
        // wrapper must land on the same result the wrapper gives directly -
        // proving the two flavors name one identical path rather than two.
        assert_eq!(canonicalize(&std_form), wrapped_form);
    }

    // ── normalize_index_path ──────────────────────────────────────────────────

    #[test]
    fn normalize_index_path_converts_backslashes() {
        assert_eq!(normalize_index_path("src\\lib.rs"), "src/lib.rs");
        assert_eq!(normalize_index_path("a\\b\\c.rs"), "a/b/c.rs");
        // already-forward-slash and bare names are unchanged
        assert_eq!(normalize_index_path("src/lib.rs"), "src/lib.rs");
        assert_eq!(normalize_index_path("lib.rs"), "lib.rs");
    }

    // ── fts5_quote_literal ───────────────────────────────────────────────────

    #[test]
    fn fts5_quote_plain_term() {
        assert_eq!(fts5_quote_literal("hello world"), "\"hello world\"");
    }

    #[test]
    fn fts5_quote_escapes_internal_quotes() {
        assert_eq!(fts5_quote_literal("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn fts5_quote_colon_term_stays_literal() {
        // `foo:` looks like an FTS5 column filter; quoting keeps it literal.
        assert_eq!(fts5_quote_literal("foo:bar"), "\"foo:bar\"");
    }

    #[test]
    fn fts5_quote_boolean_keywords_stay_literal() {
        assert_eq!(fts5_quote_literal("a OR NOT b"), "\"a OR NOT b\"");
    }

    #[test]
    fn fts5_quote_empty_term() {
        assert_eq!(fts5_quote_literal(""), "\"\"");
    }

    #[test]
    fn fts5_quote_near_keyword_stays_literal() {
        assert_eq!(fts5_quote_literal("a NEAR/3 b"), "\"a NEAR/3 b\"");
    }

    #[test]
    fn fts5_quote_consecutive_internal_quotes_all_escaped() {
        // Every internal `"` must be doubled, including runs of them.
        assert_eq!(fts5_quote_literal("\"\"\""), "\"\"\"\"\"\"\"\"");
    }

    #[test]
    fn fts5_quote_embedded_nul_is_stripped() {
        let term = "before\0after";
        let quoted = fts5_quote_literal(term);
        assert_eq!(quoted, "\"beforeafter\"");
    }

    // ── fts5_match_query ──────────────────────────────────────────────────────

    #[test]
    fn fts5_match_multi_word_joins_literal_terms_with_or() {
        // Each word is a separate quoted literal combined with OR, so the query
        // matches the terms independently rather than as one ordered phrase.
        assert_eq!(fts5_match_query("leaky bucket"), "\"leaky\" OR \"bucket\"");
        assert_eq!(
            fts5_match_query("token bucket bursts"),
            "\"token\" OR \"bucket\" OR \"bursts\""
        );
    }

    #[test]
    fn fts5_match_single_word_is_one_literal_no_or() {
        assert_eq!(fts5_match_query("bucket"), "\"bucket\"");
    }

    #[test]
    fn fts5_match_empty_or_whitespace_matches_nothing() {
        // No terms → a single empty literal: valid FTS5 that matches nothing,
        // never a parse error.
        assert_eq!(fts5_match_query(""), "\"\"");
        assert_eq!(fts5_match_query("   \t\n "), "\"\"");
    }

    #[test]
    fn fts5_match_boolean_keywords_stay_literal_terms() {
        // User words that look like FTS5 operators are quoted per-term, so only
        // the injected separators are ever real OR operators.
        assert_eq!(
            fts5_match_query("a OR NOT b"),
            "\"a\" OR \"OR\" OR \"NOT\" OR \"b\""
        );
    }

    #[test]
    fn fts5_match_escapes_and_strips_per_term() {
        // Per-term delegation to fts5_quote_literal: internal quotes doubled,
        // embedded NUL stripped.
        assert_eq!(
            fts5_match_query("say\"hi there"),
            "\"say\"\"hi\" OR \"there\""
        );
        assert_eq!(
            fts5_match_query("be\0fore after"),
            "\"before\" OR \"after\""
        );
    }

    // ── strip_ansi ────────────────────────────────────────────────────────────

    #[test]
    fn strips_csi_colour() {
        assert_eq!(strip_ansi("\x1b[1;32mhello\x1b[0m"), "hello");
    }

    #[test]
    fn strips_osc() {
        assert_eq!(strip_ansi("\x1b]0;title\x07text"), "text");
    }

    #[test]
    fn preserves_newlines_and_tabs() {
        assert_eq!(strip_ansi("line1\nline2\ttabbed"), "line1\nline2\ttabbed");
    }

    #[test]
    fn strips_lone_c0_controls() {
        assert_eq!(strip_ansi("a\x01\x08b"), "ab");
    }

    #[test]
    fn clean_string_unchanged() {
        let s = "hello, world! 123";
        assert_eq!(strip_ansi(s), s);
    }
}
