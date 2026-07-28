use super::super::chunker::{Chunk, ChunkKind, chunk_token_cap, sliding_window};
use crate::search::tokens::estimate_tokens;

/// Parse a `.ipynb` notebook into per-cell chunks.
///
/// Code cells become `Verbatim` chunks tagged with the kernel language.
/// Markdown/raw cells become `Section` chunks.  Falls back to sliding-window
/// if the JSON is malformed.
pub(super) fn parse_notebook(source: &str, file_path: &str) -> Vec<Chunk> {
    #[derive(serde::Deserialize)]
    struct Notebook {
        cells: Vec<Cell>,
        #[serde(default)]
        metadata: NotebookMeta,
    }
    #[derive(serde::Deserialize, Default)]
    struct NotebookMeta {
        #[serde(default)]
        kernelspec: Kernelspec,
    }
    #[derive(serde::Deserialize, Default)]
    struct Kernelspec {
        #[serde(default)]
        language: String,
    }
    #[derive(serde::Deserialize)]
    struct Cell {
        cell_type: String,
        source: CellSource,
    }
    /// The `source` field is either a JSON string or an array of strings.
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum CellSource {
        Lines(Vec<String>),
        Blob(String),
    }
    impl CellSource {
        fn text(&self) -> String {
            match self {
                Self::Lines(v) => v.join(""),
                Self::Blob(s) => s.clone(),
            }
        }
    }

    let nb: Notebook = match serde_json::from_str(source) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("failed to parse notebook {file_path}: {e}");
            return sliding_window(source, file_path, "notebook", None, None, None);
        }
    };

    let kernel_lang = if nb.metadata.kernelspec.language.is_empty() {
        "python".to_owned()
    } else {
        nb.metadata.kernelspec.language.clone()
    };

    let mut chunks = Vec::new();
    let mut line = 1usize;

    for (idx, cell) in nb.cells.iter().enumerate() {
        let text = cell.source.text();
        if text.trim().is_empty() {
            continue;
        }
        let line_count = text.lines().count().max(1);
        let (kind, lang) = match cell.cell_type.as_str() {
            "markdown" | "raw" => (ChunkKind::Section, "markdown"),
            _ => (ChunkKind::Verbatim, kernel_lang.as_str()),
        };
        chunks.push(Chunk {
            file_path: file_path.to_owned(),
            language: lang.to_owned(),
            kind,
            name: Some(format!("cell {}", idx + 1)),
            start_line: line,
            end_line: line + line_count - 1,
            content: text,
            docstring: None,
            parent_scope: None,
            summary: None,
        });
        line += line_count;
    }

    if chunks.is_empty() {
        return sliding_window(source, file_path, "notebook", None, None, None);
    }
    chunks
}

/// Split a Markdown document into per-section chunks.
/// Each ATX heading (`#`, `##`, …) starts a new chunk that includes the
/// heading line and all content until the next same-or-higher-level heading.
/// If the file has no headings the whole document is split by sliding window.
pub(super) fn parse_markdown(source: &str, file_path: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = source.lines().collect();
    let mut chunks: Vec<Chunk> = Vec::new();
    // (start_line_idx, heading_text)
    let mut section: Option<(usize, String)> = None;
    let mut preamble: Vec<usize> = Vec::new(); // line indices before first heading

    let flush = |start: usize,
                 title: Option<String>,
                 end: usize,
                 lines: &[&str],
                 chunks: &mut Vec<Chunk>| {
        let content = lines[start..end].join("\n");
        if content.trim().is_empty() {
            return;
        }
        if estimate_tokens(&content) > chunk_token_cap() {
            // Section start is 0-based line index `start`; sub-chunk lines are
            // 1-based within the section, so `start` is the correct offset.
            // Thread the section heading through as each sub-chunk's name so an
            // oversized section keeps its title in the embedding text.
            for mut sub in sliding_window(
                &content,
                file_path,
                "markdown",
                title.as_deref(),
                None,
                None,
            ) {
                sub.start_line += start;
                sub.end_line += start;
                chunks.push(sub);
            }
            return;
        }
        chunks.push(Chunk {
            file_path: file_path.to_owned(),
            language: "markdown".to_owned(),
            kind: ChunkKind::Section,
            name: title,
            start_line: start + 1,
            end_line: end,
            content,
            docstring: None,
            parent_scope: None,
            summary: None,
        });
    };

    for (i, line) in lines.iter().enumerate() {
        if let Some(heading_text) = atx_heading(line) {
            if let Some((start, title)) = section.take() {
                flush(start, Some(title), i, &lines, &mut chunks);
            } else if !preamble.is_empty() {
                // Flush preamble (content before the first heading)
                let start = *preamble.first().unwrap();
                flush(start, None, i, &lines, &mut chunks);
                preamble.clear();
            }
            section = Some((i, heading_text));
        } else if section.is_none() {
            preamble.push(i);
        }
    }

    // Flush the last section / remaining preamble
    if let Some((start, title)) = section {
        flush(start, Some(title), lines.len(), &lines, &mut chunks);
    } else if !preamble.is_empty() {
        let start = *preamble.first().unwrap();
        flush(start, None, lines.len(), &lines, &mut chunks);
    }

    if chunks.is_empty() {
        return sliding_window(source, file_path, "markdown", None, None, None);
    }
    chunks
}

/// Extract the text of an ATX heading line (`# Foo` → `"Foo"`).
/// Returns None for non-heading lines or fenced-code-block lines.
pub(super) fn atx_heading(line: &str) -> Option<String> {
    let stripped = line.trim_start_matches('#');
    let hashes = line.len() - stripped.len();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    // Must be followed by a space (or end of line for empty heading)
    if !stripped.is_empty() && !stripped.starts_with(' ') {
        return None;
    }
    Some(stripped.trim().to_owned())
}
