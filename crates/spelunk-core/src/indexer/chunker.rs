use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// Process-wide document embedding-text template, set once at the start of
/// `index` from `Config::document_prompt_template`. Placeholders: `{title}`,
/// `{body}`. When unset, [`Chunk::embedding_text`] uses the built-in
/// EmbeddingGemma format. Follows the same set-once-config pattern as the
/// secrets-scanner regexes.
static DOC_TEMPLATE: OnceLock<Option<String>> = OnceLock::new();

/// Set the document embedding-text template for this process. Idempotent: only
/// the first call takes effect (one `spelunk index` invocation per process).
pub fn set_document_template(template: Option<String>) {
    let _ = DOC_TEMPLATE.set(template);
}

/// Process-wide cap on embedding-text length (chars), set once at the start of
/// `index` from `Config::max_embed_chars`. Bounds input for external embedding
/// backends that reject over-length text (e.g. llama.cpp).
static MAX_EMBED_CHARS: OnceLock<Option<usize>> = OnceLock::new();

/// Set the embedding-text character cap for this process. Idempotent.
pub fn set_max_embed_chars(max_chars: Option<usize>) {
    let _ = MAX_EMBED_CHARS.set(max_chars);
}

/// Truncate `text` to at most `max` chars on a UTF-8 boundary.
fn cap_chars(text: String) -> String {
    if let Some(Some(max)) = MAX_EMBED_CHARS.get()
        && text.chars().count() > *max
    {
        return text.chars().take(*max).collect();
    }
    text
}

/// The semantic kind of an extracted code chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    Function,
    Method,
    Struct,
    Class,
    Enum,
    Interface,
    Impl,
    Trait,
    Module,
    Constant,
    TypeAlias,
    /// A CSS rule set (selector + declarations block)
    Rule,
    /// A Markdown heading + its body content
    Section,
    /// Fallback: plain line range (unsupported language or oversized node)
    Verbatim,
}

impl std::fmt::Display for ChunkKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "unknown".to_string());
        write!(f, "{s}")
    }
}

/// A single unit of source code to be embedded and stored.
#[derive(Debug, Clone)]
pub struct Chunk {
    #[allow(dead_code)]
    pub file_path: String,
    #[allow(dead_code)]
    pub language: String,
    pub kind: ChunkKind,
    /// Symbol name, if the node has one (e.g. function or struct name).
    pub name: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
    /// Optional docstring / leading comment.
    pub docstring: Option<String>,
    /// Enclosing scope (e.g. `impl MyStruct` for a method).
    pub parent_scope: Option<String>,
    /// LLM-generated one-sentence summary (set after indexing, not during parsing).
    pub summary: Option<String>,
}

impl Chunk {
    /// The text that gets passed to the embedding model.
    /// Uses EmbeddingGemma's recommended document retrieval format:
    /// `title: {title | "none"} | text: {content}`
    ///
    /// When a summary is present, it is prepended:
    /// `title: {title} | summary: {summary} | text: {content}`
    pub fn embedding_text(&self) -> String {
        let title = self.name.as_deref().unwrap_or("none");
        let body = match &self.docstring {
            Some(doc) => format!("{doc}\n{}", self.content),
            None => self.content.clone(),
        };
        // Per-model override (e.g. nomic: `search_document: {body}`).
        let out = if let Some(Some(tmpl)) = DOC_TEMPLATE.get() {
            tmpl.replace("{title}", title).replace("{body}", &body)
        } else {
            match &self.summary {
                Some(summary) => format!("title: {title} | summary: {summary} | text: {body}"),
                None => format!("title: {title} | text: {body}"),
            }
        };
        cap_chars(out)
    }
}

/// Split `source` into chunks using a sliding window (fallback for
/// languages without a tree-sitter grammar or for files that failed parsing).
pub fn sliding_window(
    source: &str,
    file_path: &str,
    language: &str,
    window_lines: usize,
    overlap_lines: usize,
) -> Vec<Chunk> {
    let lines: Vec<&str> = source.lines().collect();
    let step = window_lines.saturating_sub(overlap_lines).max(1);
    let mut chunks = Vec::new();
    let mut start = 0;

    while start < lines.len() {
        let end = (start + window_lines).min(lines.len());
        chunks.push(Chunk {
            file_path: file_path.to_string(),
            language: language.to_string(),
            kind: ChunkKind::Verbatim,
            name: None,
            start_line: start + 1,
            end_line: end,
            content: lines[start..end].join("\n"),
            docstring: None,
            parent_scope: None,
            summary: None,
        });
        if end == lines.len() {
            break;
        }
        start += step;
    }

    chunks
}
