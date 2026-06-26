use super::mentions::extract_mention_tokens;

use anyhow::Result;
use ignore::WalkBuilder;
use indicatif::{MultiProgress, ProgressBar};

use super::super::ui::{is_tty, progress_style, short_path};
use super::IndexArgs;
#[cfg(feature = "rich-formats")]
use crate::indexer::docparser::parse_doc;
use crate::{
    indexer::{
        graph::EdgeExtractor,
        parser::{
            SourceParser, detect_doc_language, detect_language, detect_text_language,
            is_binary_file,
        },
    },
    search::tokens::estimate_tokens,
    storage::Database,
};

pub(super) struct ParseResult {
    /// (chunk_id, embedding_text) pairs awaiting embedding.
    pub chunk_ids_and_texts: Vec<(i64, String)>,
    pub indexed: u64,
    pub removed: u64,
}

/// Mutable accumulators shared across per-file processor functions.
/// Bundled into one struct so processor signatures stay under 7 arguments.
struct ParseAcc {
    out: Vec<(i64, String)>,
    indexed: u64,
    skipped: u64,
}

/// Collect source files from `root`, parse them, store chunks + graph edges,
/// then remove stale index records for files that no longer exist.
pub(super) fn run_parse_phase(
    root: &std::path::Path,
    db: &Database,
    args: &IndexArgs,
    mp: &MultiProgress,
) -> Result<ParseResult> {
    let files = collect_files(root)?;

    if files.is_empty() {
        println!("No supported source files found in {}", root.display());
        return Ok(ParseResult {
            chunk_ids_and_texts: vec![],
            indexed: 0,
            removed: 0,
        });
    }

    let parse_bar = if is_tty() && !crate::utils::is_agent_mode() {
        let bar = mp.add(ProgressBar::new(files.len() as u64));
        bar.set_style(progress_style("Parsing  "));
        bar
    } else {
        ProgressBar::hidden()
    };

    let mut acc = ParseAcc {
        out: Vec::new(),
        indexed: 0,
        skipped: 0,
    };

    for entry in &files {
        let path = entry.path();
        // Store paths relative to the project root so the index is portable.
        // Normalize separators to `/` so the on-disk index is identical across
        // OSes and matches forward-slash CLI/query paths (Windows `to_string_lossy`
        // would otherwise emit `src\lib.rs`).
        let rel = path.strip_prefix(root).unwrap_or(path);
        let path_str = spelunk_core::utils::normalize_index_path(&rel.to_string_lossy());
        parse_bar.set_message(short_path(&path_str));

        // ── Binary document formats (DOCX, XLSX, PDF, …) ─────────────────────
        #[cfg(feature = "rich-formats")]
        if let Some(doc_lang) = detect_doc_language(path)
            && process_doc_file(path, &path_str, doc_lang, db, args, &mut acc)?
        {
            parse_bar.inc(1);
            continue;
        }

        // ── PDF documents (feature-gated) ─────────────────────────────────────
        #[cfg(feature = "rich-formats")]
        if detect_language(path) == Some("pdf")
            && process_pdf_file(path, &path_str, db, args, &mut acc)?
        {
            parse_bar.inc(1);
            continue;
        }

        // ── Text / code formats ───────────────────────────────────────────────
        process_text_file(path, &path_str, db, args, &mut acc)?;
        parse_bar.inc(1);
    }

    parse_bar.finish_with_message(format!(
        "{} files parsed ({} skipped, {} new/changed)",
        acc.indexed, acc.skipped, acc.indexed
    ));

    let removed = cleanup_stale(&files, root, db)?;
    let ParseAcc {
        out: chunk_ids_and_texts,
        indexed,
        ..
    } = acc;

    Ok(ParseResult {
        chunk_ids_and_texts,
        indexed,
        removed,
    })
}

// ── File collection ───────────────────────────────────────────────────────────

fn collect_files(root: &std::path::Path) -> Result<Vec<ignore::DirEntry>> {
    let sensitive_patterns = [
        "!.env",
        "!.env.*",
        "!*.pem",
        "!*.key",
        "!*.p12",
        "!*.pfx",
        "!*.p8",
        "!*.cer",
        "!*.crt",
        "!*.der",
        "!id_rsa",
        "!id_ecdsa",
        "!id_ed25519",
        "!id_dsa",
        "!*.keystore",
        "!*.jks",
        "!.netrc",
        "!.npmrc",
    ];
    let mut walk = WalkBuilder::new(root);
    walk.standard_filters(true);
    walk.add_custom_ignore_filename(".spelunkignore");
    let mut ob = ignore::overrides::OverrideBuilder::new(root);
    for pat in &sensitive_patterns {
        ob.add(pat).ok();
    }
    if let Ok(ov) = ob.build() {
        walk.overrides(ov);
    }

    Ok(walk
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_file()).unwrap_or(false))
        .filter(|e| {
            let p = e.path();
            detect_language(p).is_some()
                || detect_text_language(p).is_some()
                || detect_doc_language(p).is_some()
        })
        .collect())
}

// ── Per-file processors ───────────────────────────────────────────────────────

#[cfg(feature = "rich-formats")]
fn process_doc_file(
    path: &std::path::Path,
    path_str: &str,
    doc_lang: &'static str,
    db: &Database,
    args: &IndexArgs,
    acc: &mut ParseAcc,
) -> Result<bool> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("read error for {path_str}: {e}");
            return Ok(true);
        }
    };
    let hash = format!("{}", blake3::hash(&bytes));
    if !args.force
        && let Some(existing) = db.file_hash(path_str)?
        && existing == hash
    {
        acc.skipped += 1;
        return Ok(true);
    }
    let chunks = parse_doc(&bytes, path_str, doc_lang);
    let file_id = db.upsert_file(path_str, Some(doc_lang), &hash)?;
    db.delete_embeddings_for_file(file_id)?;
    db.delete_chunks_for_file(file_id)?;
    store_chunks(&chunks, path_str, file_id, db, acc)?;
    acc.indexed += 1;
    Ok(true)
}

#[cfg(feature = "rich-formats")]
fn process_pdf_file(
    path: &std::path::Path,
    path_str: &str,
    db: &Database,
    args: &IndexArgs,
    acc: &mut ParseAcc,
) -> Result<bool> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("read error for {path_str}: {e}");
            return Ok(true);
        }
    };
    let hash = format!("{}", blake3::hash(&bytes));
    if !args.force
        && let Some(existing) = db.file_hash(path_str)?
        && existing == hash
    {
        return Ok(true);
    }
    match crate::indexer::pdf::extract_pdf_text(path) {
        Ok(pages) => {
            let file_id = db.upsert_file(path_str, Some("pdf"), &hash)?;
            db.delete_embeddings_for_file(file_id)?;
            db.delete_chunks_for_file(file_id)?;
            let chunks = pages_to_chunks(pages, path_str);
            store_chunks(&chunks, path_str, file_id, db, acc)?;
            acc.indexed += 1;
        }
        Err(e) => {
            tracing::warn!("skipping PDF {}: {e}", path.display());
        }
    }
    Ok(true)
}

#[cfg(feature = "rich-formats")]
fn pages_to_chunks(pages: Vec<(u32, String)>, path_str: &str) -> Vec<crate::indexer::Chunk> {
    pages
        .into_iter()
        .map(|(page_num, text)| crate::indexer::Chunk {
            file_path: path_str.to_string(),
            language: "pdf".to_string(),
            kind: crate::indexer::ChunkKind::Section,
            name: Some(format!("page {page_num}")),
            start_line: page_num as usize,
            end_line: page_num as usize,
            content: text,
            docstring: None,
            parent_scope: None,
            summary: None,
        })
        .collect()
}

fn process_text_file(
    path: &std::path::Path,
    path_str: &str,
    db: &Database,
    args: &IndexArgs,
    acc: &mut ParseAcc,
) -> Result<()> {
    let language = detect_language(path)
        .or_else(|| detect_text_language(path))
        .unwrap(); // safe: files were filtered to only include detectable files

    // Skip binary files (e.g. compiled output with wrong extension)
    if matches!(language, "text" | "markdown") && is_binary_file(path) {
        return Ok(());
    }
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("skipping {path_str}: {e}");
            return Ok(());
        }
    };
    let hash = format!("{}", blake3::hash(source.as_bytes()));

    if !args.force
        && let Some(existing) = db.file_hash(path_str)?
        && existing == hash
    {
        acc.skipped += 1;
        return Ok(());
    }

    let chunks = match SourceParser::parse(&source, path_str, language) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("parse error for {path_str}: {e}");
            return Ok(());
        }
    };

    let file_id = db.upsert_file(path_str, Some(language), &hash)?;
    db.delete_embeddings_for_file(file_id)?;
    db.delete_chunks_for_file(file_id)?;

    // Extract and store graph edges for this file (structural: calls/imports/extends).
    match EdgeExtractor::extract(&source, path_str, language) {
        Ok(edges) => {
            if let Err(e) = db.replace_edges(path_str, &edges) {
                tracing::warn!("graph edge storage failed for {path_str}: {e}");
            }
        }
        Err(e) => tracing::warn!("graph extraction failed for {path_str}: {e}"),
    }

    // Store mention edges (broader than calls — used by LinearRAG C matrix).
    // replace_edges already cleared the file's edges, so we just append here.
    let mention_owned: Vec<(Option<String>, String)> = chunks
        .iter()
        .filter(|c| c.name.is_some())
        .flat_map(|c| {
            let name = c.name.clone().unwrap();
            extract_mention_tokens(&c.content, language)
                .into_iter()
                .map(move |sym| (Some(name.clone()), sym))
        })
        .collect();
    let mention_refs: Vec<(Option<&str>, &str)> = mention_owned
        .iter()
        .map(|(n, s)| (n.as_deref(), s.as_str()))
        .collect();
    if !mention_refs.is_empty()
        && let Err(e) = db.append_mention_edges(path_str, &mention_refs)
    {
        tracing::warn!("mention edge storage failed for {path_str}: {e}");
    }

    store_chunks(&chunks, path_str, file_id, db, acc)?;
    acc.indexed += 1;
    Ok(())
}

/// Insert a slice of parsed chunks into the DB and record their embedding texts.
fn store_chunks(
    chunks: &[crate::indexer::Chunk],
    path_str: &str,
    file_id: i64,
    db: &Database,
    acc: &mut ParseAcc,
) -> Result<()> {
    for chunk in chunks {
        if crate::indexer::secrets::contains_secret(&chunk.content) {
            tracing::warn!(
                "skipping chunk '{}' in {path_str} (possible secret detected)",
                chunk.name.as_deref().unwrap_or("<anonymous>"),
            );
            continue;
        }
        let metadata =
            serde_json::json!({ "docstring": chunk.docstring, "parent_scope": chunk.parent_scope });
        let tc = estimate_tokens(&chunk.content);
        let chunk_id = db.insert_chunk(
            file_id,
            &chunk.kind.to_string(),
            chunk.name.as_deref(),
            chunk.start_line,
            chunk.end_line,
            &chunk.content,
            Some(&metadata.to_string()),
            tc,
        )?;
        acc.out.push((chunk_id, chunk.embedding_text()));
    }
    Ok(())
}

// ── Stale file cleanup ────────────────────────────────────────────────────────

fn cleanup_stale(files: &[ignore::DirEntry], root: &std::path::Path, db: &Database) -> Result<u64> {
    // Paths in the DB are root-relative, so visited uses the same relative form.
    let visited: std::collections::HashSet<String> = files
        .iter()
        .map(|e| {
            let p = e.path();
            // Match the normalized form stored during indexing (forward slashes).
            spelunk_core::utils::normalize_index_path(
                &p.strip_prefix(root).unwrap_or(p).to_string_lossy(),
            )
        })
        .collect();
    // Pass "" so file_paths_under returns all files in this DB (paths are relative).
    let all_indexed = db.file_paths_under("")?;
    let mut removed = 0u64;
    for (id, path) in all_indexed {
        if !visited.contains(&path) {
            db.delete_file(id, &path)?;
            removed += 1;
        }
    }
    Ok(removed)
}
