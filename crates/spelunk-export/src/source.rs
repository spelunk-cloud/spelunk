//! Reading a local store, at any shape it has ever had.
//!
//! Every store is opened read-only and read with plain SQL over probed
//! columns. There is no schema ladder here and no version constant: the shape
//! is discovered from the file, so a store written by any past release reads
//! the same way as one written yesterday, and a store written by a future
//! release loses only the columns this build has never heard of.
//!
//! Nothing derived is read. Full text indexes, embeddings, import cursors,
//! marker tables and engine shadow tables are all reconstructible from the
//! authored rows or from the user's own source tree, and reading them would
//! only create a way to carry stale state forward.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

use crate::dump::{CommandUsage, Dump, MemoryEntry, Project, Relationship};

/// What a store turned out to contain, and anything the read could not honour.
pub struct Extract {
    pub dump: Dump,
    pub warnings: Vec<String>,
}

/// Open a store for reading and nothing else.
///
/// A URI with `immutable=1` would avoid touching the directory at all, but it
/// lies to SQLite about a file another process may be writing, and a store in
/// write-ahead-log mode keeps committed rows in the log: reading it as
/// immutable would silently return a stale database. So the content guarantee
/// is that no byte of the database or its log is changed, not that the
/// directory gains no file. Reading a log-mode store nobody has open brings
/// SQLite's shared-memory index and an empty log into being, and a read-only
/// connection has no way to decline that.
pub fn open_read_only(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {} read-only", path.display()))?;
    // A store in the older rollback journal mode locks out readers while a
    // writer commits. Holding the read open across every table means such a
    // writer can now block the export where it previously could not, so wait
    // out a passing one rather than refusing. A store that is locked for longer
    // than this is held by something that is not passing, and the right answer
    // there is to stop with a reason.
    conn.busy_timeout(WRITER_PATIENCE)
        .context("configuring how long to wait for a writer")?;
    Ok(conn)
}

const WRITER_PATIENCE: std::time::Duration = std::time::Duration::from_secs(5);

fn tables(conn: &Connection) -> Result<BTreeSet<String>> {
    let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type='table'")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn columns(conn: &Connection, table: &str) -> Result<BTreeSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn schema_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

pub fn count(conn: &Connection, table: &str) -> Result<i64> {
    Ok(conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
}

/// Split the comma joined representation the stores use for lists.
///
/// Reproduces the product's own splitting exactly, including that a value
/// containing a comma is indistinguishable from two values. That ambiguity is
/// in the stored data, not introduced here, and the format cannot resolve it
/// without inventing information.
fn list(value: Option<String>) -> Vec<String> {
    match value.as_deref() {
        None | Some("") => vec![],
        Some(s) => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    }
}

/// Read every authored surface the file happens to contain, as of a single
/// point in time.
///
/// Content is detected from table shape rather than declared by the caller, so
/// one code path covers the per project store, the registry, the accumulated
/// command counts that live beside a code index, and a multi project server
/// store. A caller that had to name the kind would be a caller that has to be
/// kept in step with every historic layout.
///
/// Every table is read inside one read transaction. Without it each statement
/// gets its own snapshot, so entries come from one instant and links from a
/// later one, and a writer committing an entry and then a link referencing it
/// lands between them. The link then names an entry the read never returned,
/// which is indistinguishable from a link whose endpoint really is gone: the
/// live data would be dropped, reported as a broken store, and the export would
/// still succeed. One transaction is what makes a reported broken link true.
pub fn extract(conn: &Connection) -> Result<Extract> {
    let snapshot = conn
        .unchecked_transaction()
        .context("opening a read transaction over the store")?;
    let extracted = extract_snapshot(&snapshot)?;
    snapshot
        .rollback()
        .context("closing the read transaction")?;
    Ok(extracted)
}

fn extract_snapshot(conn: &Connection) -> Result<Extract> {
    let tables = tables(conn)?;
    let mut dump = Dump::default();
    let mut warnings = Vec::new();

    if tables.contains("notes") {
        let cols = columns(conn, "notes")?;
        if cols.contains("project_id") {
            read_namespaced_entries(conn, &cols, &tables, &mut dump, &mut warnings)?;
        } else {
            read_entries(conn, &cols, &tables, &mut dump, &mut warnings)?;
        }
    }

    if tables.contains("projects") && columns(conn, "projects")?.contains("root_path") {
        read_projects(conn, &tables, &mut dump, &mut warnings)?;
    }

    if tables.contains("usage") {
        read_usage(conn, &mut dump)?;
    }

    Ok(Extract { dump, warnings })
}

struct EntryCols {
    has: BTreeSet<String>,
}

impl EntryCols {
    fn optional(&self, name: &str) -> bool {
        self.has.contains(name)
    }

    /// The select list, with absent columns replaced by NULL so the row reader
    /// can index by a fixed position regardless of the store's age.
    fn select(&self, base: &[&str], optional: &[&str]) -> String {
        let mut parts: Vec<String> = base.iter().map(|c| (*c).to_string()).collect();
        for c in optional {
            if self.optional(c) {
                parts.push((*c).to_string());
            } else {
                parts.push(format!("NULL AS {c}"));
            }
        }
        parts.join(", ")
    }
}

const ENTRY_OPTIONAL: &[&str] = &[
    "status",
    "superseded_by",
    "source_ref",
    "valid_at",
    "invalid_at",
    "uuid",
    "remote_id",
    "entity_id",
];

fn require(cols: &BTreeSet<String>, needed: &[&str]) -> Result<()> {
    let missing: Vec<&str> = needed
        .iter()
        .copied()
        .filter(|c| !cols.contains(*c))
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "the 'notes' table is missing required columns: {}",
            missing.join(", ")
        );
    }
    Ok(())
}

fn read_entries(
    conn: &Connection,
    cols: &BTreeSet<String>,
    tables: &BTreeSet<String>,
    dump: &mut Dump,
    warnings: &mut Vec<String>,
) -> Result<()> {
    require(cols, &["id", "kind", "title", "body", "created_at"])?;
    let ec = EntryCols { has: cols.clone() };
    let has_tags = ec.optional("tags");
    let has_linked = ec.optional("linked_files");
    let sql = format!(
        "SELECT id, kind, title, body, {}, {}, created_at, {} FROM notes ORDER BY id",
        if has_tags { "tags" } else { "NULL AS tags" },
        if has_linked {
            "linked_files"
        } else {
            "NULL AS linked_files"
        },
        ec.select(&[], ENTRY_OPTIONAL)
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, Option<String>>(7)?,
            r.get::<_, Option<i64>>(8)?,
            r.get::<_, Option<String>>(9)?,
            r.get::<_, Option<i64>>(10)?,
            r.get::<_, Option<i64>>(11)?,
            r.get::<_, Option<String>>(12)?,
            r.get::<_, Option<String>>(13)?,
            r.get::<_, Option<String>>(14)?,
        ))
    })?;

    let mut supersedes: Vec<(i64, i64)> = Vec::new();
    let mut present: HashSet<i64> = HashSet::new();
    for row in rows {
        let row = row?;
        present.insert(row.0);
        if let Some(successor) = row.8 {
            supersedes.push((successor, row.0));
        }
        dump.entries.push(MemoryEntry {
            record: "entity".into(),
            kind_tag: "memory_entry".into(),
            reference: entry_ref(row.0),
            uuid: row.12,
            kind: row.1,
            title: row.2,
            body: row.3,
            tags: list(row.4),
            linked_files: list(row.5),
            created_at: row.6,
            status: row.7,
            source_ref: row.9,
            valid_at: row.10,
            invalid_at: row.11,
            entity_id: row.14,
            remote_id: row.13,
            namespace: None,
        });
    }

    let mut rels: BTreeSet<Relationship> = BTreeSet::new();
    for (successor, predecessor) in supersedes {
        if !present.contains(&successor) {
            warnings.push(format!(
                "entry {predecessor} names a successor ({successor}) that is not in the \
                 store; the supersede link is not carried"
            ));
            continue;
        }
        rels.insert(Relationship::new(
            "supersedes",
            entry_ref(successor),
            entry_ref(predecessor),
            None,
        ));
    }

    if tables.contains("memory_edges") {
        read_edges(
            conn,
            "memory_edges",
            &present,
            entry_ref,
            &mut rels,
            warnings,
        )?;
    }
    dump.relationships.extend(rels);
    Ok(())
}

/// A multi project store keys its entries by an internal project id; the
/// portable form names the project instead, because an internal id means
/// nothing outside the file it came from.
fn read_namespaced_entries(
    conn: &Connection,
    cols: &BTreeSet<String>,
    tables: &BTreeSet<String>,
    dump: &mut Dump,
    warnings: &mut Vec<String>,
) -> Result<()> {
    require(cols, &["id", "kind", "title", "body", "created_at"])?;
    let ec = EntryCols { has: cols.clone() };
    let mut slugs: HashMap<i64, String> = HashMap::new();
    if tables.contains("projects") && columns(conn, "projects")?.contains("slug") {
        let mut stmt = conn.prepare("SELECT id, slug FROM projects")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, slug) = row?;
            slugs.insert(id, slug);
        }
    }

    let has_tags = ec.optional("tags");
    let has_linked = ec.optional("linked_files");
    // `uuid` is deliberately not selected even when a column of that name
    // exists on a multi project store: the identifiers such a store mints are
    // arrival ordered rather than creation ordered, so presenting one as the
    // entry's stable identity would hand a reader an ordering it did not
    // choose. Absent identity is assigned by the reader, seeded from the
    // entry's own creation time.
    let sql = format!(
        "SELECT id, project_id, kind, title, body, {}, {}, created_at, {} FROM notes ORDER BY id",
        if has_tags { "tags" } else { "NULL AS tags" },
        if has_linked {
            "linked_files"
        } else {
            "NULL AS linked_files"
        },
        ec.select(
            &[],
            &[
                "status",
                "superseded_by",
                "source_ref",
                "valid_at",
                "invalid_at",
                "remote_id",
                "entity_id",
            ]
        )
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<i64>>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, Option<String>>(8)?,
            r.get::<_, Option<i64>>(9)?,
            r.get::<_, Option<String>>(10)?,
            r.get::<_, Option<i64>>(11)?,
            r.get::<_, Option<i64>>(12)?,
            r.get::<_, Option<String>>(13)?,
            r.get::<_, Option<String>>(14)?,
        ))
    })?;

    let mut supersedes: Vec<(i64, i64)> = Vec::new();
    let mut present: HashSet<i64> = HashSet::new();
    for row in rows {
        let row = row?;
        present.insert(row.0);
        if let Some(successor) = row.9 {
            supersedes.push((successor, row.0));
        }
        dump.entries.push(MemoryEntry {
            record: "entity".into(),
            kind_tag: "memory_entry".into(),
            reference: namespaced_ref(row.0),
            uuid: None,
            kind: row.2,
            title: row.3,
            body: row.4,
            tags: list(row.5),
            linked_files: list(row.6),
            created_at: row.7,
            status: row.8,
            source_ref: row.10,
            valid_at: row.11,
            invalid_at: row.12,
            entity_id: row.14,
            remote_id: row.13,
            namespace: row.1.and_then(|pid| slugs.get(&pid).cloned()),
        });
    }

    let mut rels: BTreeSet<Relationship> = BTreeSet::new();
    for (successor, predecessor) in supersedes {
        if !present.contains(&successor) {
            warnings.push(format!(
                "entry {predecessor} names a successor ({successor}) that is not in the \
                 store; the supersede link is not carried"
            ));
            continue;
        }
        rels.insert(Relationship::new(
            "supersedes",
            namespaced_ref(successor),
            namespaced_ref(predecessor),
            None,
        ));
    }

    if tables.contains("note_edges") {
        read_edges(
            conn,
            "note_edges",
            &present,
            namespaced_ref,
            &mut rels,
            warnings,
        )?;
    }
    dump.relationships.extend(rels);
    Ok(())
}

fn read_edges(
    conn: &Connection,
    table: &'static str,
    present: &HashSet<i64>,
    make_ref: fn(i64) -> String,
    rels: &mut BTreeSet<Relationship>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let has_created = columns(conn, table)?.contains("created_at");
    let sql = format!(
        "SELECT from_id, to_id, kind, {} FROM {table} ORDER BY from_id, to_id, kind",
        if has_created {
            "created_at"
        } else {
            "NULL AS created_at"
        }
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<i64>>(3)?,
        ))
    })?;
    for row in rows {
        let (from, to, kind, created_at) = row?;
        if !present.contains(&from) || !present.contains(&to) {
            warnings.push(format!(
                "a '{kind}' link between {from} and {to} names an entry that is not in the \
                 store; it is not carried"
            ));
            continue;
        }
        // Insertion order matters only for the earliest timestamp to win, and
        // a set keyed on the whole record would keep both. Replace an existing
        // link only when this one carries a timestamp and the stored one does
        // not, so a lifecycle-derived link never shadows a recorded one.
        let candidate = Relationship::new(&kind, make_ref(from), make_ref(to), created_at);
        let existing = rels
            .iter()
            .find(|r| {
                r.kind_tag == candidate.kind_tag && r.from == candidate.from && r.to == candidate.to
            })
            .cloned();
        match existing {
            Some(prev) if prev.created_at.is_none() && candidate.created_at.is_some() => {
                rels.remove(&prev);
                rels.insert(candidate);
            }
            Some(_) => {}
            None => {
                rels.insert(candidate);
            }
        }
    }
    Ok(())
}

fn read_projects(
    conn: &Connection,
    tables: &BTreeSet<String>,
    dump: &mut Dump,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let cols = columns(conn, "projects")?;
    let has_registered = cols.contains("registered_at");
    let sql = format!(
        "SELECT id, root_path, {} FROM projects ORDER BY id",
        if has_registered {
            "registered_at"
        } else {
            "NULL AS registered_at"
        }
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<i64>>(2)?,
        ))
    })?;
    let mut present: HashSet<i64> = HashSet::new();
    for row in rows {
        let (id, root_path, registered_at) = row?;
        present.insert(id);
        dump.projects.push(Project {
            record: "entity".into(),
            kind_tag: "project".into(),
            reference: project_ref(id),
            root_path,
            registered_at,
        });
    }

    if tables.contains("project_deps") {
        let mut stmt = conn
            .prepare("SELECT project_id, dep_id FROM project_deps ORDER BY project_id, dep_id")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (from, to) = row?;
            if !present.contains(&from) || !present.contains(&to) {
                warnings.push(format!(
                    "a dependency between projects {from} and {to} names a project that is not \
                     registered; it is not carried"
                ));
                continue;
            }
            dump.relationships.push(Relationship::new(
                "depends_on",
                project_ref(from),
                project_ref(to),
                None,
            ));
        }
    }
    Ok(())
}

fn read_usage(conn: &Connection, dump: &mut Dump) -> Result<()> {
    let mut stmt =
        conn.prepare("SELECT command, called_at FROM usage ORDER BY called_at, rowid")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for (n, row) in rows.enumerate() {
        let (command, at) = row?;
        dump.usage.push(CommandUsage {
            record: "entity".into(),
            kind_tag: "command_usage".into(),
            reference: format!("u{n}"),
            command,
            at,
        });
    }
    Ok(())
}

fn entry_ref(id: i64) -> String {
    format!("e{id}")
}

fn namespaced_ref(id: i64) -> String {
    format!("n{id}")
}

fn project_ref(id: i64) -> String {
    format!("p{id}")
}
