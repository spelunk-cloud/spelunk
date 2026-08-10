//! Describing what is on the machine without touching any of it.
//!
//! Every field here is answerable by opening a file read-only or by asking the
//! filesystem, and nothing in this module writes. That is the whole contract:
//! a caller can run it against a live installation and be sure it changed
//! nothing, which is what makes it safe to run before deciding anything.

use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::source;

#[derive(Serialize)]
pub struct StoreReport {
    pub path: String,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contents: Vec<TableReport>,
    /// Present when the file exists but could not be read. A store that cannot
    /// be opened is reported rather than aborting the whole inventory: the
    /// caller usually has several stores and one unreadable file should not
    /// cost them the report on the rest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unreadable: Option<String>,
}

#[derive(Serialize)]
pub struct TableReport {
    pub table: String,
    pub rows: i64,
}

/// Only authored tables are counted. Derived tables are excluded here for the
/// same reason they are excluded from a dump: reporting them would invite a
/// caller to believe they need carrying.
const AUTHORED: &[&str] = &[
    "notes",
    "memory_edges",
    "note_edges",
    "projects",
    "project_deps",
    "usage",
];

pub fn describe(path: &Path) -> StoreReport {
    let mut report = StoreReport {
        path: path.display().to_string(),
        exists: path.exists(),
        bytes: None,
        schema_version: None,
        contents: Vec::new(),
        unreadable: None,
    };
    if !report.exists {
        return report;
    }
    report.bytes = std::fs::metadata(path).ok().map(|m| m.len());
    match inspect(path, &mut report) {
        Ok(()) => {}
        Err(e) => report.unreadable = Some(e.to_string()),
    }
    report
}

fn inspect(path: &Path, report: &mut StoreReport) -> Result<()> {
    let conn = source::open_read_only(path)?;
    report.schema_version = Some(source::schema_version(&conn)?);
    for table in AUTHORED {
        // A missing table is the normal case for every store: each one holds a
        // different subset. Only a present-but-unreadable table is a problem,
        // and that surfaces as the count query failing on the next store.
        if let Ok(rows) = source::count(&conn, table) {
            report.contents.push(TableReport {
                table: (*table).to_string(),
                rows,
            });
        }
    }
    Ok(())
}
