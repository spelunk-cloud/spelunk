//! Global project registry.
//!
//! Stores all known project roots and their dependency relationships in a
//! single SQLite database at `~/.config/spelunk/registry.db`.
//!
//! The registry is separate from per-project index DBs.  It is used to:
//!   - Auto-detect which project the user is working in from their CWD
//!   - Track cross-project dependencies for multi-repo search
//!   - Power `spelunk status --all` and `spelunk autoclean`

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Project {
    pub id: i64,
    pub root_path: PathBuf,
    pub db_path: PathBuf,
    #[allow(dead_code)]
    pub registered_at: i64,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct Registry {
    conn: Connection,
}

impl Registry {
    /// Open (or create) the global registry database.
    pub fn open() -> Result<Self> {
        let path = registry_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating registry directory {}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("opening registry at {}", path.display()))?;
        let reg = Self { conn };
        reg.init()?;
        Ok(reg)
    }

    fn init(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "
            PRAGMA journal_mode=WAL;
            PRAGMA foreign_keys=ON;

            CREATE TABLE IF NOT EXISTS projects (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                root_path     TEXT    NOT NULL UNIQUE,
                db_path       TEXT    NOT NULL,
                registered_at INTEGER NOT NULL DEFAULT (unixepoch())
            );

            CREATE TABLE IF NOT EXISTS project_deps (
                project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                dep_id     INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                PRIMARY KEY (project_id, dep_id)
            );
        ",
            )
            .context("initialising registry schema")?;
        Ok(())
    }

    // ── Registration ──────────────────────────────────────────────────────────

    /// Register (or update) a project.  Returns the project's id.
    pub fn register(&self, root: &Path, db: &Path) -> Result<i64> {
        let root_str = root.to_string_lossy();
        let db_str = db.to_string_lossy();
        self.conn
            .execute(
                "INSERT INTO projects (root_path, db_path)
             VALUES (?1, ?2)
             ON CONFLICT(root_path) DO UPDATE SET db_path = excluded.db_path",
                params![root_str, db_str],
            )
            .context("registering project")?;
        let id: i64 = self
            .conn
            .query_row(
                "SELECT id FROM projects WHERE root_path = ?1",
                params![root_str],
                |row| row.get(0),
            )
            .context("fetching project id after register")?;
        Ok(id)
    }

    // ── Lookup ────────────────────────────────────────────────────────────────

    /// Find the closest ancestor of `start` that is a registered project root.
    /// If none found in the registry, falls back to filesystem walk looking for
    /// `.spelunk/index.db` and auto-registers what it finds.
    ///
    /// If `start` is inside a git linked worktree, the walk begins from the
    /// main worktree root so commands run inside a worktree find the shared DB.
    pub fn find_project_for_path(&self, start: &Path) -> Result<Option<Project>> {
        // If start is inside a git linked worktree, resolve to the main
        // worktree root so the shared index is found without a symlink.
        let search_root = crate::utils::resolve_main_worktree_root(start);

        // 1. Registry walk-up (most specific first)
        let mut dir = search_root.clone();
        loop {
            let dir_str = dir.to_string_lossy().to_string();
            let maybe: Option<(i64, String, String, i64)> = self
                .conn
                .query_row(
                    "SELECT id, root_path, db_path, registered_at
                 FROM projects WHERE root_path = ?1",
                    params![dir_str],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()
                .context("querying registry")?;

            if let Some((id, root_path, db_path, registered_at)) = maybe {
                return Ok(Some(Project {
                    id,
                    root_path: PathBuf::from(root_path),
                    db_path: PathBuf::from(db_path),
                    registered_at,
                }));
            }
            if !dir.pop() {
                break;
            }
        }

        // 2. Filesystem fallback — look for .spelunk/index.db and auto-register.
        let mut dir = search_root;
        loop {
            let candidate = dir.join(".spelunk").join("index.db");
            if candidate.exists() {
                let id = self.register(&dir, &candidate)?;
                return Ok(Some(Project {
                    id,
                    root_path: dir.clone(),
                    db_path: candidate,
                    registered_at: 0,
                }));
            }
            if !dir.pop() {
                break;
            }
        }

        Ok(None)
    }

    /// Find a project by its exact root path.
    pub fn find_by_root(&self, root: &Path) -> Result<Option<Project>> {
        let root_str = root.to_string_lossy().to_string();
        self.conn
            .query_row(
                "SELECT id, root_path, db_path, registered_at
             FROM projects WHERE root_path = ?1",
                params![root_str],
                |row| {
                    Ok(Project {
                        id: row.get(0)?,
                        root_path: PathBuf::from(row.get::<_, String>(1)?),
                        db_path: PathBuf::from(row.get::<_, String>(2)?),
                        registered_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .context("querying registry by root")
    }

    // ── Dependencies ──────────────────────────────────────────────────────────

    /// Return all dep DB paths for a project (direct deps only).
    pub fn get_deps(&self, project_id: i64) -> Result<Vec<Project>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT p.id, p.root_path, p.db_path, p.registered_at
             FROM projects p
             JOIN project_deps d ON d.dep_id = p.id
             WHERE d.project_id = ?1",
            )
            .context("preparing dep query")?;

        let rows = stmt
            .query_map(params![project_id], |row| {
                Ok(Project {
                    id: row.get(0)?,
                    root_path: PathBuf::from(row.get::<_, String>(1)?),
                    db_path: PathBuf::from(row.get::<_, String>(2)?),
                    registered_at: row.get(3)?,
                })
            })
            .context("querying deps")?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("reading dep rows")
    }

    /// Add a dependency: `from_id` depends on `dep_id`.
    pub fn add_dep(&self, from_id: i64, dep_id: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO project_deps (project_id, dep_id) VALUES (?1, ?2)",
                params![from_id, dep_id],
            )
            .context("adding dependency")?;
        Ok(())
    }

    /// Remove a dependency.
    pub fn remove_dep(&self, from_id: i64, dep_id: i64) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM project_deps WHERE project_id = ?1 AND dep_id = ?2",
                params![from_id, dep_id],
            )
            .context("removing dependency")?;
        Ok(())
    }

    // ── Listing ───────────────────────────────────────────────────────────────

    /// Return all registered projects, ordered by root_path.
    pub fn all_projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, root_path, db_path, registered_at
             FROM projects ORDER BY root_path",
            )
            .context("preparing all-projects query")?;

        let rows = stmt
            .query_map([], |row| {
                Ok(Project {
                    id: row.get(0)?,
                    root_path: PathBuf::from(row.get::<_, String>(1)?),
                    db_path: PathBuf::from(row.get::<_, String>(2)?),
                    registered_at: row.get(3)?,
                })
            })
            .context("querying all projects")?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("reading project rows")
    }

    /// Return projects that list `project_id` as a dependency (reverse deps).
    #[allow(dead_code)]
    pub fn projects_depending_on(&self, project_id: i64) -> Result<Vec<Project>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT p.id, p.root_path, p.db_path, p.registered_at
             FROM projects p
             JOIN project_deps d ON d.project_id = p.id
             WHERE d.dep_id = ?1",
            )
            .context("preparing reverse-dep query")?;

        let rows = stmt
            .query_map(params![project_id], |row| {
                Ok(Project {
                    id: row.get(0)?,
                    root_path: PathBuf::from(row.get::<_, String>(1)?),
                    db_path: PathBuf::from(row.get::<_, String>(2)?),
                    registered_at: row.get(3)?,
                })
            })
            .context("querying reverse deps")?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("reading reverse-dep rows")
    }

    // ── Autoclean ─────────────────────────────────────────────────────────────

    /// Remove all registry entries whose root path no longer exists on disk.
    /// Returns the list of removed root paths.
    pub fn autoclean(&self) -> Result<Vec<String>> {
        let projects = self.all_projects()?;
        let mut removed = Vec::new();
        for p in projects {
            let is_gone = !p.root_path.exists();
            let is_remnant = !is_gone && spelunk_only_remnant(&p.root_path);

            if is_gone || is_remnant {
                if is_remnant {
                    // Remove the leftover .spelunk dir (worktree was cleaned but
                    // .spelunk was skipped because it is in .gitignore).
                    let spelunk_dir = p.root_path.join(".spelunk");
                    std::fs::remove_dir_all(&spelunk_dir)
                        .with_context(|| format!("removing {}", spelunk_dir.display()))?;
                    // Root dir is now empty — remove it too.
                    let _ = std::fs::remove_dir(&p.root_path);
                }
                self.conn
                    .execute("DELETE FROM projects WHERE id = ?1", params![p.id])
                    .context("deleting stale project from registry")?;
                removed.push(p.root_path.to_string_lossy().to_string());
            }
        }
        Ok(removed)
    }
}

// ---------------------------------------------------------------------------
// Project context resolution
// ---------------------------------------------------------------------------

/// Resolved project context for the current working directory.
///
/// Combines registry lookup, explicit `--db` override, and filesystem fallback
/// into a single value that any CLI/server command can use to find its index.
pub struct ResolvedProject {
    /// Registered `Project` entry if CWD resolved into the registry.
    /// `None` for explicit `--db` overrides and filesystem-fallback paths.
    pub project: Option<Project>,
    /// Path to the SQLite index. May not exist on disk — callers requiring
    /// existence must check it themselves.
    pub db_path: PathBuf,
    /// Linked dependency projects (only populated when `project` is `Some`).
    /// Filtered to those whose `db_path` exists on disk.
    pub deps: Vec<Project>,
}

/// Resolve the project context for the current working directory.
///
/// Priority:
/// 1. Explicit `--db` override (skips registry; `project` and `deps` are empty).
/// 2. Registry lookup for the closest ancestor of CWD that has a registered
///    project (or `.spelunk/index.db` auto-detection via `find_project_for_path`).
/// 3. Filesystem fallback — `resolve_db(None, cfg_default_db)`.
///
/// Never fails based on file existence. Callers that require the index to
/// exist must check `db_path.exists()` themselves.
pub fn resolve_project_context(
    explicit_db: Option<&Path>,
    cfg_default_db: &Path,
) -> Result<ResolvedProject> {
    if let Some(p) = explicit_db {
        return Ok(ResolvedProject {
            project: None,
            db_path: p.to_path_buf(),
            deps: vec![],
        });
    }

    if let Ok(reg) = Registry::open()
        && let Ok(cwd) = std::env::current_dir()
        && let Ok(Some(project)) = reg.find_project_for_path(&cwd)
        && project.db_path.exists()
    {
        let deps = reg
            .get_deps(project.id)
            .unwrap_or_default()
            .into_iter()
            .filter(|d| d.db_path.exists())
            .collect();
        let db_path = project.db_path.clone();
        return Ok(ResolvedProject {
            project: Some(project),
            db_path,
            deps,
        });
    }

    Ok(ResolvedProject {
        project: None,
        db_path: crate::config::resolve_db(None, cfg_default_db),
        deps: vec![],
    })
}

/// Returns true if `path` exists but contains only a `.spelunk` subdirectory —
/// i.e. it is a git worktree remnant where everything tracked was removed but
/// the gitignored `.spelunk` folder was left behind.
fn spelunk_only_remnant(path: &std::path::Path) -> bool {
    let Ok(mut entries) = std::fs::read_dir(path) else {
        return false;
    };
    match (entries.next(), entries.next()) {
        (Some(Ok(entry)), None) => entry.file_name() == ".spelunk",
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn registry_path() -> Result<PathBuf> {
    // `SPELUNK_REGISTRY_DIR` overrides the registry location. This exists for
    // test isolation: the default uses `dirs::config_dir()`, which on Windows
    // resolves via the Known Folder API (`%APPDATA%`) and is not redirectable by
    // `HOME`/env, so tests cannot otherwise point the CLI at a temp registry.
    if let Ok(dir) = std::env::var("SPELUNK_REGISTRY_DIR")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir).join("registry.db"));
    }
    let base = dirs::config_dir()
        .context("could not determine user config directory")?
        .join("spelunk");
    Ok(base.join("registry.db"))
}

// Allow `.optional()` on query_row results without boilerplate.
trait OptionalExt<T> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for rusqlite::Result<T> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
