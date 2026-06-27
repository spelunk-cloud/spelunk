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

    // ── normalize_index_path ──────────────────────────────────────────────────

    #[test]
    fn normalize_index_path_converts_backslashes() {
        assert_eq!(normalize_index_path("src\\lib.rs"), "src/lib.rs");
        assert_eq!(normalize_index_path("a\\b\\c.rs"), "a/b/c.rs");
        // already-forward-slash and bare names are unchanged
        assert_eq!(normalize_index_path("src/lib.rs"), "src/lib.rs");
        assert_eq!(normalize_index_path("lib.rs"), "lib.rs");
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
