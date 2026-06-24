use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use super::tools::{ToolCall, parse_tool_call, tool_call_schema};
use crate::{
    embeddings::{EmbeddingBackend, vec_to_blob},
    llm::{LlmBackend, Message},
    storage::Database,
};

/// A single tool invocation recorded during exploration.
#[derive(Debug, serde::Serialize)]
pub struct ExploreStep {
    pub step: usize,
    pub tool: String,
    pub args_summary: String,
    pub result_preview: String,
}

/// The final result returned by `Explorer::explore`.
#[derive(Debug, serde::Serialize)]
pub struct ExploreResult {
    pub answer: String,
    pub sources: Vec<String>,
    pub steps: Vec<ExploreStep>,
}

/// Drives the tool-use loop for `spelunk explore`.
///
/// Stores `db_path` and re-opens the database per tool call so that no
/// `&Database` borrow crosses an `.await` point (keeping futures `Send`).
pub struct Explorer<'a> {
    db_path: PathBuf,
    /// Canonicalized project root. `read_file` targets are confined to this
    /// directory. Computed once at construction. `None` if the supplied root
    /// could not be canonicalized (e.g. does not exist), in which case every
    /// `read_file` is denied.
    project_root: Option<PathBuf>,
    embedder: &'a (dyn EmbeddingBackend + 'a),
    llm: &'a (dyn LlmBackend + 'a),
    max_steps: usize,
    verbose: bool,
}

impl<'a> Explorer<'a> {
    pub fn new(
        db_path: PathBuf,
        project_root: PathBuf,
        embedder: &'a (dyn EmbeddingBackend + 'a),
        llm: &'a (dyn LlmBackend + 'a),
        max_steps: usize,
        verbose: bool,
    ) -> Self {
        // Canonicalize the root once so the per-call symlink backstop is a cheap
        // `starts_with` comparison rather than a repeated filesystem walk.
        let project_root = std::fs::canonicalize(&project_root).ok();
        Self {
            db_path,
            project_root,
            embedder,
            llm,
            max_steps,
            verbose,
        }
    }

    pub async fn explore(&self, question: &str) -> Result<ExploreResult> {
        let schema = tool_call_schema();
        let mut messages = vec![
            Message::system(SYSTEM_PROMPT),
            Message::user(format!(
                "Question: {question}\n\n\
                 Begin exploring. Use tools to find relevant code, then call done with your answer."
            )),
        ];
        let mut steps: Vec<ExploreStep> = Vec::new();
        let mut sources: HashSet<String> = HashSet::new();

        for step_num in 1..=self.max_steps {
            let raw = self.call_llm(&messages, &schema).await?;
            let raw = crate::utils::strip_ansi(&raw);

            if self.verbose {
                eprintln!("\n\x1b[2m[step {step_num}] {}\x1b[0m", raw.trim());
            }

            messages.push(Message {
                role: "assistant".into(),
                content: raw.clone(),
            });

            let tool_call = match parse_tool_call(&raw) {
                Some(tc) => tc,
                None => {
                    // Unparseable output — treat as final answer.
                    return Ok(ExploreResult {
                        answer: raw.trim().to_string(),
                        sources: sorted(sources),
                        steps,
                    });
                }
            };

            if let ToolCall::Done { answer } = tool_call {
                return Ok(ExploreResult {
                    answer,
                    sources: sorted(sources),
                    steps,
                });
            }

            let tool_name = tool_call.name();
            let (args_summary, result) = self.execute(&tool_call, &mut sources).await?;
            let result_preview: String = result.chars().take(200).collect();

            if self.verbose {
                eprintln!("\x1b[2m  → {result_preview}\x1b[0m");
            }

            steps.push(ExploreStep {
                step: step_num,
                tool: tool_name.to_string(),
                args_summary,
                result_preview,
            });

            messages.push(Message::user(format!(
                "<tool_result name=\"{tool_name}\" step=\"{step_num}\">\n{result}\n</tool_result>\n\n\
                 Continue exploring or call done when you have enough information."
            )));
        }

        // Max steps reached — request a final answer.
        messages.push(Message::user(
            "You have reached the maximum number of steps. \
             Call done with your best answer based on what you found so far."
                .to_string(),
        ));
        let raw = self.call_llm(&messages, &schema).await?;
        let raw = crate::utils::strip_ansi(&raw);
        let answer = parse_tool_call(&raw)
            .and_then(|tc| {
                if let ToolCall::Done { answer } = tc {
                    Some(answer)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| raw.trim().to_string());

        Ok(ExploreResult {
            answer,
            sources: sorted(sources),
            steps,
        })
    }

    async fn call_llm(&self, messages: &[Message], schema: &serde_json::Value) -> Result<String> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::llm::Token>(256);
        let generate = self.llm.generate(messages, 512, tx, Some(schema.clone()));
        let collect = async {
            let mut buf = String::new();
            while let Some(t) = rx.recv().await {
                buf.push_str(&t);
            }
            buf
        };
        let (gen_result, raw) = tokio::join!(generate, collect);
        gen_result?;
        Ok(raw)
    }

    async fn execute(
        &self,
        tool: &ToolCall,
        sources: &mut HashSet<String>,
    ) -> Result<(String, String)> {
        match tool {
            ToolCall::Search { query, limit } => {
                // Async: embed the query.
                let query_text = format!("task: code retrieval | query: {query}");
                let vecs = self.embedder.embed(&[&query_text]).await?;
                let blob = vec_to_blob(vecs.first().context("no embedding returned")?);

                // Sync: open DB, query, drop before next await.
                let db = Database::open(&self.db_path)?;
                let results = db.search_similar(&blob, *limit)?;
                drop(db);

                for r in &results {
                    sources.insert(r.file_path.clone());
                }
                let result = if results.is_empty() {
                    "No results found.".to_string()
                } else {
                    results
                        .iter()
                        .map(|r| {
                            let name = r.name.as_deref().unwrap_or("<anonymous>");
                            let preview: String = r.content.chars().take(400).collect();
                            format!(
                                "chunk_id={} {}:{}-{} [{}: {name}]\n{preview}",
                                r.chunk_id, r.file_path, r.start_line, r.end_line, r.node_type,
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n---\n\n")
                };
                Ok((format!("query={query:?} limit={limit}"), result))
            }

            ToolCall::Graph { symbol } => {
                let db = Database::open(&self.db_path)?;
                let edges = if symbol.contains('/')
                    || symbol.contains('\\')
                    || symbol.ends_with(".rs")
                    || symbol.ends_with(".py")
                    || symbol.ends_with(".go")
                    || symbol.ends_with(".ts")
                    || symbol.ends_with(".js")
                {
                    db.edges_for_file(symbol)?
                } else {
                    db.edges_for_symbol(symbol)?
                };
                drop(db);

                let result = if edges.is_empty() {
                    format!("No graph edges found for '{symbol}'.")
                } else {
                    edges
                        .iter()
                        .map(|e| {
                            let src = e.source_name.as_deref().unwrap_or(&e.source_file);
                            format!(
                                "{src} --[{}]--> {} ({}:{})",
                                e.kind, e.target_name, e.source_file, e.line
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                Ok((format!("symbol={symbol:?}"), result))
            }

            ToolCall::ReadChunk { chunk_id } => {
                let db = Database::open(&self.db_path)?;
                let chunks = db.chunks_by_ids(&[*chunk_id])?;
                drop(db);

                let result = chunks
                    .first()
                    .map(|c| {
                        sources.insert(c.file_path.clone());
                        format!(
                            "{}:{}-{}\n```{}\n{}\n```",
                            c.file_path, c.start_line, c.end_line, c.language, c.content
                        )
                    })
                    .unwrap_or_else(|| format!("Chunk {chunk_id} not found."));
                Ok((format!("chunk_id={chunk_id}"), result))
            }

            ToolCall::ReadFile {
                path,
                start_line,
                end_line,
            } => {
                // The `path` is LLM-supplied and therefore untrusted: it can be
                // steered by adversarial content in indexed source files. Confine
                // every read to indexed files inside the canonical project root
                // before touching the filesystem.
                let resolved = match self.resolve_indexed_path(path) {
                    Ok(p) => p,
                    Err(denied) => {
                        // Recoverable: surface a tool result the model can react
                        // to instead of aborting the explore session. Echo back
                        // only the caller-supplied path, never a resolved path or
                        // any file contents.
                        let result = denied.tool_result(path);
                        return Ok((format!("path={path:?}"), result));
                    }
                };

                sources.insert(path.clone());
                let content = std::fs::read_to_string(&resolved)
                    .with_context(|| format!("reading file '{path}'"))?;
                let lines: Vec<&str> = content.lines().collect();
                let from = start_line.map(|n| n.saturating_sub(1)).unwrap_or(0);
                let to = end_line
                    .map(|n| n.min(lines.len()))
                    .unwrap_or_else(|| (from + 80).min(lines.len()));
                let slice = lines.get(from..to).unwrap_or(&[]);
                let result = format!("{}:{}-{}\n{}", path, from + 1, to, slice.join("\n"));
                Ok((format!("path={path:?} lines={}-{}", from + 1, to), result))
            }

            ToolCall::Done { .. } => unreachable!("Done handled in caller"),
        }
    }

    /// Validate an LLM-supplied `read_file` path against the index allow-list and
    /// the canonical project root, returning the on-disk path to read.
    ///
    /// Checks run in cheapest-first order so traversal probes are rejected before
    /// any filesystem syscall:
    /// 1. Reject absolute paths, Windows drive/UNC prefixes, and NUL bytes.
    /// 2. Lexically normalize the relative path; reject if it escapes root via `..`.
    /// 3. Index-membership check against the `files` table (primary control).
    /// 4. Canonicalize `root.join(rel)` and require it stays under the canonical
    ///    root (symlink backstop).
    fn resolve_indexed_path(&self, raw: &str) -> std::result::Result<PathBuf, Denied> {
        resolve_indexed_path(&self.db_path, self.project_root.as_deref(), raw)
    }
}

/// Validate an LLM-supplied `read_file` path against the index allow-list and the
/// canonical project root, returning the on-disk path to read. See
/// [`Explorer::resolve_indexed_path`] for the ordering rationale. Split out as a
/// free function so it can be unit-tested without constructing an `Explorer`
/// (which requires live inference backends).
fn resolve_indexed_path(
    db_path: &Path,
    project_root: Option<&Path>,
    raw: &str,
) -> std::result::Result<PathBuf, Denied> {
    // (1) Reject NUL bytes and absolute / drive / UNC inputs outright. The tool
    // contract is project-relative; an absolute path is never valid and must not
    // be silently rebased.
    if raw.contains('\0') {
        return Err(Denied::Invalid);
    }
    let raw_path = Path::new(raw);
    if raw_path.is_absolute() {
        return Err(Denied::Invalid);
    }
    // Windows drive-relative ("C:foo") and UNC/verbatim prefixes are not
    // `is_absolute()` on Unix, so check the first component explicitly.
    if let Some(Component::Prefix(_) | Component::RootDir) = raw_path.components().next() {
        return Err(Denied::Invalid);
    }
    // A backslash is a path separator on Windows; treat any input containing one
    // as a separator so traversal can't hide behind `..\\`. Normalize to `/` to
    // match the separator the indexer stores.
    let unified = raw.replace('\\', "/");

    // (2) Lexical normalization: resolve `.` and `..` textually. If the path
    // still escapes the root after normalization, reject before any I/O.
    let mut parts: Vec<&str> = Vec::new();
    for seg in unified.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    // `..` with nothing to pop escapes the root.
                    return Err(Denied::Invalid);
                }
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Err(Denied::Invalid);
    }
    let rel = parts.join("/");

    // (3) Index-membership check (primary allow-list). Only files the indexer
    // already vetted (ignore rules + sensitive-file overrides + secret scanner)
    // are readable.
    let db = Database::open(db_path).map_err(|_| Denied::NotIndexed)?;
    let is_indexed = matches!(db.file_id_for_path(&rel), Ok(Some(_)));
    drop(db);
    if !is_indexed {
        return Err(Denied::NotIndexed);
    }

    // (4) Canonical-root confinement (symlink backstop). Without a known root we
    // cannot prove confinement, so deny.
    let root = project_root.ok_or(Denied::NotIndexed)?;
    let canonical = std::fs::canonicalize(root.join(&rel)).map_err(|_| Denied::NotIndexed)?;
    if !canonical.starts_with(root) {
        return Err(Denied::NotIndexed);
    }

    Ok(canonical)
}

/// Reason a `read_file` target was rejected. Carries no resolved path or file
/// contents so the message echoed to the model can't leak either.
#[derive(Debug)]
enum Denied {
    /// Malformed or out-of-bounds input (absolute, drive/UNC, NUL, or `..`
    /// escaping the root).
    Invalid,
    /// Path is not an indexed file, or its resolved target escapes the root.
    NotIndexed,
}

impl Denied {
    /// Build the recoverable tool-result string, echoing back only the
    /// caller-supplied path.
    fn tool_result(&self, requested: &str) -> String {
        match self {
            Denied::Invalid => format!(
                "read_file denied: '{requested}' is not a valid project-relative path. \
                 Only files returned by search/graph results can be read. \
                 Use read_chunk for indexed content."
            ),
            Denied::NotIndexed => format!(
                "read_file denied: '{requested}' is outside the indexed project or not an \
                 indexed file. Only files returned by search/graph results can be read. \
                 Use read_chunk for indexed content."
            ),
        }
    }
}

fn sorted(set: HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = set.into_iter().collect();
    v.sort();
    v
}

const SYSTEM_PROMPT: &str = "\
You are an expert code analyst exploring a codebase to answer a developer's question.\n\
\n\
You have access to these tools. Always respond with exactly one JSON object — no prose, no code fences.\n\
\n\
{\"tool\": \"search\", \"args\": {\"query\": \"<semantic query>\", \"limit\": 5}}\n\
  Semantically search the code index. Returns chunks with chunk_id, file path, line range, content.\n\
\n\
{\"tool\": \"graph\", \"args\": {\"symbol\": \"<function name or file path>\"}}\n\
  Get call/import graph edges for a symbol or file.\n\
\n\
{\"tool\": \"read_chunk\", \"args\": {\"chunk_id\": 42}}\n\
  Read the full content of a specific chunk by id (from search results).\n\
\n\
{\"tool\": \"read_file\", \"args\": {\"path\": \"src/foo.rs\", \"start_line\": 10, \"end_line\": 50}}\n\
  Read lines from a file. The path must be a project-relative path to an indexed file\n\
  (one that appears in search/graph results). Absolute paths and files outside the index\n\
  are rejected. Omit start_line/end_line to read from line 1.\n\
\n\
{\"tool\": \"done\", \"args\": {\"answer\": \"<your final answer>\"}}\n\
  Call this when you have enough information to answer the question fully.\n\
\n\
Strategy: start with search, use read_chunk/graph to go deeper, call done when confident.";

#[cfg(test)]
mod read_file_boundary_tests {
    use super::{Denied, resolve_indexed_path};
    use crate::storage::Database;
    use std::fs;
    use std::sync::OnceLock;

    /// Register the sqlite-vec extension once per test process. `Database::open`
    /// creates a `vec0` virtual table, which requires the extension to be loaded
    /// before any connection is opened (done in `main` for the real binaries).
    fn register_sqlite_vec() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )));
            }
        });
    }

    /// Build a temp project: a real on-disk file `src/foo.rs` plus a DB whose
    /// `files` table lists it. Returns (tempdir, canonical_root, db_path).
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        register_sqlite_vec();
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/foo.rs"), "fn main() {}\n").unwrap();
        // An in-tree file the indexer deliberately skipped (e.g. a secret).
        fs::write(root.join(".env"), "SECRET=1\n").unwrap();

        let db_path = root.join("index.db");
        let db = Database::open(&db_path).unwrap();
        db.upsert_file("src/foo.rs", Some("rust"), "deadbeef")
            .unwrap();
        drop(db);
        (dir, root, db_path)
    }

    #[test]
    fn indexed_relative_path_resolves() {
        let (_dir, root, db_path) = fixture();
        let got = resolve_indexed_path(&db_path, Some(&root), "src/foo.rs").unwrap();
        assert_eq!(got, root.join("src/foo.rs"));
    }

    #[test]
    fn absolute_path_is_denied_as_invalid() {
        let (_dir, root, db_path) = fixture();
        assert!(matches!(
            resolve_indexed_path(&db_path, Some(&root), "/etc/passwd"),
            Err(Denied::Invalid)
        ));
    }

    #[test]
    fn traversal_escape_is_denied_lexically() {
        let (_dir, root, db_path) = fixture();
        assert!(matches!(
            resolve_indexed_path(&db_path, Some(&root), "../../etc/passwd"),
            Err(Denied::Invalid)
        ));
    }

    #[test]
    fn in_tree_but_unindexed_file_is_denied() {
        let (_dir, root, db_path) = fixture();
        // `.env` exists on disk and is under root, but is not in the index.
        assert!(matches!(
            resolve_indexed_path(&db_path, Some(&root), ".env"),
            Err(Denied::NotIndexed)
        ));
    }

    #[test]
    fn denial_message_echoes_only_requested_path() {
        let msg = Denied::NotIndexed.tool_result("../../etc/passwd");
        assert!(msg.contains("../../etc/passwd"));
        assert!(!msg.contains("/private"));
        assert!(!msg.contains("SECRET"));
    }

    // ---- Acceptance criterion: indexed symlink resolving outside root ----

    /// An indexed entry that is itself a symlink pointing outside the project
    /// root must be denied at the canonicalize/`starts_with` backstop (step 4),
    /// even though it passes the index-membership check (step 3). This is the
    /// case the implementer deliberately left uncovered.
    #[cfg(unix)]
    #[test]
    fn indexed_symlink_escaping_root_is_denied() {
        let (_dir, root, db_path) = fixture();

        // A real secret file living OUTSIDE the project root.
        let outside = tempfile::tempdir().unwrap();
        let secret = fs::canonicalize(outside.path()).unwrap().join("secret.txt");
        fs::write(&secret, "TOPSECRET\n").unwrap();

        // An in-tree symlink `src/escape.rs` -> the outside secret, and we add it
        // to the index so membership (step 3) passes. The canonical target lands
        // outside `root`, so step 4 must reject it.
        let link = root.join("src/escape.rs");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        let db = Database::open(&db_path).unwrap();
        db.upsert_file("src/escape.rs", Some("rust"), "cafef00d")
            .unwrap();
        drop(db);

        let err = resolve_indexed_path(&db_path, Some(&root), "src/escape.rs");
        assert!(
            matches!(err, Err(Denied::NotIndexed)),
            "indexed symlink escaping root must be denied, got {err:?}"
        );
    }

    /// Counterpart: a symlink that is indexed and resolves to a target *inside*
    /// the root is allowed. Guards against the backstop over-rejecting every
    /// symlink rather than only escaping ones.
    #[cfg(unix)]
    #[test]
    fn indexed_symlink_staying_in_root_is_allowed() {
        let (_dir, root, db_path) = fixture();
        // `src/alias.rs` -> `src/foo.rs` (both inside root).
        let link = root.join("src/alias.rs");
        std::os::unix::fs::symlink(root.join("src/foo.rs"), &link).unwrap();
        let db = Database::open(&db_path).unwrap();
        db.upsert_file("src/alias.rs", Some("rust"), "0000beef")
            .unwrap();
        drop(db);

        let got = resolve_indexed_path(&db_path, Some(&root), "src/alias.rs").unwrap();
        // Canonicalized through the symlink to the real in-tree target.
        assert_eq!(got, root.join("src/foo.rs"));
        assert!(got.starts_with(&root));
    }

    // ---- Acceptance criterion: indexed path read returns requested range ----

    /// A legitimately indexed relative path resolves to a readable on-disk file,
    /// and reading the requested line range returns exactly those lines. This
    /// mirrors the slice arithmetic in `Explorer::execute`'s `ReadFile` arm so a
    /// regression in the range math is caught.
    #[test]
    fn indexed_path_reads_requested_line_range() {
        let (_dir, root, db_path) = fixture();
        // Replace foo.rs with multi-line content and index it.
        let body = "line1\nline2\nline3\nline4\nline5\n";
        fs::write(root.join("src/foo.rs"), body).unwrap();

        let resolved = resolve_indexed_path(&db_path, Some(&root), "src/foo.rs").unwrap();
        let content = fs::read_to_string(&resolved).unwrap();
        let lines: Vec<&str> = content.lines().collect();

        // Request lines 2..=4 (1-based, inclusive end as in execute()).
        let (start_line, end_line) = (Some(2usize), Some(4usize));
        let from = start_line.map(|n| n.saturating_sub(1)).unwrap_or(0);
        let to = end_line
            .map(|n| n.min(lines.len()))
            .unwrap_or_else(|| (from + 80).min(lines.len()));
        let slice = lines.get(from..to).unwrap_or(&[]);
        assert_eq!(slice, &["line2", "line3", "line4"]);
    }

    // ---- Acceptance criterion: denial is recoverable, never leaks ----

    /// Denial returns a value (`Err(Denied)`) rather than panicking or reading
    /// the file, so the caller can surface a recoverable tool result instead of
    /// aborting the explore session. Asserts across every denial path that no
    /// resolved absolute path or file content leaks into the model-facing string.
    #[test]
    fn every_denial_is_recoverable_and_leak_free() {
        let (_dir, root, db_path) = fixture();

        let probes = ["/etc/passwd", "../../etc/passwd", ".env", "src/missing.rs"];
        for probe in probes {
            let outcome = resolve_indexed_path(&db_path, Some(&root), probe);
            let denied = match outcome {
                Err(d) => d,
                Ok(p) => panic!("expected denial for {probe:?}, resolved to {p:?}"),
            };
            let msg = denied.tool_result(probe);
            // Echoes back only the caller-supplied probe string.
            assert!(msg.contains(probe), "denial for {probe:?} should echo it");
            // Never leaks the canonical root, the on-disk secret, or its contents.
            assert!(
                !msg.contains(root.to_str().unwrap()),
                "denial for {probe:?} leaked resolved root: {msg}"
            );
            assert!(
                !msg.contains("SECRET") && !msg.contains("passwd:"),
                "denial for {probe:?} leaked file contents: {msg}"
            );
        }
    }

    /// Backslash-disguised traversal (`..\\..\\etc`) must be normalized to `/`
    /// and rejected lexically, not slip through as a single odd filename.
    #[test]
    fn backslash_traversal_is_denied() {
        let (_dir, root, db_path) = fixture();
        assert!(matches!(
            resolve_indexed_path(&db_path, Some(&root), "..\\..\\etc\\passwd"),
            Err(Denied::Invalid)
        ));
    }

    /// A path that normalizes to nothing (pure `.`/empty components) is rejected
    /// rather than treated as the root directory itself.
    #[test]
    fn empty_after_normalization_is_denied() {
        let (_dir, root, db_path) = fixture();
        assert!(matches!(
            resolve_indexed_path(&db_path, Some(&root), "./"),
            Err(Denied::Invalid)
        ));
    }

    /// A NUL byte in the path is rejected outright.
    #[test]
    fn nul_byte_path_is_denied() {
        let (_dir, root, db_path) = fixture();
        assert!(matches!(
            resolve_indexed_path(&db_path, Some(&root), "src/foo.rs\0.png"),
            Err(Denied::Invalid)
        ));
    }

    /// With no canonicalizable project root, every read is denied even when the
    /// path is indexed (can't prove confinement).
    #[test]
    fn missing_root_denies_indexed_path() {
        let (_dir, _root, db_path) = fixture();
        assert!(matches!(
            resolve_indexed_path(&db_path, None, "src/foo.rs"),
            Err(Denied::NotIndexed)
        ));
    }
}
