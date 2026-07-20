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

/// Upper bound on the size of any single file read into memory during
/// indexing, checked via `metadata().len()` *before* the file is opened for
/// reading. Applied uniformly to every format (text, markdown, tree-sitter
/// source, PDF, DOCX, XLSX, …) — a single gate, not one per branch — so a
/// multi-GB file (or a compression-bomb office/PDF doc) can't be read fully
/// into memory and OOM-kill the indexer. This is distinct from (and
/// complementary to) `MAX_PARSE_BYTES` in `spelunk_core::indexer::parser`,
/// which only bounds how much of an *already-read* buffer tree-sitter will
/// attempt to GLR-parse before falling back to a sliding window.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Return `true` (and log a warning) if `path` is over `MAX_FILE_BYTES`,
/// checked via a `metadata()` call — no file content is read either way.
/// Callers must skip the file without reading it when this returns `true`.
/// The file's filesystem modification time as unix seconds, or `0` when it is
/// unavailable (platform without mtime support, or a stat/timestamp error).
/// Persisted via `upsert_file` so the embed queue can order by file recency;
/// `0` sorts last under the queue's `mtime DESC` order — deterministic, never
/// an error. Only called for files being (re)parsed, so the stat is on
/// new/changed files, not every walked file.
fn stat_mtime(path: &std::path::Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn is_file_too_large(path: &std::path::Path, path_str: &str) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_FILE_BYTES => {
            tracing::warn!(
                "skipping {path_str}: file too large ({} bytes > {MAX_FILE_BYTES} byte cap)",
                meta.len()
            );
            true
        }
        _ => false,
    }
}

pub(super) struct ParseResult {
    /// (chunk_id, embedding_text, token_count) tuples awaiting embedding.
    /// `token_count` mirrors the stored `chunks.token_count` (never 0 here).
    pub chunk_ids_and_texts: Vec<(i64, String, usize)>,
    pub indexed: u64,
    pub removed: u64,
    /// Count of files skipped by the built-in index filter (generated/vendored/
    /// machine-data). Distinct from the hash/oversized `skipped` counter.
    /// Surfaced to the user via the post-parse notice; retained for assertions.
    #[allow(dead_code)]
    pub filtered: u64,
}

/// The single-line notice printed after the parse bar when the index filter
/// dropped at least one file. Pure so it can be unit-tested verbatim.
fn filtered_notice(filtered: u64) -> String {
    format!(
        "Filtered out {filtered} generated/vendored/data file(s) \
         (built-in index filter; override in [index] of .spelunk/config.toml)"
    )
}

/// Mutable accumulators shared across per-file processor functions.
/// Bundled into one struct so processor signatures stay under 7 arguments.
struct ParseAcc {
    out: Vec<(i64, String, usize)>,
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
    cfg: &crate::config::Config,
) -> Result<ParseResult> {
    let filter = spelunk_core::indexer::filter::IndexFilter::build(
        &cfg.index.exclude,
        cfg.index.use_default_excludes,
        cfg.index.detect_generated,
    )?;
    let (files, filtered) = collect_files(root, &filter)?;

    if files.is_empty() {
        if filtered > 0 {
            println!("{}", filtered_notice(filtered));
        }
        println!("No supported source files found in {}", root.display());
        return Ok(ParseResult {
            chunk_ids_and_texts: vec![],
            indexed: 0,
            removed: 0,
            filtered,
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

    if filtered > 0 {
        println!("{}", filtered_notice(filtered));
    }

    let removed = cleanup_stale(&files, root, db)?;
    let ParseAcc {
        out: mut chunk_ids_and_texts,
        indexed,
        ..
    } = acc;

    // Backfill: pick up any chunks that exist in the index but have no
    // embedding row yet (e.g. a prior `init`/`index` parsed & chunked while
    // the embedder was still loading, so the embed phase was skipped). These
    // belong to unchanged files that the hash-based skip above never re-emits,
    // so without this union a plain `spelunk index` would report "nothing to
    // do" and leave them permanently unembedded.
    //
    // Freshly-parsed chunks from this run also lack an embedding row, so they
    // appear here too; dedupe against the ids we already queued to avoid
    // embedding them twice.
    let already: std::collections::HashSet<i64> =
        chunk_ids_and_texts.iter().map(|(id, ..)| *id).collect();
    for (chunk_id, name, metadata, summary, content, token_count) in
        db.chunks_missing_embeddings()?
    {
        if already.contains(&chunk_id) {
            continue;
        }
        let tokens = effective_token_count(token_count, &content);
        let text =
            reconstruct_embedding_text(name.as_deref(), metadata.as_deref(), summary, content);
        chunk_ids_and_texts.push((chunk_id, text, tokens));
    }

    Ok(ParseResult {
        chunk_ids_and_texts,
        indexed,
        removed,
        filtered,
    })
}

/// Build the `(chunk_id, embedding_text)` list for every chunk in the index
/// that has no embedding row yet, reconstructing each chunk's document text
/// from its stored columns. This is the same union `run_parse_phase` applies as
/// a backfill; exposed separately so a detached embed-only
/// subprocess can rebuild the embed queue straight from the DB without
/// re-parsing.
pub(super) fn missing_embedding_texts(db: &Database) -> Result<Vec<(i64, String, usize)>> {
    let mut out = Vec::new();
    for (chunk_id, name, metadata, summary, content, token_count) in
        db.chunks_missing_embeddings()?
    {
        let tokens = effective_token_count(token_count, &content);
        let text =
            reconstruct_embedding_text(name.as_deref(), metadata.as_deref(), summary, content);
        out.push((chunk_id, text, tokens));
    }
    Ok(out)
}

/// Token weight for a queue entry: the stored `chunks.token_count`, estimated
/// on the fly for a pre-backfill row (stored 0), floored at 1 so token-weighted
/// arithmetic never divides by zero.
fn effective_token_count(stored: usize, content: &str) -> usize {
    let tc = if stored == 0 {
        estimate_tokens(content)
    } else {
        stored
    };
    tc.max(1)
}

/// Rebuild the exact document text that `Chunk::embedding_text()` produces,
/// from the columns stored for a chunk. The `docstring` lives inside the
/// `metadata` JSON (`{ "docstring": ..., "parent_scope": ... }`), mirroring how
/// `store_chunks` persists it. Keep this in lockstep with
/// `spelunk_core::indexer::Chunk::embedding_text`.
pub(super) fn reconstruct_embedding_text(
    name: Option<&str>,
    metadata: Option<&str>,
    summary: Option<String>,
    content: String,
) -> String {
    let title = name.unwrap_or("none");
    let docstring = metadata
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .and_then(|v| {
            v.get("docstring")
                .and_then(|d| d.as_str().map(str::to_string))
        });
    let body = match docstring {
        Some(doc) => format!("{doc}\n{content}"),
        None => content,
    };
    match summary {
        Some(summary) => format!("title: {title} | summary: {summary} | text: {body}"),
        None => format!("title: {title} | text: {body}"),
    }
}

// ── File collection ───────────────────────────────────────────────────────────

/// Walk `root` collecting the files the indexer should ingest, returning them
/// alongside a count of files dropped by the built-in index filter.
///
/// Two independent exclusion layers apply:
///   1. The **sensitive** `OverrideBuilder` (`.env*`, `*.pem`, private keys):
///      unconditional and NOT user-overridable. Sensitive files are dropped by
///      the walk before `filter` ever sees them, so nothing in `[index]` can
///      re-include them.
///   2. The **index `filter`** (generated/vendored/machine-data): user-tunable
///      via `[index]` in config. Excluded directories are pruned from the walk
///      (perf); individual excluded files (and generated-marker survivors) are
///      counted.
fn collect_files(
    root: &std::path::Path,
    filter: &spelunk_core::indexer::filter::IndexFilter,
) -> Result<(Vec<ignore::DirEntry>, u64)> {
    use spelunk_core::indexer::filter::Decision;

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
    ob.case_insensitive(true).ok();
    for pat in &sensitive_patterns {
        ob.add(pat).ok();
    }
    if let Ok(ov) = ob.build() {
        walk.overrides(ov);
    }

    // Prune index-filter-excluded directories during the walk so we never
    // descend into node_modules/, vendor/, dist/, etc. Files are left to the
    // collect loop below so they can be classified and counted individually.
    let root_owned = root.to_path_buf();
    let dir_filter = filter.clone();
    walk.filter_entry(move |entry| {
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if !is_dir {
            return true;
        }
        match entry.path().strip_prefix(&root_owned) {
            // Never prune the root itself (empty relative path).
            Ok(rel) if !rel.as_os_str().is_empty() => !dir_filter.prune_dir(rel),
            _ => true,
        }
    });

    let mut files = Vec::new();
    let mut filtered = 0u64;
    for entry in walk.build().filter_map(|e| e.ok()) {
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let p = entry.path();
        // Only weigh files the indexer would otherwise ingest, so the filtered
        // count reflects real embed/parse work avoided.
        if !(detect_language(p).is_some()
            || detect_text_language(p).is_some()
            || detect_doc_language(p).is_some())
        {
            continue;
        }
        let rel = p.strip_prefix(root).unwrap_or(p);
        match filter.decide(rel, false) {
            Decision::Exclude(mi) => {
                tracing::debug!(
                    "index filter: excluding {} (matched {:?}, {})",
                    rel.display(),
                    mi.pattern,
                    if mi.from_default { "default" } else { "user" },
                );
                filtered += 1;
                continue;
            }
            // A user `!` re-include keeps the file AND exempts it from
            // generated-marker detection.
            Decision::ForceInclude(_) => {
                files.push(entry);
                continue;
            }
            Decision::Keep => {}
        }
        // Generated-marker detection on glob survivors (self-declaration only).
        if filter.detect_generated()
            && let Some(marker) = spelunk_core::indexer::filter::generated_marker(p)
        {
            tracing::debug!(
                "index filter: excluding {} (generated marker: {})",
                rel.display(),
                marker,
            );
            filtered += 1;
            continue;
        }
        files.push(entry);
    }
    Ok((files, filtered))
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
    if is_file_too_large(path, path_str) {
        acc.skipped += 1;
        return Ok(true);
    }
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
    let file_id = db.upsert_file(path_str, Some(doc_lang), &hash, stat_mtime(path))?;
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
    if is_file_too_large(path, path_str) {
        acc.skipped += 1;
        return Ok(true);
    }
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
            let file_id = db.upsert_file(path_str, Some("pdf"), &hash, stat_mtime(path))?;
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
    if is_file_too_large(path, path_str) {
        acc.skipped += 1;
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

    let file_id = db.upsert_file(path_str, Some(language), &hash, stat_mtime(path))?;
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
        // Scan the full text that will be persisted/embedded (docstring + content;
        // `chunk.summary` is always `None` at this point, so `embedding_text()`
        // here is exactly docstring+content). Dropping the chunk here — before the
        // metadata JSON is built — ensures a secret in the docstring never lands
        // in stored metadata either. See secrets.rs module doc: this is
        // best-effort defense-in-depth, not a security boundary.
        if crate::indexer::secrets::contains_secret(&chunk.embedding_text()) {
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
        acc.out.push((chunk_id, chunk.embedding_text(), tc.max(1)));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::OnceLock;

    /// Register the sqlite-vec extension exactly once per test process.
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

    fn open_db() -> Database {
        register_sqlite_vec();
        Database::open(std::path::Path::new(":memory:")).expect("open in-memory Database")
    }

    fn default_args(path: std::path::PathBuf) -> IndexArgs {
        IndexArgs {
            path,
            db: None,
            batch_size: 64,
            force: false,
            recount: false,
            no_summaries: false,
            summary_batch_size: 10,
            background_phases: false,
            embed_phases: false,
            detach: false,
            detach_embed: false,
            config_path: None,
        }
    }

    /// A sparse file whose reported length is over `MAX_FILE_BYTES`, created
    /// via `set_len` so no actual bytes are written/allocated on disk — the
    /// test itself must not read megabytes of data to prove the cap works.
    fn make_oversized_sparse_file() -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().expect("create temp file");
        file.as_file()
            .set_len(MAX_FILE_BYTES + 1)
            .expect("set_len on temp file");
        file
    }

    // ── reconstruct_embedding_text mirrors Chunk::embedding_text ─────────────

    /// The DB-side reconstruction used to backfill unembedded chunks must
    /// produce byte-for-byte the same document text as `Chunk::embedding_text()`
    /// did at store time, so a backfilled embedding is identical to one written
    /// during a normal parse. Covers: name present/absent and
    /// docstring present/absent (summary is always None at store time).
    #[test]
    fn reconstruct_embedding_text_matches_chunk_embedding_text() {
        use crate::indexer::{Chunk, ChunkKind};

        let cases = [
            (Some("do_thing"), Some("Does the thing.")),
            (Some("do_thing"), None),
            (None, Some("Anonymous doc.")),
            (None, None),
        ];
        for (name, docstring) in cases {
            let chunk = Chunk {
                file_path: "src/lib.rs".to_string(),
                language: "rust".to_string(),
                kind: ChunkKind::Function,
                name: name.map(str::to_string),
                start_line: 1,
                end_line: 3,
                content: "fn do_thing() {}".to_string(),
                docstring: docstring.map(str::to_string),
                parent_scope: None,
                summary: None,
            };
            // Metadata JSON exactly as store_chunks persists it.
            let metadata = serde_json::json!({
                "docstring": chunk.docstring,
                "parent_scope": chunk.parent_scope,
            })
            .to_string();

            let reconstructed =
                reconstruct_embedding_text(name, Some(&metadata), None, chunk.content.clone());
            assert_eq!(
                reconstructed,
                chunk.embedding_text(),
                "reconstruction diverged for name={name:?} docstring={docstring:?}"
            );
        }
    }

    /// The summary branch of `reconstruct_embedding_text` must also match
    /// `Chunk::embedding_text()`. Phase-4 LLM summaries can be written to a
    /// chunk (`chunks.summary`) before a later re-index backfills its embedding,
    /// so the backfill path reconstructs with a non-null `summary` and must
    /// produce the exact `title: {name} | summary: {summary} | text: {body}`
    /// document. Covers summary × docstring present/absent.
    #[test]
    fn reconstruct_embedding_text_matches_chunk_embedding_text_with_summary() {
        use crate::indexer::{Chunk, ChunkKind};

        let cases = [
            (
                Some("do_thing"),
                Some("Does the thing."),
                "Summarised: does the thing.",
            ),
            (Some("do_thing"), None, "Summarised: no docstring."),
            (None, Some("Anonymous doc."), "Summarised: anonymous."),
            (None, None, "Summarised: bare."),
        ];
        for (name, docstring, summary) in cases {
            let chunk = Chunk {
                file_path: "src/lib.rs".to_string(),
                language: "rust".to_string(),
                kind: ChunkKind::Function,
                name: name.map(str::to_string),
                start_line: 1,
                end_line: 3,
                content: "fn do_thing() {}".to_string(),
                docstring: docstring.map(str::to_string),
                parent_scope: None,
                summary: Some(summary.to_string()),
            };
            // Metadata JSON exactly as store_chunks persists it (docstring lives
            // in metadata; the summary is a separate stored column).
            let metadata = serde_json::json!({
                "docstring": chunk.docstring,
                "parent_scope": chunk.parent_scope,
            })
            .to_string();

            let reconstructed = reconstruct_embedding_text(
                name,
                Some(&metadata),
                Some(summary.to_string()),
                chunk.content.clone(),
            );
            assert_eq!(
                reconstructed,
                chunk.embedding_text(),
                "reconstruction diverged for name={name:?} docstring={docstring:?} summary={summary:?}"
            );
        }
    }

    // ── End-to-end backfill: parse-only run leaves chunks unembedded, a
    //    second parse run backfills them without reparsing ────

    /// `run_parse_phase` stores chunks but never writes embeddings — that is the
    /// embed phase's job. So a single parse run models the real bug: an
    /// `init`/`index` that chunked while the embedder was still loading, leaving
    /// the `embeddings` table empty. This test drives the full parse path over a
    /// real fixture repo twice (no `--force`) and asserts:
    ///   (a) after run 1, every stored chunk is unembedded (embeddings empty);
    ///   (b) run 2 reparses nothing (`indexed == 0`, all files hash-skipped);
    ///   (c) yet run 2 still returns a NON-EMPTY `chunk_ids_and_texts` — the
    ///       missing-embedding chunks are unioned in for the embed phase;
    ///   (d) the backfilled ids are exactly the chunk ids stored in run 1
    ///       (same ids ⇒ no delete+reinsert ⇒ no unchanged file was reparsed).
    #[test]
    fn reindex_backfills_unembedded_chunks_without_reparsing() {
        use indicatif::MultiProgress;

        let db = open_db();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "/// Doc for foo.\npub fn foo() -> i32 { 1 }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.rs"),
            "pub struct Bar { x: i32 }\npub fn bar() {}\n",
        )
        .unwrap();

        let args = default_args(dir.path().to_path_buf());
        let mp = MultiProgress::new();

        // ── Run 1: parse + store chunks. No embeddings are ever written here. ──
        let cfg = crate::config::Config::default();
        let first = run_parse_phase(dir.path(), &db, &args, &mp, &cfg).expect("first parse phase");
        assert!(
            first.indexed >= 2,
            "both fixture files must be indexed on the first run"
        );
        assert!(
            !first.chunk_ids_and_texts.is_empty(),
            "the first run must queue freshly-parsed chunks for embedding"
        );

        // Every chunk stored in run 1 is currently unembedded (embeddings empty):
        // the set of missing-embedding chunk ids must equal the run-1 queued ids.
        let mut queued_run1: Vec<i64> = first
            .chunk_ids_and_texts
            .iter()
            .map(|(id, ..)| *id)
            .collect();
        queued_run1.sort();
        let mut missing_after_run1: Vec<i64> = db
            .chunks_missing_embeddings()
            .expect("missing after run 1")
            .into_iter()
            .map(|(id, ..)| id)
            .collect();
        missing_after_run1.sort();
        assert_eq!(
            missing_after_run1, queued_run1,
            "after a parse-only run the embeddings table is empty — every stored chunk is missing its embedding"
        );

        // ── Run 2: no file changed, so nothing is reparsed. The backfill union
        //    must still surface the unembedded chunks for the embed phase. ──────
        let second =
            run_parse_phase(dir.path(), &db, &args, &mp, &cfg).expect("second parse phase");
        assert_eq!(
            second.indexed, 0,
            "no file changed — the hash-based skip must reparse nothing on the second run"
        );
        assert!(
            !second.chunk_ids_and_texts.is_empty(),
            "the fix must union the missing-embedding chunks into the embed batch even though indexed == 0"
        );

        // (d) The backfilled ids are exactly the run-1 chunk ids: identical ids
        // prove the chunks were NOT deleted and reinserted (a reparse would mint
        // fresh rowids), i.e. no unchanged file was reparsed — only its missing
        // embeddings were queued.
        let mut backfilled: Vec<i64> = second
            .chunk_ids_and_texts
            .iter()
            .map(|(id, ..)| *id)
            .collect();
        backfilled.sort();
        assert_eq!(
            backfilled, queued_run1,
            "backfill must queue the same chunk ids stored in run 1 (no reparse / re-chunk)"
        );

        // The reconstructed embedding texts must also be byte-identical to what
        // the first (parse-time) run produced for those same chunks.
        let mut texts_run1: Vec<(i64, String, usize)> = first.chunk_ids_and_texts.clone();
        texts_run1.sort_by_key(|(id, ..)| *id);
        let mut texts_run2: Vec<(i64, String, usize)> = second.chunk_ids_and_texts.clone();
        texts_run2.sort_by_key(|(id, ..)| *id);
        assert_eq!(
            texts_run2, texts_run1,
            "backfilled embedding text must match the parse-time embedding text byte-for-byte"
        );
    }

    // ── missing_embedding_texts: detached embed-only queue reconstruction ──────

    /// The detached `--_embed-phases` subprocess rebuilds its embed queue purely
    /// from the DB via `missing_embedding_texts()` — it never re-parses. This test
    /// seeds chunks, embeds a subset directly, and proves the function returns
    /// exactly the un-embedded chunks (skipping embedded ones), in id order, with
    /// each text reconstructed byte-for-byte to `Chunk::embedding_text()`. If this
    /// diverged, the detached run would either re-embed already-done chunks or
    /// embed the wrong text.
    #[test]
    fn missing_embedding_texts_returns_only_unembedded_chunks_from_db() {
        use crate::indexer::{Chunk, ChunkKind};

        let db = open_db();
        let file_id = db
            .upsert_file("src/lib.rs", Some("rust"), "hash0", 0)
            .unwrap();

        // Store three chunks the way `store_chunks` does (docstring lives in the
        // metadata JSON), so the reconstructed text is comparable to the
        // parse-time `embedding_text()`.
        let mut ids = Vec::new();
        let chunks = [
            ("alpha", Some("Doc for alpha."), "fn alpha() {}"),
            ("beta", None, "fn beta() {}"),
            ("gamma", Some("Doc for gamma."), "fn gamma() {}"),
        ];
        for (name, docstring, content) in chunks {
            let chunk = Chunk {
                file_path: "src/lib.rs".to_string(),
                language: "rust".to_string(),
                kind: ChunkKind::Function,
                name: Some(name.to_string()),
                start_line: 1,
                end_line: 2,
                content: content.to_string(),
                docstring: docstring.map(str::to_string),
                parent_scope: None,
                summary: None,
            };
            let metadata = serde_json::json!({
                "docstring": chunk.docstring,
                "parent_scope": chunk.parent_scope,
            })
            .to_string();
            let id = db
                .insert_chunk(
                    file_id,
                    "function",
                    Some(name),
                    1,
                    2,
                    content,
                    Some(&metadata),
                    1,
                )
                .unwrap();
            ids.push((id, chunk));
        }

        // Embed only the middle chunk (`beta`), leaving `alpha` and `gamma`
        // missing their embedding rows.
        let (beta_id, _) = &ids[1];
        db.insert_embedding(
            *beta_id,
            &vec![0.1f32; spelunk_core::embeddings::EMBEDDING_DIM],
        )
        .unwrap();

        let missing = missing_embedding_texts(&db).expect("missing_embedding_texts");

        // Exactly the two un-embedded chunks, in ascending id order, and NOT the
        // embedded one.
        let got_ids: Vec<i64> = missing.iter().map(|(id, ..)| *id).collect();
        assert_eq!(
            got_ids,
            vec![ids[0].0, ids[2].0],
            "only the un-embedded chunks (alpha, gamma) must be queued, in id order"
        );
        assert!(
            !got_ids.contains(beta_id),
            "the already-embedded chunk must not be re-queued"
        );

        // Each queued text is reconstructed byte-for-byte to the parse-time
        // `embedding_text()` for that chunk.
        for (queued_id, queued_text, _) in &missing {
            let (_, chunk) = ids.iter().find(|(id, _)| id == queued_id).unwrap();
            assert_eq!(
                queued_text,
                &chunk.embedding_text(),
                "queued text must match Chunk::embedding_text for chunk {queued_id}"
            );
        }
    }

    /// When every chunk already has an embedding, the detached embed queue must
    /// be empty — the subprocess then does no embed work (guards against the
    /// missing-embedding query over-matching).
    #[test]
    fn missing_embedding_texts_is_empty_when_all_embedded() {
        let db = open_db();
        let file_id = db
            .upsert_file("src/lib.rs", Some("rust"), "hash0", 0)
            .unwrap();
        let id = db
            .insert_chunk(file_id, "function", Some("f"), 1, 2, "fn f() {}", None, 1)
            .unwrap();
        db.insert_embedding(id, &vec![0.1f32; spelunk_core::embeddings::EMBEDDING_DIM])
            .unwrap();

        assert!(
            missing_embedding_texts(&db).unwrap().is_empty(),
            "a fully-embedded index yields an empty detached embed queue"
        );
    }

    // ── mtime capture + recency ordering (onboarding embed queue) ─────────────

    /// Set a file's filesystem modification time to `unix_secs` past the epoch,
    /// without touching its content (so its content hash is unchanged).
    fn set_file_mtime(path: &std::path::Path, unix_secs: u64) {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(unix_secs);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    /// `stat_mtime` on a path that cannot be `stat()`'d (doesn't exist —
    /// stands in for any metadata-read failure, e.g. a permission error or a
    /// virtual/generated path with no real inode) must fall back to `0`
    /// rather than panicking or erroring the whole parse phase. `0` is the
    /// same sentinel a pre-migration row carries and sorts last,
    /// deterministically, under the queue's `mtime DESC` order.
    #[test]
    fn stat_mtime_nonexistent_path_falls_back_to_zero() {
        let missing = std::path::Path::new("/nonexistent/definitely-not-a-real-path.rs");
        assert_eq!(
            stat_mtime(missing),
            0,
            "an unstattable path must fall back to 0, not panic"
        );
    }

    /// A file with a modification time before the Unix epoch (a corrupted
    /// filesystem timestamp, or a container/VM with a badly-skewed clock) makes
    /// `SystemTime::duration_since(UNIX_EPOCH)` return `Err`. `stat_mtime` must
    /// still fall back to `0` rather than panicking on the `i64` conversion.
    #[test]
    fn stat_mtime_pre_epoch_time_falls_back_to_zero_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ancient.rs");
        std::fs::write(&f, "pub fn ancient() {}\n").unwrap();
        let pre_epoch = std::time::UNIX_EPOCH - std::time::Duration::from_secs(100);
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(pre_epoch)
            .unwrap();

        assert_eq!(
            stat_mtime(&f),
            0,
            "a pre-epoch mtime must fall back to 0, not panic on the i64 cast"
        );
    }

    /// A file whose mtime is far in the future (clock skew, or a deliberately
    /// forward-touched file) must be read back verbatim as a large positive
    /// `i64`, without overflow or panic on the `u64 -> i64` cast.
    #[test]
    fn stat_mtime_far_future_time_returns_positive_no_panic() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("future.rs");
        std::fs::write(&f, "pub fn future() {}\n").unwrap();
        // Year ~2107 — comfortably future without approaching u64/i64 bounds.
        set_file_mtime(&f, 4_300_000_000);

        assert_eq!(
            stat_mtime(&f),
            4_300_000_000,
            "a far-future mtime must round-trip verbatim, not overflow or panic"
        );
    }

    /// A parsed file stores its filesystem mtime in `files.mtime`, and the
    /// DB-driven embed queue (the exact path the detached `--_embed-phases`
    /// worker rebuilds from) orders the resulting chunks by that recency on a
    /// cold index (all graph_rank still 0): the more-recently-modified file's
    /// chunks come first. Drives the real production parse path end-to-end.
    #[test]
    fn parse_captures_mtime_and_queue_orders_by_recency() {
        use indicatif::MultiProgress;

        let db = open_db();
        let dir = tempfile::tempdir().unwrap();
        let older = dir.path().join("older.rs");
        let newer = dir.path().join("newer.rs");
        std::fs::write(&older, "pub fn older_fn() {}\n").unwrap();
        std::fs::write(&newer, "pub fn newer_fn() {}\n").unwrap();
        set_file_mtime(&older, 1_000);
        set_file_mtime(&newer, 2_000);

        let args = default_args(dir.path().to_path_buf());
        let mp = MultiProgress::new();
        let cfg = crate::config::Config::default();
        run_parse_phase(dir.path(), &db, &args, &mp, &cfg).expect("parse phase");

        // (a) mtime captured verbatim per file.
        assert_eq!(db.file_mtime("older.rs").unwrap(), Some(1_000));
        assert_eq!(db.file_mtime("newer.rs").unwrap(), Some(2_000));

        // (b) the DB-driven queue emits every newer-file chunk before any
        // older-file chunk — recency first.
        let queue_ids: Vec<i64> = missing_embedding_texts(&db)
            .expect("queue")
            .iter()
            .map(|(id, ..)| *id)
            .collect();
        let newer_ids: Vec<i64> = db
            .chunks_for_file("newer.rs")
            .unwrap()
            .iter()
            .map(|c| c.chunk_id)
            .collect();
        let older_ids: Vec<i64> = db
            .chunks_for_file("older.rs")
            .unwrap()
            .iter()
            .map(|c| c.chunk_id)
            .collect();
        assert!(
            !newer_ids.is_empty() && !older_ids.is_empty(),
            "both files chunked"
        );
        let last_newer = queue_ids
            .iter()
            .rposition(|id| newer_ids.contains(id))
            .expect("newer chunks queued");
        let first_older = queue_ids
            .iter()
            .position(|id| older_ids.contains(id))
            .expect("older chunks queued");
        assert!(
            last_newer < first_older,
            "all newer-file chunks must precede older-file chunks: {queue_ids:?}"
        );
    }

    /// A hash-unchanged file is skipped on re-parse, so its stored mtime is NOT
    /// refreshed even when the file's filesystem mtime changed (a plain touch).
    /// Retention falls out of the skip path never calling `upsert_file`.
    #[test]
    fn unchanged_file_retains_stored_mtime_on_reindex() {
        use indicatif::MultiProgress;

        let db = open_db();
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("keep.rs");
        std::fs::write(&f, "pub fn keep() {}\n").unwrap();
        set_file_mtime(&f, 1_000);

        let args = default_args(dir.path().to_path_buf());
        let mp = MultiProgress::new();
        let cfg = crate::config::Config::default();

        let r1 = run_parse_phase(dir.path(), &db, &args, &mp, &cfg).expect("run 1");
        assert!(r1.indexed >= 1);
        assert_eq!(
            db.file_mtime("keep.rs").unwrap(),
            Some(1_000),
            "run 1 stores the file's filesystem mtime"
        );

        // Touch the file's mtime WITHOUT changing its content: the hash is
        // identical, so the re-parse hash-skips it.
        set_file_mtime(&f, 5_000);
        let r2 = run_parse_phase(dir.path(), &db, &args, &mp, &cfg).expect("run 2");
        assert_eq!(
            r2.indexed, 0,
            "unchanged content → the file is hash-skipped, not reparsed"
        );
        assert_eq!(
            db.file_mtime("keep.rs").unwrap(),
            Some(1_000),
            "a skipped file's stored mtime is retained, not overwritten with the new FS mtime"
        );
    }

    // ── is_file_too_large ────────────────────────────────────────────────────

    #[test]
    fn is_file_too_large_true_over_cap() {
        let file = make_oversized_sparse_file();
        assert!(is_file_too_large(file.path(), "oversized.txt"));
    }

    #[test]
    fn is_file_too_large_false_under_cap() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"small file content").unwrap();
        assert!(!is_file_too_large(file.path(), "small.txt"));
    }

    // ── process_text_file: oversized files are skipped before any read ──────

    /// An oversized text file must be skipped without ever being read into
    /// memory. We assert this indirectly but strongly: `process_text_file`
    /// only calls `db.upsert_file` (recording a content hash) *after*
    /// `std::fs::read_to_string` succeeds. If the size gate didn't
    /// short-circuit before the read, the file would still get indexed (a
    /// sparse file reads back as all-zero bytes, which is valid UTF-8) and
    /// `db.file_hash` would return `Some(..)`. Asserting it stays `None`
    /// proves the read (and everything downstream of it) never happened —
    /// not just that some later step errored out.
    #[test]
    fn process_text_file_oversized_is_skipped_before_read() {
        let db = open_db();
        let dir = tempfile::tempdir().unwrap();
        // `.rs` (tree-sitter language) rather than `.txt`, so the unrelated
        // is_binary_file() sniff (which only applies to "text"/"markdown"
        // languages) doesn't short-circuit before we reach the size gate —
        // a sparse file reads back as all-zero bytes, which is_binary_file
        // would otherwise flag as binary regardless of the size cap.
        let path = dir.path().join("huge.rs");
        {
            let f = std::fs::File::create(&path).unwrap();
            f.set_len(MAX_FILE_BYTES + 1).unwrap();
        }
        let args = default_args(dir.path().to_path_buf());
        let mut acc = ParseAcc {
            out: Vec::new(),
            indexed: 0,
            skipped: 0,
        };

        let path_str = "huge.rs";
        let result = process_text_file(&path, path_str, &db, &args, &mut acc);

        assert!(result.is_ok(), "oversized file must be skipped, not error");
        assert_eq!(acc.indexed, 0, "oversized file must not be indexed");
        assert_eq!(acc.skipped, 1, "oversized file must be counted as skipped");
        assert!(
            db.file_hash(path_str).unwrap().is_none(),
            "oversized file must never reach upsert_file — proves the read never happened"
        );
    }

    /// A file just at the cap boundary is allowed through to the normal read
    /// path (sanity check that the gate uses `>`, not `>=`, matching the doc
    /// comment "over the size cap").
    #[test]
    fn process_text_file_at_cap_boundary_is_not_skipped_by_size_gate() {
        let file = tempfile::NamedTempFile::new().unwrap();
        // Exactly at the cap: must NOT be flagged as too large.
        file.as_file().set_len(MAX_FILE_BYTES).unwrap();
        assert!(!is_file_too_large(file.path(), "boundary.bin"));
    }

    // ── Index filter: collect_files exclusion + counting ─────────────────────

    use spelunk_core::indexer::filter::IndexFilter;

    fn collected_names(files: &[ignore::DirEntry]) -> Vec<String> {
        files
            .iter()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect()
    }

    /// Junk (lockfiles, minified, protobuf codegen) is excluded with the correct
    /// count; vendored directories are pruned (their contents never counted);
    /// real source survives.
    #[test]
    fn collect_files_excludes_junk_with_correct_count() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(dir.path().join("package-lock.json"), "{}\n").unwrap();
        std::fs::write(dir.path().join("app.min.js"), "var x=1;\n").unwrap();
        std::fs::write(dir.path().join("user.pb.go"), "package x\n").unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("node_modules/index.js"), "var y=2;\n").unwrap();

        let filter = IndexFilter::build(&[], true, true).unwrap();
        let (files, filtered) = collect_files(dir.path(), &filter).unwrap();
        let names = collected_names(&files);

        assert!(names.contains(&"lib.rs".to_string()));
        assert!(names.contains(&"package.json".to_string()));
        assert!(!names.contains(&"package-lock.json".to_string()));
        assert!(!names.contains(&"app.min.js".to_string()));
        assert!(!names.contains(&"user.pb.go".to_string()));
        // node_modules is pruned, so its file never appears.
        assert!(!names.contains(&"index.js".to_string()));
        // The three file-level excludes are counted; the pruned dir's contents
        // are not (that is the walk-time performance win).
        assert_eq!(filtered, 3);
    }

    /// Survivors listed in the spec pass the filter untouched (incl. a `.ts`
    /// under `i18n/`, since that default only excludes `*.json`).
    #[test]
    fn collect_files_keeps_spec_survivors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/i18n")).unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(dir.path().join("tsconfig.json"), "{}\n").unwrap();
        std::fs::write(dir.path().join("README.md"), "# hi\n").unwrap();
        std::fs::write(dir.path().join("tests/foo_test.rs"), "fn t() {}\n").unwrap();
        std::fs::write(dir.path().join("src/i18n/index.ts"), "export const x=1;\n").unwrap();

        let filter = IndexFilter::build(&[], true, true).unwrap();
        let (files, filtered) = collect_files(dir.path(), &filter).unwrap();
        let names = collected_names(&files);

        for expected in [
            "lib.rs",
            "package.json",
            "tsconfig.json",
            "README.md",
            "foo_test.rs",
            "index.ts",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "{expected} must survive"
            );
        }
        assert_eq!(filtered, 0);
    }

    /// A generated-marker file is filtered when `detect_generated` is on, and
    /// survives when it is off.
    #[test]
    fn collect_files_generated_marker_toggle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("normal.rs"), "fn a() {}\n").unwrap();
        std::fs::write(
            dir.path().join("gen.rs"),
            "// Code generated by tool. DO NOT EDIT.\nfn a() {}\n",
        )
        .unwrap();

        let on = IndexFilter::build(&[], true, true).unwrap();
        let (files, filtered) = collect_files(dir.path(), &on).unwrap();
        let names = collected_names(&files);
        assert!(names.contains(&"normal.rs".to_string()));
        assert!(!names.contains(&"gen.rs".to_string()));
        assert_eq!(filtered, 1);

        let off = IndexFilter::build(&[], true, false).unwrap();
        let (files_off, filtered_off) = collect_files(dir.path(), &off).unwrap();
        assert!(collected_names(&files_off).contains(&"gen.rs".to_string()));
        assert_eq!(filtered_off, 0);
    }

    /// HARD INVARIANT: the sensitive-file layer (`.env`) is independent of the
    /// index filter and NOT user-overridable. `[index].exclude = ["!.env"]` must
    /// have no effect: the sensitive `OverrideBuilder` drops `.env` before the
    /// index filter can re-include it.
    #[test]
    fn sensitive_env_not_reincludable_via_index_exclude() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join(".env"), "SECRET=1\n").unwrap();

        let filter = IndexFilter::build(&["!.env".to_string()], true, true).unwrap();
        let (files, _filtered) = collect_files(dir.path(), &filter).unwrap();
        let names = collected_names(&files);

        assert!(
            names.contains(&"keep.rs".to_string()),
            "normal file collected"
        );
        assert!(
            !names.contains(&".env".to_string()),
            "[index].exclude=[\"!.env\"] must NOT re-include the sensitive file"
        );
    }

    // ── Cleanup: flipping the filter on removes previously-indexed junk ──────

    /// Cleanup already exists (`cleanup_stale` + `delete_file` cascade). Index
    /// junk with the filter OFF, then re-index with it ON: the now-excluded file
    /// is no longer visited, so `cleanup_stale` deletes its rows.
    #[test]
    fn reindex_with_filter_on_cleans_up_previously_indexed_junk() {
        use indicatif::MultiProgress;

        let db = open_db();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn a() {}\n").unwrap();
        std::fs::write(
            dir.path().join("app.min.js"),
            "var x=1;\nfunction f(){return x;}\n",
        )
        .unwrap();
        let args = default_args(dir.path().to_path_buf());
        let mp = MultiProgress::new();

        // Filter fully off: the minified file is indexed like any JS file.
        let mut cfg_off = crate::config::Config::default();
        cfg_off.index.use_default_excludes = false;
        cfg_off.index.detect_generated = false;
        let r1 = run_parse_phase(dir.path(), &db, &args, &mp, &cfg_off).unwrap();
        assert_eq!(r1.filtered, 0);
        let indexed_off: Vec<String> = db
            .file_paths_under("")
            .unwrap()
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        assert!(
            indexed_off.iter().any(|p| p == "app.min.js"),
            "junk is indexed while the filter is off"
        );

        // Filter on (defaults): re-index; the excluded junk row is cleaned up.
        let cfg_on = crate::config::Config::default();
        let r2 = run_parse_phase(dir.path(), &db, &args, &mp, &cfg_on).unwrap();
        assert!(r2.filtered >= 1, "app.min.js filtered on re-index");
        let indexed_on: Vec<String> = db
            .file_paths_under("")
            .unwrap()
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        assert!(
            !indexed_on.iter().any(|p| p == "app.min.js"),
            "excluded junk must be removed from the index on re-index"
        );
        assert!(
            indexed_on.iter().any(|p| p == "lib.rs"),
            "the real source file remains indexed"
        );
    }

    // ── Dir-prune re-include at the walk level: matches git ──────────────────

    /// A `!file` re-include CANNOT escape an excluded parent directory (the walk
    /// never descends into node_modules/), but a `!dir/` re-include of the
    /// directory itself DOES bring its contents back. This is the walk-level
    /// counterpart to filter.rs's `dir_prune_reinclude_semantics`, exercised
    /// end-to-end through `collect_files`.
    #[test]
    fn collect_files_reinclude_respects_pruned_parent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn a() {}\n").unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("node_modules/keep.js"), "var x=1;\n").unwrap();
        std::fs::create_dir(dir.path().join("vendor")).unwrap();
        std::fs::write(dir.path().join("vendor/util.rs"), "fn v() {}\n").unwrap();

        // A `!file` inside a pruned dir does not resurrect it.
        let file_reinclude =
            IndexFilter::build(&["!node_modules/keep.js".to_string()], true, true).unwrap();
        let (files, _) = collect_files(dir.path(), &file_reinclude).unwrap();
        let names = collected_names(&files);
        assert!(
            !names.contains(&"keep.js".to_string()),
            "a !file line must not re-include a file under a pruned directory"
        );
        assert!(names.contains(&"lib.rs".to_string()));

        // A `!dir/` re-include of the directory brings its contents back.
        let dir_reinclude = IndexFilter::build(&["!vendor/".to_string()], true, true).unwrap();
        let (files2, _) = collect_files(dir.path(), &dir_reinclude).unwrap();
        let names2 = collected_names(&files2);
        assert!(
            names2.contains(&"util.rs".to_string()),
            "a !dir/ line must re-include the directory's contents"
        );
    }

    // ── Filtered-count contract: pruned-dir contents are never counted ───────

    /// The documented count semantics: files inside a pruned directory are NOT
    /// added to `filtered` (the walk never descends, so there is no per-file
    /// decision to count). Only file-level excludes at reachable depths are
    /// counted. Pins the contract so a later change to walk pruning can't
    /// silently start (or stop) counting descendants.
    #[test]
    fn collect_files_pruned_dir_contents_never_counted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn a() {}\n").unwrap();
        // One reachable, file-level exclude: counted.
        std::fs::write(dir.path().join("package-lock.json"), "{}\n").unwrap();
        // Many files (and a nested subdir) inside a pruned directory: none counted.
        std::fs::create_dir_all(dir.path().join("node_modules/react/lib")).unwrap();
        std::fs::write(dir.path().join("node_modules/a.js"), "1\n").unwrap();
        std::fs::write(dir.path().join("node_modules/b.js"), "2\n").unwrap();
        std::fs::write(dir.path().join("node_modules/react/index.js"), "3\n").unwrap();
        std::fs::write(dir.path().join("node_modules/react/lib/c.js"), "4\n").unwrap();

        let filter = IndexFilter::build(&[], true, true).unwrap();
        let (files, filtered) = collect_files(dir.path(), &filter).unwrap();

        assert!(collected_names(&files).contains(&"lib.rs".to_string()));
        assert_eq!(
            filtered, 1,
            "only the reachable file-level exclude is counted; the 4 files under \
             the pruned node_modules/ are never descended into, so never counted"
        );
    }

    // ── Marker exemption: a `!`-re-included file skips generated-marker sniff ──

    /// A user `!` re-include yields `ForceInclude`, which the collect loop treats
    /// as "keep AND skip the generated-marker check". So a re-included file that
    /// carries a `@generated` header still survives, whereas the identical file
    /// without the re-include is dropped by marker detection.
    #[test]
    fn collect_files_reincluded_file_exempt_from_marker() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gen.js"),
            "// @generated\nfunction f(){return 1;}\n",
        )
        .unwrap();

        // Without a re-include: the marker drops it.
        let plain = IndexFilter::build(&[], true, true).unwrap();
        let (files, filtered) = collect_files(dir.path(), &plain).unwrap();
        assert!(!collected_names(&files).contains(&"gen.js".to_string()));
        assert_eq!(filtered, 1);

        // With `!gen.js`: ForceInclude exempts it from marker detection.
        let reincluded = IndexFilter::build(&["!gen.js".to_string()], true, true).unwrap();
        let (files2, filtered2) = collect_files(dir.path(), &reincluded).unwrap();
        assert!(
            collected_names(&files2).contains(&"gen.js".to_string()),
            "a !re-included file must be exempt from generated-marker detection"
        );
        assert_eq!(filtered2, 0);
    }

    // ── Sensitive-layer defense-in-depth: non-dotfile key materials ──────────

    /// The sensitive `OverrideBuilder` (not the index filter) drops key-material
    /// files. `[index].exclude` re-include attempts have no effect, and turning
    /// the whole index filter off (`use_default_excludes=false`) does not expose
    /// them either - proving the two layers are independent. Complements the
    /// `.env` case with non-dotfile patterns (`*.pem`, private keys) so the
    /// invariant isn't only tested on hidden files.
    #[test]
    fn sensitive_key_material_never_reincludable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("server.pem"), "-----BEGIN-----\n").unwrap();
        std::fs::write(dir.path().join("id_rsa"), "-----BEGIN-----\n").unwrap();
        std::fs::write(dir.path().join("tls.key"), "-----BEGIN-----\n").unwrap();

        // Filter off entirely, plus explicit re-include attempts for each.
        let filter = IndexFilter::build(
            &[
                "!server.pem".to_string(),
                "!id_rsa".to_string(),
                "!tls.key".to_string(),
            ],
            false,
            false,
        )
        .unwrap();
        let (files, _) = collect_files(dir.path(), &filter).unwrap();
        let names = collected_names(&files);

        assert!(names.contains(&"keep.rs".to_string()));
        for secret in ["server.pem", "id_rsa", "tls.key"] {
            assert!(
                !names.contains(&secret.to_string()),
                "sensitive file {secret} must stay excluded regardless of [index] config"
            );
        }
    }

    // ── Cleanup-on-reindex: full cascade across every table ──────────────────

    /// Strengthens the file-row cleanup test: after flipping the filter on and
    /// re-indexing, the previously-indexed junk must be gone from files, chunks,
    /// embeddings, AND graph_edges - not just the files row - while the real
    /// source file's rows in every table survive. Embeddings are inserted by
    /// hand after run 1 (the parse phase never embeds), so the assertion proves
    /// the `delete_file` cascade clears the embeddings table too.
    #[test]
    fn reindex_with_filter_on_cleans_up_all_tables() {
        use indicatif::MultiProgress;

        let db = open_db();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn real() {}\n").unwrap();
        // A JS file that parses into named chunks (so graph/mention edges exist)
        // and is dropped by the default `*.min.js` glob once the filter is on.
        std::fs::write(
            dir.path().join("app.min.js"),
            "function junk(){return 1;}\nfunction more(){return junk();}\n",
        )
        .unwrap();
        let args = default_args(dir.path().to_path_buf());
        let mp = MultiProgress::new();

        // Run 1: filter off; the junk is indexed like any JS file.
        let mut cfg_off = crate::config::Config::default();
        cfg_off.index.use_default_excludes = false;
        cfg_off.index.detect_generated = false;
        run_parse_phase(dir.path(), &db, &args, &mp, &cfg_off).unwrap();

        // The junk has chunks and graph edges before cleanup.
        let junk_chunks = db.chunks_for_file("app.min.js").unwrap();
        assert!(
            !junk_chunks.is_empty(),
            "junk must have chunk rows while the filter is off"
        );
        assert!(
            !db.edges_for_file("app.min.js").unwrap().is_empty(),
            "junk must have graph/mention edge rows while the filter is off"
        );

        // Embed both files by hand (the parse phase never writes embeddings).
        for c in db.chunks_for_file("app.min.js").unwrap() {
            db.insert_embedding(
                c.chunk_id,
                &vec![0.1f32; spelunk_core::embeddings::EMBEDDING_DIM],
            )
            .unwrap();
        }
        for c in db.chunks_for_file("lib.rs").unwrap() {
            db.insert_embedding(
                c.chunk_id,
                &vec![0.2f32; spelunk_core::embeddings::EMBEDDING_DIM],
            )
            .unwrap();
        }
        let embeddings_before = db.stats().unwrap().embedding_count;
        assert_eq!(
            embeddings_before as usize,
            db.chunks_for_file("app.min.js").unwrap().len()
                + db.chunks_for_file("lib.rs").unwrap().len(),
            "both files' chunks are embedded before cleanup"
        );

        // Run 2: filter on; app.min.js is excluded, so cleanup_stale purges it.
        let cfg_on = crate::config::Config::default();
        let r2 = run_parse_phase(dir.path(), &db, &args, &mp, &cfg_on).unwrap();
        assert!(r2.filtered >= 1, "app.min.js is filtered on re-index");

        // Files row gone.
        let files_now: Vec<String> = db
            .file_paths_under("")
            .unwrap()
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        assert!(!files_now.iter().any(|p| p == "app.min.js"));
        assert!(files_now.iter().any(|p| p == "lib.rs"));

        // Chunks gone for junk, kept for the real file.
        assert!(
            db.chunks_for_file("app.min.js").unwrap().is_empty(),
            "junk chunk rows must be deleted on cleanup"
        );
        let real_chunks = db.chunks_for_file("lib.rs").unwrap();
        assert!(!real_chunks.is_empty(), "real file's chunks survive");

        // Graph edges gone for junk.
        assert!(
            db.edges_for_file("app.min.js").unwrap().is_empty(),
            "junk graph/mention edge rows must be deleted on cleanup"
        );

        // Embeddings: only the real file's remain.
        let embeddings_after = db.stats().unwrap().embedding_count;
        assert_eq!(
            embeddings_after as usize,
            real_chunks.len(),
            "the junk's embedding rows must be gone; only the real file's remain"
        );
        assert!(
            embeddings_after < embeddings_before,
            "cleanup must reduce the embedding count"
        );
    }

    // ── Output: the filtered-count notice line ───────────────────────────────

    #[test]
    fn filtered_notice_names_count_and_override_location() {
        let s = filtered_notice(7);
        assert!(s.contains("Filtered out 7"));
        assert!(s.contains("[index]"));
        assert!(s.contains(".spelunk/config.toml"));
    }
}
