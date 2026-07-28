use anyhow::{Context, Result};
use std::path::Path;

use super::Database;

/// Aggregate statistics for the live index.
#[derive(Debug, serde::Serialize)]
pub struct IndexStats {
    pub file_count: i64,
    pub chunk_count: i64,
    pub embedding_count: i64,
    pub last_indexed: Option<i64>,
}

/// Token-weighted view of the embed queue, for progress reporting. Chunk
/// counts answer "what can search see" (coverage); token sums answer "how much
/// embed work remains" (progress). The two are different questions and must
/// never be rendered under one name.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct EmbedTokenStats {
    /// Sum of `token_count` over all chunks. 0 on an empty or pre-backfill index.
    pub total_tokens: i64,
    /// Sum of `token_count` over chunks with no embedding row.
    pub pending_tokens: i64,
}

/// Result of a lightweight random-sample staleness probe.
#[derive(Debug, serde::Serialize)]
pub struct StalenessReport {
    /// Number of files sampled.
    pub sampled: usize,
    /// Number of sampled files whose on-disk hash differs from the stored hash.
    pub stale: usize,
    /// Paths of the stale files in the sample.
    pub stale_paths: Vec<String>,
    /// Estimated percentage of files in the full index that are stale (0–100).
    pub estimated_stale_pct: f32,
    /// Unix timestamp of the most recently indexed file, or None if the index is empty.
    pub last_indexed_at: Option<i64>,
}

/// Language breakdown: how many files are indexed for each detected language.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LanguageStat {
    /// The detected language name (e.g. `"rust"`, `"python"`), or `"unknown"` when
    /// the language column is NULL.
    pub name: String,
    /// Number of indexed files in this language.
    pub file_count: i64,
}

/// A file that appears to have drifted behind the rest of the project.
#[derive(Debug, serde::Serialize)]
pub struct DriftCandidate {
    pub path: String,
    /// Days behind the most recently indexed file in the project.
    pub days_behind: i64,
    /// Number of distinct files that call/import symbols from this file.
    pub caller_count: i64,
}

impl Database {
    pub fn stats(&self) -> Result<IndexStats> {
        let file_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        let chunk_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
        let embedding_count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))?;
        let last_indexed: Option<i64> = self
            .conn
            .query_row("SELECT MAX(indexed_at) FROM files", [], |r| r.get(0))
            .ok()
            .flatten();
        Ok(IndexStats {
            file_count,
            chunk_count,
            embedding_count,
            last_indexed,
        })
    }

    /// Token-weighted embed-queue totals (see [`EmbedTokenStats`]).
    pub fn embed_token_stats(&self) -> Result<EmbedTokenStats> {
        let total_tokens: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(token_count), 0) FROM chunks",
            [],
            |r| r.get(0),
        )?;
        let pending_tokens: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(c.token_count), 0)
             FROM chunks c
             LEFT JOIN embeddings e ON e.chunk_id = c.id
             WHERE e.chunk_id IS NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(EmbedTokenStats {
            total_tokens,
            pending_tokens,
        })
    }

    /// Return per-language file counts, ordered by count descending.
    ///
    /// Files with a NULL language column are grouped under `"unknown"`.
    pub fn language_stats(&self) -> Result<Vec<LanguageStat>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT COALESCE(language, 'unknown') AS lang, COUNT(*) AS cnt \
             FROM files \
             GROUP BY lang \
             ORDER BY cnt DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(LanguageStat {
                name: row.get(0)?,
                file_count: row.get(1)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("querying language stats")
    }

    /// Sample up to `n` random files and compare on-disk blake3 hashes to stored hashes.
    /// Designed to be fast (<10 ms for n=20).
    pub fn sample_staleness_check(&self, n: usize) -> Result<StalenessReport> {
        let last_indexed_at: Option<i64> = self
            .conn
            .query_row("SELECT MAX(indexed_at) FROM files", [], |r| r.get(0))
            .ok()
            .flatten();

        let mut stmt = self
            .conn
            .prepare("SELECT id, path, hash FROM files ORDER BY RANDOM() LIMIT ?1")?;
        let rows = stmt.query_map(rusqlite::params![n as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;

        let sampled_rows: Vec<(i64, String, String)> =
            rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let sampled = sampled_rows.len();
        let mut stale = 0usize;
        let mut stale_paths: Vec<String> = Vec::new();

        for (_id, path, stored_hash) in &sampled_rows {
            let is_stale = match std::fs::read(path) {
                Ok(bytes) => format!("{}", blake3::hash(&bytes)) != *stored_hash,
                Err(_) => true,
            };
            if is_stale {
                stale += 1;
                stale_paths.push(path.clone());
            }
        }

        let estimated_stale_pct = if sampled == 0 {
            0.0
        } else {
            stale as f32 / sampled as f32 * 100.0
        };

        Ok(StalenessReport {
            sampled,
            stale,
            stale_paths,
            estimated_stale_pct,
            last_indexed_at,
        })
    }

    /// Files that haven't changed while the rest of the project has.
    pub fn drift_candidates(
        &self,
        min_days_behind: i64,
        limit: usize,
    ) -> Result<Vec<DriftCandidate>> {
        let newest: i64 = self
            .conn
            .query_row("SELECT MAX(indexed_at) FROM files", [], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .ok()
            .flatten()
            .unwrap_or(0);

        if newest == 0 {
            return Ok(vec![]);
        }

        let mut stmt = self.conn.prepare(
            "SELECT
                 f.path,
                 (:newest - f.indexed_at) / 86400 AS days_behind,
                 (SELECT COUNT(DISTINCT e.source_file)
                  FROM graph_edges e
                  JOIN chunks c ON c.file_id = f.id AND c.name = e.target_name
                  WHERE e.source_file != f.path) AS caller_count
             FROM files f
             WHERE days_behind >= :min_days
             ORDER BY days_behind DESC
             LIMIT :lim",
        )?;

        let candidates = stmt
            .query_map(
                rusqlite::named_params! {
                    ":newest":   newest,
                    ":min_days": min_days_behind,
                    ":lim":      limit as i64,
                },
                |row| {
                    Ok(DriftCandidate {
                        path: row.get(0)?,
                        days_behind: row.get(1)?,
                        caller_count: row.get(2)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(candidates)
    }

    /// Record a command invocation. Fire-and-forget: errors are silently discarded.
    pub fn record_usage(&self, command: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let _ = self.conn.execute(
            "INSERT INTO usage (command, called_at) VALUES (?1, ?2)",
            rusqlite::params![command, now],
        );
    }

    /// Return `(command, count)` rows for the last 7 days, ordered by count descending.
    pub fn usage_last_7_days(&self) -> Result<Vec<(String, i64)>> {
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
            - 7 * 24 * 3600;
        let mut stmt = self.conn.prepare_cached(
            "SELECT command, COUNT(*) FROM usage \
             WHERE called_at > ?1 \
             GROUP BY command \
             ORDER BY COUNT(*) DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![cutoff], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("querying usage stats")
    }
}

/// Record a command invocation at `db_path` without requiring a `Database` handle.
/// Opens a raw connection and inserts into the `usage` table. Fire-and-forget.
pub fn record_usage_at(db_path: &Path, command: &str) {
    use rusqlite::Connection;
    let Ok(conn) = Connection::open(db_path) else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let _ = conn.execute(
        "INSERT INTO usage (command, called_at) VALUES (?1, ?2)",
        rusqlite::params![command, now],
    );
}
