use anyhow::Result;

use super::Database;

/// A registered spec file.
#[derive(Debug, serde::Serialize)]
pub struct SpecRecord {
    pub id: i64,
    pub path: String,
    pub title: String,
    pub is_auto: bool,
}

/// A spec that is potentially out-of-date with its linked code.
#[derive(Debug, serde::Serialize)]
pub struct StaleSpec {
    pub spec_id: i64,
    pub spec_path: String,
    pub title: String,
    pub linked_path: String,
    pub spec_indexed_at: i64,
    pub code_indexed_at: i64,
}

impl Database {
    /// Register or update a spec file. Returns the spec id.
    pub fn upsert_spec(&self, path: &str, title: &str, is_auto: bool) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO specs (path, title, is_auto)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET
                title   = excluded.title,
                is_auto = excluded.is_auto",
            rusqlite::params![path, title, is_auto as i64],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM specs WHERE path = ?1",
            rusqlite::params![path],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Look up a spec by its path. Returns None if not registered.
    pub fn spec_by_path(&self, path: &str) -> Result<Option<SpecRecord>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, path, title, is_auto FROM specs WHERE path = ?1")?;
        let mut rows = stmt.query(rusqlite::params![path])?;
        if let Some(row) = rows.next()? {
            Ok(Some(SpecRecord {
                id: row.get(0)?,
                path: row.get(1)?,
                title: row.get(2)?,
                is_auto: row.get::<_, i64>(3)? != 0,
            }))
        } else {
            Ok(None)
        }
    }

    /// Return all registered specs with their linked paths.
    pub fn all_specs(&self) -> Result<Vec<SpecRecord>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, path, title, is_auto FROM specs ORDER BY path")?;
        let rows = stmt.query_map([], |r| {
            Ok(SpecRecord {
                id: r.get(0)?,
                path: r.get(1)?,
                title: r.get(2)?,
                is_auto: r.get::<_, i64>(3)? != 0,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Return all linked paths for a spec.
    pub fn spec_links(&self, spec_id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT linked_path FROM spec_links WHERE spec_id = ?1 ORDER BY linked_path",
        )?;
        let rows = stmt.query_map(rusqlite::params![spec_id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Add a link from a spec to a path prefix (idempotent).
    pub fn add_spec_link(&self, spec_id: i64, linked_path: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO spec_links (spec_id, linked_path) VALUES (?1, ?2)",
            rusqlite::params![spec_id, linked_path],
        )?;
        Ok(())
    }

    /// Remove a specific link from a spec.
    pub fn remove_spec_link(&self, spec_id: i64, linked_path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM spec_links WHERE spec_id = ?1 AND linked_path = ?2",
            rusqlite::params![spec_id, linked_path],
        )?;
        Ok(())
    }

    /// Delete a spec and all its links.
    pub fn delete_spec(&self, spec_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM specs WHERE id = ?1",
            rusqlite::params![spec_id],
        )?;
        Ok(())
    }

    /// For a set of file paths, return the specs that govern them (via linked_path prefix match).
    /// Returns deduplicated (spec_path, title) pairs.
    pub fn specs_for_files(&self, file_paths: &[String]) -> Result<Vec<(String, String)>> {
        if file_paths.is_empty() {
            return Ok(vec![]);
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT s.path, s.title, sl.linked_path
             FROM spec_links sl
             JOIN specs s ON s.id = sl.spec_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;

        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for row in rows {
            let (spec_path, title, linked_path) = row?;
            for fp in file_paths {
                if fp.starts_with(&linked_path) || fp == &linked_path {
                    if seen.insert(spec_path.clone()) {
                        result.push((spec_path.clone(), title.clone()));
                    }
                    break;
                }
            }
        }
        Ok(result)
    }

    /// Return specs whose linked code has been indexed more recently than the spec itself.
    pub fn stale_specs(&self) -> Result<Vec<StaleSpec>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT s.id, s.path, s.title, sl.linked_path,
                    sf.indexed_at  AS spec_indexed_at,
                    lf.indexed_at  AS code_indexed_at
             FROM specs s
             JOIN spec_links sl ON sl.spec_id = s.id
             JOIN files sf      ON sf.path = s.path
             JOIN files lf      ON (lf.path = sl.linked_path
                                    OR lf.path LIKE replace(replace(replace(sl.linked_path,
                                        '\\', '\\\\'), '%', '\\%'), '_', '\\_') || '%' ESCAPE '\\')
             WHERE lf.indexed_at > sf.indexed_at
             ORDER BY s.path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(StaleSpec {
                spec_id: r.get(0)?,
                spec_path: r.get(1)?,
                title: r.get(2)?,
                linked_path: r.get(3)?,
                spec_indexed_at: r.get(4)?,
                code_indexed_at: r.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}
