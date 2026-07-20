use anyhow::Result;

use super::Database;

impl Database {
    /// K-nearest-neighbour search using sqlite-vec.
    ///
    /// Takes the raw float query vector; it is int8-quantised here to match the
    /// `embeddings` `int8[896]` column (see `embeddings::vec_to_int8_blob`).
    /// Returns results ordered by ascending distance (closest first).
    pub fn search_similar(
        &self,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<crate::search::SearchResult>> {
        let query_blob = crate::embeddings::vec_to_int8_blob(query);
        let limit = limit.min(1_000);
        let sql = format!(
            "WITH knn AS (
                 SELECT chunk_id, distance
                 FROM   embeddings
                 WHERE  embedding MATCH vec_int8(?1)
                   AND  k = {limit}
             )
             SELECT  k.chunk_id,
                     CAST(k.distance AS REAL),
                     c.node_type,
                     c.name,
                     CAST(c.start_line AS INTEGER),
                     CAST(c.end_line   AS INTEGER),
                     c.content,
                     f.path,
                     f.language,
                     c.token_count,
                     c.graph_rank
             FROM knn k
             JOIN chunks c ON c.id = k.chunk_id
             JOIN files  f ON f.id = c.file_id
             ORDER BY k.distance"
        );

        const GRAPH_RANK_ALPHA: f32 = 0.15;

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![&query_blob], |row| {
            // int8 L2 distance is ~127× the f32 distance; rescale to keep the
            // graph-rank blend (and downstream `1 - distance`) on the old scale.
            let raw_distance: f32 = row.get::<_, f32>(1)? / crate::embeddings::INT8_SCALE;
            let graph_rank: f32 = row.get(10)?;
            let blended = raw_distance * (1.0 - GRAPH_RANK_ALPHA) - graph_rank * GRAPH_RANK_ALPHA;
            Ok(crate::search::SearchResult {
                chunk_id: row.get(0)?,
                distance: blended,
                node_type: row.get(2)?,
                name: row.get(3)?,
                start_line: row.get::<_, i64>(4)? as usize,
                end_line: row.get::<_, i64>(5)? as usize,
                content: row.get(6)?,
                file_path: row.get(7)?,
                language: row.get(8)?,
                from_graph: false,
                governing_specs: vec![],
                token_count: row.get::<_, i64>(9)? as usize,
                project_name: None,
                project_path: None,
                summary: None,
            })
        })?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// FTS5 full-text search. Returns results ranked by BM25 (best match first).
    ///
    /// BM25 in FTS5 returns negative values (more negative = better match).
    /// We negate the score so that higher `distance` values indicate better matches,
    /// consistent with the convention used in `SearchResult`.
    pub fn search_text(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::search::SearchResult>> {
        let limit = limit.min(1_000);
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.node_type, c.name,
                    CAST(c.start_line AS INTEGER), CAST(c.end_line AS INTEGER),
                    c.content, f.path, f.language,
                    bm25(chunks_fts) AS score
             FROM chunks_fts
             JOIN chunks c ON chunks_fts.rowid = c.id
             JOIN files  f ON c.file_id = f.id
             WHERE chunks_fts MATCH ?1
             ORDER BY score
             LIMIT ?2",
        )?;
        let fts_query = crate::utils::fts5_quote_literal(query);
        let rows = stmt.query_map(rusqlite::params![fts_query, limit as i64], |row| {
            let bm25_score: f64 = row.get(8)?;
            Ok(crate::search::SearchResult {
                chunk_id: row.get(0)?,
                node_type: row.get(1)?,
                name: row.get(2)?,
                start_line: row.get::<_, i64>(3)? as usize,
                end_line: row.get::<_, i64>(4)? as usize,
                content: row.get(5)?,
                file_path: row.get(6)?,
                language: row.get(7)?,
                // Negate so that more-relevant results have a lower distance,
                // matching the ascending-distance convention of vector search.
                distance: (-bm25_score) as f32,
                from_graph: false,
                governing_specs: vec![],
                token_count: 0,
                project_name: None,
                project_path: None,
                summary: None,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Hybrid search: fuses FTS5 BM25 ranking with vector KNN via Reciprocal Rank Fusion.
    ///
    /// RRF score: `Σ 1 / (k + rank_i)` where `k = 60`.
    pub fn search_hybrid(
        &self,
        query: &str,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<crate::search::SearchResult>> {
        use std::collections::HashMap;

        let candidates = (limit * 3).max(20);
        let vec_results = self.search_similar(embedding, candidates)?;
        let text_results = self.search_text(query, candidates).unwrap_or_default();

        const K: f64 = 60.0;

        let mut scores: HashMap<i64, f64> = HashMap::new();
        let mut by_id: HashMap<i64, crate::search::SearchResult> = HashMap::new();

        for (rank, result) in vec_results.into_iter().enumerate() {
            let rrf = 1.0 / (K + (rank + 1) as f64);
            *scores.entry(result.chunk_id).or_insert(0.0) += rrf;
            by_id.entry(result.chunk_id).or_insert(result);
        }

        for (rank, result) in text_results.into_iter().enumerate() {
            let rrf = 1.0 / (K + (rank + 1) as f64);
            *scores.entry(result.chunk_id).or_insert(0.0) += rrf;
            by_id.entry(result.chunk_id).or_insert(result);
        }

        let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(limit);

        let results = ranked
            .into_iter()
            .filter_map(|(id, rrf_score)| {
                by_id.remove(&id).map(|mut r| {
                    r.distance = (1.0 / rrf_score) as f32;
                    r
                })
            })
            .collect();

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::super::Database;
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
        Database::open(std::path::Path::new(":memory:")).expect("failed to open in-memory Database")
    }

    fn seed_chunk(db: &Database, content: &str) {
        let file_id = db
            .upsert_file("src/lib.rs", Some("rust"), "deadbeef", 0)
            .expect("upsert file");
        db.insert_chunk(file_id, "function", Some("f"), 1, 5, content, None, 4)
            .expect("insert chunk");
    }

    /// A search term containing FTS5-special punctuation (unbalanced `"`,
    /// a bare `:`, and boolean-looking keywords) must never surface a raw
    /// FTS5 parse error — it should be treated as a literal string and either
    /// return matches or an empty result, but always `Ok`.
    #[test]
    fn search_text_with_punctuation_never_errors() {
        let db = open_db();
        seed_chunk(&db, "fn parse_config() { /* handles foo:bar */ }");

        let queries = [
            "foo:bar",
            "\"unterminated quote",
            "a OR NOT b",
            "weird (parens",
            "trailing*",
            "-leading-dash",
            "",
            "a NEAR b",
            "a NEAR/3 b",
            "content:secret", // attempted FTS5 column-filter injection
            "\"\"\"\"\"",     // consecutive quotes
            "^prefix",
            "((()))",
        ];
        for q in queries {
            let result = db.search_text(q, 10);
            assert!(
                result.is_ok(),
                "query {q:?} must not surface a raw FTS5 parse error, got: {:?}",
                result.err()
            );
        }
    }

    /// A query term containing an embedded NUL byte must not surface a raw
    /// FTS5 "unterminated string" parse error. `fts5_quote_literal` strips
    /// embedded `\0` before quoting, since FTS5's own query-string parser
    /// treats `\0` as an early string terminator (distinct from SQLite's
    /// NUL-safe text binding), which would otherwise hide the closing `"` we
    /// append.
    #[test]
    fn search_text_embedded_nul_byte_still_leaks_raw_parse_error() {
        let db = open_db();
        seed_chunk(&db, "fn parse_config() { /* handles foo:bar */ }");

        let result = db.search_text("\0embedded nul", 10);
        assert!(
            result.is_ok(),
            "query with embedded NUL must not surface a raw FTS5 parse error, got: {:?}",
            result.err()
        );
    }

    /// A literal-quoted term still matches real content containing that
    /// literal substring, so quoting doesn't silently break search relevance.
    #[test]
    fn search_text_quoted_colon_term_still_matches() {
        let db = open_db();
        seed_chunk(&db, "fn parse_config() { /* handles foo:bar */ }");

        let results = db.search_text("parse_config", 10).expect("search ok");
        assert!(
            !results.is_empty(),
            "expected the seeded chunk to match a plain-word query"
        );
    }

    /// A term shaped like an FTS5 column filter (`content:...`) must be
    /// treated as a literal string to search for, not interpreted as
    /// targeting the `content` column. Quoted, it should behave like any
    /// other safe literal search (no match on an unrelated nonsense term,
    /// no error).
    #[test]
    fn search_text_column_filter_syntax_is_literal_not_a_filter() {
        let db = open_db();
        seed_chunk(&db, "fn parse_config() { /* handles foo:bar */ }");

        let result = db.search_text("content:nonexistent_term_xyz", 10);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }
}
