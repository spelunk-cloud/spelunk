//! Parsers for binary document formats: DOCX and spreadsheets (XLSX/XLS/ODS).
//!
//! These formats cannot be read with `std::fs::read_to_string` and bypass the
//! tree-sitter pipeline entirely.  Each parser extracts plain text and returns
//! chunks suitable for embedding.

#![cfg(feature = "rich-formats")]

use super::chunker::{Chunk, ChunkKind, sliding_window};

// ---------------------------------------------------------------------------
// Public dispatch
// ---------------------------------------------------------------------------

/// Parse a binary document and return embeddable chunks.
/// `language` must be `"docx"` or `"spreadsheet"`.
pub fn parse_doc(bytes: &[u8], file_path: &str, language: &str) -> Vec<Chunk> {
    match language {
        "docx" => parse_docx(bytes, file_path),
        "spreadsheet" => parse_spreadsheet(bytes, file_path),
        other => {
            tracing::warn!("docparser: unknown doc language '{other}' for {file_path}");
            vec![]
        }
    }
}

// ---------------------------------------------------------------------------
// DOCX
// ---------------------------------------------------------------------------

/// Extract text from a DOCX file and return as sliding-window chunks.
fn parse_docx(bytes: &[u8], file_path: &str) -> Vec<Chunk> {
    let doc = match docx_rs::read_docx(bytes) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("failed to parse DOCX {file_path}: {e}");
            return vec![];
        }
    };

    let mut lines: Vec<String> = Vec::new();
    collect_doc_text(&doc.document.children, &mut lines);

    if lines.is_empty() {
        return vec![];
    }

    sliding_window(&lines.join("\n"), file_path, "docx", None, None, None)
}

/// Recursively collect plain text lines from document children.
fn collect_doc_text(children: &[docx_rs::DocumentChild], out: &mut Vec<String>) {
    for child in children {
        match child {
            docx_rs::DocumentChild::Paragraph(p) => {
                let text = para_text(p);
                if !text.trim().is_empty() {
                    out.push(text);
                }
            }
            docx_rs::DocumentChild::Table(table) => {
                for docx_rs::TableChild::TableRow(row) in &table.rows {
                    let mut cells: Vec<String> = Vec::new();
                    for docx_rs::TableRowChild::TableCell(cell) in &row.cells {
                        let cell_text: Vec<String> = cell
                            .children
                            .iter()
                            .filter_map(|tc| {
                                if let docx_rs::TableCellContent::Paragraph(p) = tc {
                                    let t = para_text(p);
                                    if t.trim().is_empty() { None } else { Some(t) }
                                } else {
                                    None
                                }
                            })
                            .collect();
                        if !cell_text.is_empty() {
                            cells.push(cell_text.join(" "));
                        }
                    }
                    if !cells.is_empty() {
                        out.push(cells.join(" | "));
                    }
                }
            }
            _ => {}
        }
    }
}

/// Extract plain text from a single paragraph's runs.
fn para_text(p: &docx_rs::Paragraph) -> String {
    p.children
        .iter()
        .filter_map(|child| {
            if let docx_rs::ParagraphChild::Run(run) = child {
                Some(run_text(run))
            } else {
                None
            }
        })
        .collect()
}

fn run_text(run: &docx_rs::Run) -> String {
    run.children
        .iter()
        .filter_map(|rc| {
            if let docx_rs::RunChild::Text(t) = rc {
                Some(t.text.as_str())
            } else {
                None
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Spreadsheets (XLSX / XLS / ODS via calamine)
// ---------------------------------------------------------------------------

/// Extract spreadsheet data and return one chunk per sheet (or sliding-window
/// chunks for sheets with more than 120 rows).
fn parse_spreadsheet(bytes: &[u8], file_path: &str) -> Vec<Chunk> {
    use calamine::{Reader, open_workbook_auto_from_rs};
    use std::io::Cursor;

    let cursor = Cursor::new(bytes.to_vec());
    let mut wb = match open_workbook_auto_from_rs(cursor) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("failed to open spreadsheet {file_path}: {e}");
            return vec![];
        }
    };

    let sheet_names = wb.sheet_names().to_vec();
    let mut chunks = Vec::new();

    for sheet_name in &sheet_names {
        let range: calamine::Range<calamine::Data> = match wb.worksheet_range(sheet_name) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("failed to read sheet '{sheet_name}' in {file_path}: {e}");
                continue;
            }
        };

        let lines: Vec<String> = range
            .rows()
            .map(|row| {
                row.iter()
                    .map(|cell: &calamine::Data| cell.to_string())
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .filter(|line: &String| !line.trim().is_empty())
            .collect();

        if lines.is_empty() {
            continue;
        }

        let content = lines.join("\n");
        let total = lines.len();

        if total <= 120 {
            chunks.push(Chunk {
                file_path: file_path.to_owned(),
                language: "spreadsheet".to_owned(),
                kind: ChunkKind::Section,
                name: Some(sheet_name.clone()),
                start_line: 1,
                end_line: total,
                content,
                docstring: None,
                parent_scope: None,
                summary: None,
            });
        } else {
            for mut chunk in sliding_window(&content, file_path, "spreadsheet", None, None, None) {
                chunk.name = Some(format!(
                    "{} (rows {}–{})",
                    sheet_name, chunk.start_line, chunk.end_line
                ));
                chunks.push(chunk);
            }
        }
    }

    chunks
}
