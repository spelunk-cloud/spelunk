/// Cross-project memory dep pass — ADR-003.
///
/// For `spelunk memory search/list` and `spelunk context`, after querying the
/// local backend, this module walks the registry dependency graph and surfaces
/// `locked` or `cross-project`-tagged decisions and requirements from each
/// linked project's `memory.db`.
///
/// Only `kind == "decision"` or `kind == "requirement"` entries with
/// `status == "active"` and at least one of the tags `locked` or
/// `cross-project` are returned — see §1 of ADR-003 for the privacy rationale.
///
/// Results are tagged with `source_project` / `source_project_path` so the
/// consuming agent can tell which project a surfaced entry originated from.
use std::path::Path;

use crate::registry::{Project, Registry};
use crate::storage::memory::Note;
use crate::storage::{LocalMemoryBackend, MemoryBackend, MemoryStore, NoteId};

/// Cross-cutting kinds that are eligible for cross-project surfacing (§1 ADR-003).
const CROSS_CUTTING_KINDS: &[&str] = &["decision", "requirement"];

/// Tags that opt an entry into cross-project visibility (§1 ADR-003).
const CROSS_PROJECT_TAGS: &[&str] = &["locked", "cross-project"];

/// Resolve the dep `Project` list for the current working directory, using
/// `index_db_path` (the primary `.spelunk/index.db`) to locate the project in
/// the registry.
///
/// Returns an empty vec (not an error) when:
/// - The registry cannot be opened.
/// - The current project is not registered.
/// - The project has no registered deps.
///
/// This matches the graceful-degradation contract in `search.rs`
/// (`resolve_project_and_deps` / `search_all_dbs_linearrag`).
fn resolve_dep_projects(index_db_path: &Path) -> Vec<Project> {
    let Ok(reg) = Registry::open() else {
        return vec![];
    };
    // index_db_path = <root>/.spelunk/index.db
    // parent        = <root>/.spelunk
    // parent.parent = <root>
    let project_root = index_db_path
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(index_db_path);
    let Ok(Some(project)) = reg.find_project_for_path(project_root) else {
        return vec![];
    };
    reg.get_deps(project.id).unwrap_or_default()
}

/// Query a single dep project's `memory.db` for cross-cutting entries and tag
/// them with `source_project` / `source_project_path`.
///
/// Silently skips deps whose `memory.db` does not exist (common when a project
/// is linked for code search but has no memory entries yet).
/// Emits `tracing::warn!` and skips on open/query errors (corrupt DB, etc.),
/// matching `search_all_dbs_linearrag`'s `"could not open dep DB"` pattern.
async fn query_dep_cross_cutting(dep: &Project) -> Vec<Note> {
    let mem_db_path = dep.db_path.with_file_name("memory.db");
    if !mem_db_path.exists() {
        return vec![];
    }

    let store = match MemoryStore::open(&mem_db_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "cross-project memory: could not open dep DB {}: {e}",
                mem_db_path.display()
            );
            return vec![];
        }
    };
    let backend = LocalMemoryBackend::new(store);

    let source_project = crate::cli::cmd::helpers::project_display_name(&dep.root_path);
    let source_project_path = dep.root_path.to_string_lossy().into_owned();

    let mut cross_cutting = Vec::new();

    for kind in CROSS_CUTTING_KINDS {
        // Fetch all active entries of this kind (up to the NoteStore cap of 500).
        let notes = match backend.list(Some(kind), 500, false, None).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    "cross-project memory: list failed for dep {} kind={kind}: {e}",
                    mem_db_path.display()
                );
                continue;
            }
        };

        for mut note in notes {
            if is_cross_cutting(&note.tags) {
                note.source_project = Some(source_project.clone());
                note.source_project_path = Some(source_project_path.clone());
                cross_cutting.push(note);
            }
        }
    }

    cross_cutting
}

/// Return `true` when `tags` contains at least one of `CROSS_PROJECT_TAGS`.
fn is_cross_cutting(tags: &[String]) -> bool {
    tags.iter()
        .any(|t| CROSS_PROJECT_TAGS.iter().any(|&ct| t == ct))
}

/// Collect cross-cutting notes from all dep projects of the primary project
/// (identified by `index_db_path`).
///
/// `seen` is a set of `(root_path_string, id)` pairs for entries already
/// emitted, allowing deduplication when two deps link to a shared grandparent
/// project — per ADR-003 §3 ("dedupe by `(root_path, id)`").
///
/// Returns the aggregated dep notes in registry `project_deps` iteration order
/// (same as `search_all_dbs_linearrag`), each dep's notes in their natural
/// list order.
pub(crate) async fn collect_dep_cross_cutting(
    index_db_path: &Path,
    seen: &mut std::collections::HashSet<(String, NoteId)>,
) -> Vec<Note> {
    let deps = resolve_dep_projects(index_db_path);
    let mut result = Vec::new();
    for dep in &deps {
        let root_key = dep.root_path.to_string_lossy().into_owned();
        for note in query_dep_cross_cutting(dep).await {
            // Deduplicate by (source project root, entry id).
            if seen.insert((root_key.clone(), note.id.clone())) {
                result.push(note);
            }
        }
    }
    result
}
