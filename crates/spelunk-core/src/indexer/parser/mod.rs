mod text;
mod ts_walker;

use super::chunker::{Chunk, sliding_window};
use anyhow::Result;
use std::ops::ControlFlow;

/// All languages recognised by the indexer (tree-sitter, text, and document formats).
pub const SUPPORTED_LANGUAGES: &[&str] = &[
    "rust",
    "python",
    "javascript",
    "jsx",
    "typescript",
    "tsx",
    "go",
    "java",
    "c",
    "cpp",
    "json",
    "html",
    "css",
    "hcl",
    "php",
    "ruby",
    "csharp",
    "kotlin",
    "swift",
    "sql",
    "proto",
    // text formats (sliding-window / heading-based, no tree-sitter)
    "markdown",
    "text",
    // structured text (custom parsers, no tree-sitter)
    "notebook",
    // binary document formats (docparser, no tree-sitter)
    #[cfg(feature = "rich-formats")]
    "docx",
    #[cfg(feature = "rich-formats")]
    "spreadsheet",
    // PDF (rich-formats feature)
    #[cfg(feature = "rich-formats")]
    "pdf",
];

/// Detect language from file extension.
pub fn detect_language(path: &std::path::Path) -> Option<&'static str> {
    match path.extension()?.to_str()? {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "js" | "mjs" | "cjs" => Some("javascript"),
        "jsx" => Some("jsx"),
        "ts" | "mts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "go" => Some("go"),
        "java" => Some("java"),
        "c" | "h" => Some("c"),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Some("cpp"),
        "json" => Some("json"),
        "html" | "htm" => Some("html"),
        "css" => Some("css"),
        "tf" | "hcl" => Some("hcl"),
        "php" | "phtml" => Some("php"),
        "rb" | "rake" | "gemspec" => Some("ruby"),
        "cs" => Some("csharp"),
        "kt" | "kts" => Some("kotlin"),
        "swift" => Some("swift"),
        "sql" | "sequel" => Some("sql"),
        "proto" => Some("proto"),
        #[cfg(feature = "rich-formats")]
        "pdf" => Some("pdf"),
        _ => None,
    }
}

pub(crate) fn ts_language_pub(name: &str) -> Result<tree_sitter::Language> {
    ts_walker::ts_language(name)
}

/// Detect text-format languages (markdown, plain text, notebooks) from file path.
/// These are handled without tree-sitter.
pub fn detect_text_language(path: &std::path::Path) -> Option<&'static str> {
    // Check extension first
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        return match ext.to_lowercase().as_str() {
            "md" | "mdx" | "markdown" => Some("markdown"),
            // R Markdown / Quarto documents are markdown with fenced code blocks.
            "rmd" | "qmd" => Some("markdown"),
            "txt" | "rst" | "adoc" | "asciidoc" => Some("text"),
            // Jupyter notebooks: custom JSON-based parser.
            "ipynb" => Some("notebook"),
            _ => None,
        };
    }
    // Extensionless files: README, CHANGELOG, etc.
    let name = path.file_name()?.to_str()?.to_uppercase();
    match name.as_str() {
        "README" | "CHANGELOG" | "CHANGES" | "CONTRIBUTING" | "NOTICE" | "AUTHORS" | "HISTORY" => {
            Some("text")
        }
        _ => None,
    }
}

/// Detect binary document formats (DOCX, spreadsheets) from file extension.
/// These are handled by `docparser` — they cannot be read with `read_to_string`.
/// Only returns `Some` when the `rich-formats` feature is enabled.
pub fn detect_doc_language(path: &std::path::Path) -> Option<&'static str> {
    #[cfg(feature = "rich-formats")]
    match path.extension()?.to_str()?.to_lowercase().as_str() {
        "docx" => return Some("docx"),
        "xlsx" | "xls" | "ods" => return Some("spreadsheet"),
        _ => {}
    }
    let _ = path;
    None
}

/// Return true if the file appears to be binary (contains null bytes in the
/// first 512 bytes). Used to skip compiled or binary assets.
pub fn is_binary_file(path: &std::path::Path) -> bool {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open(path) {
        let mut buf = [0u8; 512];
        if let Ok(n) = f.read(&mut buf) {
            return buf[..n].contains(&0u8);
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct SourceParser;

impl SourceParser {
    /// Parse `source` and return semantic chunks.
    /// Falls back to sliding-window if parsing fails or yields nothing.
    pub fn parse(source: &str, file_path: &str, language: &str) -> Result<Vec<Chunk>> {
        // Text formats bypass tree-sitter entirely.
        if language == "markdown" {
            return Ok(text::parse_markdown(source, file_path));
        }
        if language == "text" {
            return Ok(sliding_window(
                source, file_path, language, None, None, None,
            ));
        }
        if language == "notebook" {
            return Ok(text::parse_notebook(source, file_path));
        }

        // Guard against adversarial inputs that cause tree-sitter's GLR parser to
        // allocate exponential memory (e.g. deeply-nested pointer declarators).
        // The 5-second time budget only bounds CPU time; memory can still spike
        // before the first progress callback fires.
        const MAX_PARSE_BYTES: usize = 512 * 1024;
        if source.len() > MAX_PARSE_BYTES {
            tracing::warn!(
                "{file_path}: input too large ({} bytes > {MAX_PARSE_BYTES}), using sliding window",
                source.len()
            );
            return Ok(sliding_window(
                source, file_path, language, None, None, None,
            ));
        }

        let ts_lang = ts_walker::ts_language(language)?;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang)?;

        // Prevent pathological inputs (adversarial or deeply ambiguous) from
        // consuming unbounded memory/time during GLR parsing.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut on_progress = |_: &tree_sitter::ParseState| -> ControlFlow<()> {
            if std::time::Instant::now() >= deadline {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let bytes = source.as_bytes();
        let len = bytes.len();
        let mut opts = tree_sitter::ParseOptions::new().progress_callback(&mut on_progress);
        let tree = match parser.parse_with_options(
            &mut |i, _| if i < len { &bytes[i..] } else { &[] },
            None,
            Some(opts.reborrow()),
        ) {
            Some(t) => t,
            None => {
                tracing::warn!(
                    "{file_path}: tree-sitter parse exceeded time budget, using sliding window"
                );
                return Ok(sliding_window(
                    source, file_path, language, None, None, None,
                ));
            }
        };

        let specs = ts_walker::node_specs(language);
        let ctx = ts_walker::WalkCtx {
            src: bytes,
            file_path,
            language,
            specs: &specs,
        };
        let mut chunks = Vec::new();

        ts_walker::walk_node(tree.root_node(), &ctx, None, &mut chunks, 0);

        if chunks.is_empty() {
            tracing::debug!("{file_path}: no semantic nodes found, using sliding window");
            return Ok(sliding_window(
                source, file_path, language, None, None, None,
            ));
        }

        Ok(chunks)
    }
}
