// Upgrade corpus ("DB museum"): open artifacts written by real released
// binaries with the current build and assert the upgrade preserves them.
//
// Every wing under `fixtures/upgrade-corpus/wings/` was produced by an actual
// downloaded release, not by constructing an old shape by hand. The expected
// values in MANIFEST.json were read out of each artifact at capture time with
// plain SQL, before any current-binary code touched it, so they are an
// independent record of what the old binary wrote rather than an echo of what
// today's migrations happen to produce.
//
// Regenerate with scripts/upgrade-corpus/generate.sh.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use spelunk_core::registry::Registry;
use spelunk_core::storage::{Database, GitNotesBackend, MemoryBackend, MemoryStore};
use spelunk_core::test_support::git_command;

// sqlite-vec is registered process-globally, before any connection is opened.
// Without it every vec0 table in the corpus fails to load and the row-count
// assertions would be reading an error, not an empty table.
fn register_sqlite_vec() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        #[allow(clippy::missing_transmute_annotations)]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

#[derive(Debug, Deserialize)]
struct Manifest {
    wings: Vec<Wing>,
}

#[derive(Debug, Deserialize)]
struct Wing {
    id: String,
    producer: String,
    kind: String,
    artifact: String,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    expect: Expect,
}

#[derive(Debug, Default, Deserialize)]
struct Expect {
    #[serde(default)]
    file_count: i64,
    #[serde(default)]
    chunk_count: i64,
    #[serde(default)]
    embedding_count: i64,
    #[serde(default)]
    graph_edge_count: i64,
    // "int8" wings keep their vectors across the upgrade; a "float768" wing
    // must lose them, because mixed-dimension vectors can never be compared.
    #[serde(default)]
    vector_storage: String,
    #[serde(default)]
    fts_query: String,
    #[serde(default)]
    fts_expect_path: String,
    #[serde(default)]
    note_count: i64,
    #[serde(default)]
    active_note_count: i64,
    #[serde(default)]
    archived_title: String,
    #[serde(default)]
    superseded_title: String,
    #[serde(default)]
    successor_title: String,
    #[serde(default)]
    memory_fts_query: String,
    #[serde(default)]
    note_vector_count: i64,
    // Whether the captured artifact already had the ADR-068 column. False marks
    // a wing whose whole purpose is to make the backfill run.
    #[serde(default)]
    entity_id_present: bool,
    #[serde(default)]
    project_count: i64,
    #[serde(default)]
    dep_count: i64,
    #[serde(default)]
    era_entries: Vec<EraEntry>,
}

#[derive(Debug, Deserialize)]
struct EraEntry {
    title: String,
    kind: String,
    body: String,
}

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("upgrade-corpus")
}

fn manifest() -> Manifest {
    let path = corpus_root().join("MANIFEST.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading corpus manifest {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("parsing corpus manifest")
}

// Expand a wing's artifact into a temp dir. Every test works on that copy:
// opening a database runs migrations, which would otherwise rewrite the
// checked-in fixture and destroy the very thing under test on the first run.
//
// Artifacts are stored gzipped because a captured database is mostly the vec0
// extension's preallocated vector chunk, and that is zeros.
fn checkout(wing: &Wing, tmp: &Path) -> PathBuf {
    let src = corpus_root()
        .join("wings")
        .join(&wing.id)
        .join(&wing.artifact);
    let dst = tmp.join(wing.artifact.trim_end_matches(".gz"));

    if !wing.artifact.ends_with(".gz") {
        std::fs::copy(&src, &dst)
            .unwrap_or_else(|e| panic!("copying wing {} from {}: {e}", wing.id, src.display()));
        return dst;
    }

    let packed = std::fs::File::open(&src)
        .unwrap_or_else(|e| panic!("opening wing {} at {}: {e}", wing.id, src.display()));
    let mut reader = flate2::read::GzDecoder::new(std::io::BufReader::new(packed));
    let mut out =
        std::fs::File::create(&dst).unwrap_or_else(|e| panic!("creating {}: {e}", dst.display()));
    std::io::copy(&mut reader, &mut out)
        .unwrap_or_else(|e| panic!("expanding wing {}: {e}", wing.id));
    dst
}

// `Database`/`MemoryStore` keep their connection private, so the header and
// schema assertions read the file through their own connection. Callers open
// this only after the typed handle has been dropped.
fn raw(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path)
        .unwrap_or_else(|e| panic!("opening {} directly: {e}", path.display()))
}

// The schema version a brand-new DB is stamped with, derived by creating one
// rather than by importing the crate's constant: an upgraded field DB must land
// on exactly the version a fresh install produces.
fn fresh_index_schema_version() -> i32 {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("fresh.db");
    Database::open(&path).expect("opening fresh index db");
    read_user_version(&raw(&path))
}

fn read_user_version(conn: &rusqlite::Connection) -> i32 {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("reading user_version")
}

fn vector_column_type(conn: &rusqlite::Connection, table: &str) -> String {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
        rusqlite::params![table],
        |r| r.get::<_, String>(0),
    )
    .unwrap_or_else(|e| panic!("reading {table} schema: {e}"))
}

// Callers pass `<name>_rowids` for a vector count: a vec0 virtual table cannot
// be counted directly, but the shadow table the extension maintains alongside
// it is an ordinary table and has one row per stored vector.
fn row_count(conn: &rusqlite::Connection, table: &str) -> i64 {
    let sql = format!("SELECT count(*) FROM {table}");
    conn.query_row(&sql, [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("counting {table}: {e}"))
}

fn has_column(conn: &rusqlite::Connection, table: &str, column: &str) -> bool {
    let sql = format!("SELECT count(*) FROM pragma_table_info('{table}') WHERE name = ?1");
    conn.query_row(&sql, rusqlite::params![column], |r| r.get::<_, i64>(0))
        .unwrap_or_else(|e| panic!("probing {table}.{column}: {e}"))
        > 0
}

// The two SQLite header fields that move on any write transaction: the file
// change counter (bytes 24..28) and the version-valid-for number (92..96).
const CHANGE_COUNTER: std::ops::Range<usize> = 24..28;
const VERSION_VALID_FOR: std::ops::Range<usize> = 92..96;

// Fold the WAL back into the main file, then return its bytes with those two
// counters masked out.
//
// Checkpointing matters because the idempotency check compares files, and a
// write that only ever reached the -wal sidecar would otherwise read as
// "nothing changed".
//
// Masking matters because the two stores differ in whether a redundant open
// takes a write transaction at all, and that difference is not what is being
// measured here. `Database::open` returns before any write once the header
// already reads the current version, so an index wing comes out of a second
// open byte-identical even unmasked. `MemoryStore::open` runs the entity-id
// backfill and unique-index promotion on every open regardless of version, so
// a memory wing takes a write that settles on identical content and moves only
// these two counters. Measured, not assumed: with no mask at all the index
// wings differ at no offset and the memory wings differ at exactly bytes 27
// and 95, the low bytes of these two fields.
//
// So this tolerates a write that changed no content, and nothing else. Every
// other byte, including all page content, is compared exactly.
fn content_image(path: &Path) -> Vec<u8> {
    {
        let conn = raw(path);
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .expect("checkpointing the write-ahead log");
    }
    let mut bytes =
        std::fs::read(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    for range in [CHANGE_COUNTER, VERSION_VALID_FOR] {
        if bytes.len() >= range.end {
            bytes[range].fill(0);
        }
    }
    bytes
}

fn wings_of_kind<'a>(m: &'a Manifest, kind: &str) -> Vec<&'a Wing> {
    m.wings.iter().filter(|w| w.kind == kind).collect()
}

fn unbundle(bundle: &Path, into: &Path) {
    let parent = into.parent().expect("clone target has a parent directory");
    let status = git_command(parent)
        .args(["clone", "--quiet"])
        .arg(bundle)
        .arg(into)
        .status()
        .expect("running git clone on the corpus bundle");
    assert!(status.success(), "git clone of {} failed", bundle.display());
    // `git clone` of a bundle brings the branches but leaves notes behind:
    // refs/notes/* is outside the default refspec.
    let status = git_command(into)
        .args([
            "fetch",
            "--quiet",
            "origin",
            "refs/notes/spelunk:refs/notes/spelunk",
        ])
        .status()
        .expect("fetching refs/notes/spelunk from the corpus bundle");
    assert!(status.success(), "fetching notes ref failed");
}

// Criterion 1: every wing opens, migrates, and keeps its rows and content.

#[test]
#[serial_test::serial]
fn the_corpus_is_not_empty_and_every_wing_is_present() {
    let m = manifest();
    assert!(
        !m.wings.is_empty(),
        "upgrade corpus manifest lists no wings; the museum test would pass vacuously"
    );
    for wing in &m.wings {
        let path = corpus_root()
            .join("wings")
            .join(&wing.id)
            .join(&wing.artifact);
        assert!(
            path.exists(),
            "wing {} (produced by {}) is listed in the manifest but its artifact {} is missing",
            wing.id,
            wing.producer,
            path.display()
        );

        // An artifact that no longer hashes to what was captured is no longer
        // evidence about the release named in `producer`. The expectations in
        // this manifest were read out of *those* bytes, so silently swapping
        // them (a regenerated wing committed without recapturing, a fixture
        // hand-edited to make a test pass) would leave the suite asserting one
        // artifact's contents against another's.
        assert!(
            !wing.sha256.is_empty(),
            "wing {} has no recorded artifact digest, so nothing ties the \
             expectations below to the bytes they were read from",
            wing.id
        );
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("reading wing {} at {}: {e}", wing.id, path.display()));
        let digest = <sha2::Sha256 as sha2::Digest>::digest(&bytes);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, wing.sha256,
            "wing {} no longer matches the artifact its expectations were \
             captured from",
            wing.id
        );
    }
}

#[test]
#[serial_test::serial]
fn every_index_wing_migrates_with_its_rows_and_content_intact() {
    register_sqlite_vec();
    let m = manifest();
    let expected_version = fresh_index_schema_version();
    let index_wings = wings_of_kind(&m, "index");
    assert!(!index_wings.is_empty(), "corpus has no index.db wing");

    for wing in index_wings {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = checkout(wing, tmp.path());
        let db = Database::open(&db_path)
            .unwrap_or_else(|e| panic!("opening wing {} with the current build: {e}", wing.id));

        assert_eq!(
            read_user_version(&raw(&db_path)),
            expected_version,
            "wing {} did not land on the schema version a fresh install produces",
            wing.id
        );

        let stats = db.stats().expect("reading index stats");
        assert_eq!(
            stats.file_count, wing.expect.file_count,
            "wing {}: file count changed across the upgrade",
            wing.id
        );
        assert_eq!(
            stats.chunk_count, wing.expect.chunk_count,
            "wing {}: chunk count changed across the upgrade",
            wing.id
        );

        let hits = db
            .search_text(&wing.expect.fts_query, 10)
            .expect("full-text search over the upgraded index");
        assert!(
            hits.iter()
                .any(|h| h.file_path == wing.expect.fts_expect_path),
            "wing {}: FTS for {:?} lost the pre-existing chunk from {}; got {:?}",
            wing.id,
            wing.expect.fts_query,
            wing.expect.fts_expect_path,
            hits.iter().map(|h| &h.file_path).collect::<Vec<_>>()
        );
        assert!(
            hits.iter()
                .any(|h| h.content.contains(&wing.expect.fts_query)),
            "wing {}: chunk text no longer contains {:?} after the upgrade",
            wing.id,
            wing.expect.fts_query
        );

        drop(db);
        let conn = raw(&db_path);

        // The code graph is a whole subsystem the file and chunk counts say
        // nothing about: emptying graph_edges leaves both of them intact.
        assert_eq!(
            row_count(&conn, "graph_edges"),
            wing.expect.graph_edge_count,
            "wing {}: the code graph lost edges across the upgrade",
            wing.id
        );

        // A wing already storing int8 vectors must keep every one of them. The
        // dimension upgrade is allowed to discard 768-dimension vectors and
        // only those; a detection bug that rebuilt an int8 table as well would
        // silently cost the user their whole embedding index, and re-embedding
        // is the single most expensive thing this tool asks of them.
        if wing.expect.vector_storage != "float768" {
            assert_eq!(
                row_count(&conn, "embeddings_rowids"),
                wing.expect.embedding_count,
                "wing {}: vectors were discarded from an index that was already \
                 int8, so the whole index would have to be re-embedded",
                wing.id
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial]
async fn every_memory_wing_migrates_with_its_entries_chains_and_archives_intact() {
    register_sqlite_vec();
    let m = manifest();
    let memory_wings = wings_of_kind(&m, "memory");
    assert!(!memory_wings.is_empty(), "corpus has no memory.db wing");
    assert!(
        memory_wings.iter().any(|w| !w.expect.entity_id_present),
        "every memory wing was captured after entity_id already existed, so the \
         backfill asserted below never actually runs on anything"
    );

    for wing in memory_wings {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = checkout(wing, tmp.path());

        // Confirm the artifact is the era it claims before the upgrade touches
        // it. Without this an accidentally pre-migrated fixture would make the
        // backfill assertions below pass with the backfill never running.
        assert_eq!(
            has_column(&raw(&db_path), "notes", "entity_id"),
            wing.expect.entity_id_present,
            "wing {}: the captured artifact is not the entity_id era it is \
             recorded as",
            wing.id
        );

        let store = MemoryStore::open(&db_path)
            .unwrap_or_else(|e| panic!("opening wing {} with the current build: {e}", wing.id));

        let all = store.list(None, 500, true).expect("listing all entries");
        assert_eq!(
            all.len() as i64,
            wing.expect.note_count,
            "wing {}: entry count changed across the upgrade",
            wing.id
        );

        let active = store
            .list(None, 500, false)
            .expect("listing active entries");
        assert_eq!(
            active.len() as i64,
            wing.expect.active_note_count,
            "wing {}: the archived entry stopped being hidden from the default list",
            wing.id
        );

        let archived = all
            .iter()
            .find(|n| n.title == wing.expect.archived_title)
            .unwrap_or_else(|| {
                panic!(
                    "wing {}: archived entry {:?} vanished in the upgrade",
                    wing.id, wing.expect.archived_title
                )
            });
        assert_eq!(
            archived.status, "archived",
            "wing {}: entry {:?} lost its archived status",
            wing.id, wing.expect.archived_title
        );

        let superseded = all
            .iter()
            .find(|n| n.title == wing.expect.superseded_title)
            .unwrap_or_else(|| {
                panic!(
                    "wing {}: superseded entry {:?} vanished in the upgrade",
                    wing.id, wing.expect.superseded_title
                )
            });
        // `MemoryStore` is the local store, so its ids are always rowids.
        let successor_id = superseded
            .superseded_by
            .as_ref()
            .and_then(|id| id.as_i64())
            .unwrap_or_else(|| {
                panic!(
                    "wing {}: entry {:?} lost its supersede link",
                    wing.id, wing.expect.superseded_title
                )
            });
        let successor = store
            .get(successor_id)
            .expect("reading the successor entry")
            .unwrap_or_else(|| {
                panic!(
                    "wing {}: supersede chain points at id {successor_id}, which no longer exists",
                    wing.id
                )
            });
        assert_eq!(
            successor.title, wing.expect.successor_title,
            "wing {}: the supersede chain now points at the wrong entry",
            wing.id
        );

        let hits = store
            .search_text(&wing.expect.memory_fts_query, 10, None)
            .expect("full-text search over upgraded memory");
        assert!(
            !hits.is_empty(),
            "wing {}: memory FTS for {:?} returned nothing after the upgrade",
            wing.id,
            wing.expect.memory_fts_query
        );

        let body = store
            .get(superseded.id.as_i64().expect("local rowid"))
            .expect("re-reading the superseded entry")
            .expect("superseded entry present")
            .body;
        assert!(
            !body.is_empty(),
            "wing {}: entry bodies did not survive the upgrade",
            wing.id
        );

        drop(store);
        let conn = raw(&db_path);

        // The ADR-068 backfill. This is the reason a pre-entity-id wing exists
        // at all, and it is invisible to every assertion above: entries list,
        // read and search perfectly well with the column left NULL, and only
        // start colliding later, once something tries to key on it.
        assert!(
            has_column(&conn, "notes", "entity_id"),
            "wing {}: the entity_id column is missing after the upgrade",
            wing.id
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM notes WHERE entity_id IS NULL OR trim(entity_id) = ''",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("counting entries without an entity id"),
            0,
            "wing {}: the entity_id backfill left entries without one",
            wing.id
        );
        assert_eq!(
            conn.query_row("SELECT count(DISTINCT entity_id) FROM notes", [], |r| r
                .get::<_, i64>(0))
                .expect("counting distinct entity ids"),
            wing.expect.note_count,
            "wing {}: entity ids are not distinct per entry, so the unique index \
             they are meant to carry cannot be promoted",
            wing.id
        );

        // Note vectors are as expensive to rebuild as index vectors and just as
        // invisible to a list or a search-by-text.
        assert_eq!(
            row_count(&conn, "note_embeddings_rowids"),
            wing.expect.note_vector_count,
            "wing {}: note vectors were discarded across the upgrade",
            wing.id
        );
    }
}

// The project paths exactly as the capturing release wrote them. Read before
// the current build opens the file, so it is a record of the artifact rather
// than of what today's migrations produce.
fn captured_project_paths(conn: &rusqlite::Connection) -> BTreeMap<i64, (String, String)> {
    let mut stmt = conn
        .prepare("SELECT id, root_path, db_path FROM projects")
        .expect("preparing the captured-paths query");
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                (r.get::<_, String>(1)?, r.get::<_, String>(2)?),
            ))
        })
        .expect("querying captured project paths");
    rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()
        .expect("reading captured project paths")
}

#[test]
#[serial_test::serial]
fn every_registry_wing_keeps_its_projects_and_dependency_links() {
    let m = manifest();
    let registry_wings = wings_of_kind(&m, "registry");
    assert!(!registry_wings.is_empty(), "corpus has no registry.db wing");

    for wing in registry_wings {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = checkout(wing, tmp.path());
        let captured = captured_project_paths(&raw(&db_path));

        // Nothing below can show that a path survived if the artifact never
        // held a usable one. `starts_with` matches whole components and reads
        // the string alone, so it says the same thing on every platform.
        for (id, (root, db)) in &captured {
            assert!(
                !root.is_empty() && Path::new(db).starts_with(Path::new(root)),
                "wing {}: project {} was captured without a database under its \
                 own root, so the comparison below has nothing to preserve",
                wing.id,
                id
            );
        }

        // SPELUNK_REGISTRY_DIR is the only way to point Registry::open at a
        // temp copy; dirs::config_dir() is not redirectable on every platform.
        unsafe { std::env::set_var("SPELUNK_REGISTRY_DIR", tmp.path()) };
        let registry = Registry::open()
            .unwrap_or_else(|e| panic!("opening wing {} with the current build: {e}", wing.id));

        let projects = registry
            .all_projects()
            .expect("listing registered projects");
        assert_eq!(
            projects.len() as i64,
            wing.expect.project_count,
            "wing {}: registered projects changed across the upgrade",
            wing.id
        );

        // Counting rows says nothing about what is in them, and a registry row
        // whose paths have been mangled is worse than a missing one: the
        // project still lists, and then every command against it looks at the
        // wrong place on disk.
        //
        // The check is equality with the captured bytes, not a shape test.
        // These paths belong to the machine the wing was captured on, so
        // `is_absolute` and every other host-OS path predicate answers for the
        // runner rather than for the artifact: a POSIX path is not absolute to
        // a Windows host, whatever the migration did to it. Equality is the
        // same question everywhere, and it is the stronger one anyway, since a
        // path rewritten to some other absolute path is still mangled.
        for project in &projects {
            let (root, db) = captured.get(&project.id).unwrap_or_else(|| {
                panic!(
                    "wing {}: project {} is not one of the rows the capturing \
                     release wrote",
                    wing.id, project.id
                )
            });
            assert_eq!(
                project.root_path,
                Path::new(root),
                "wing {}: project {} came back with a rewritten root path",
                wing.id,
                project.id
            );
            assert_eq!(
                project.db_path,
                Path::new(db),
                "wing {}: project {} came back with a rewritten database path",
                wing.id,
                project.id
            );
            assert!(
                project.registered_at > 0,
                "wing {}: project {} lost its registration timestamp",
                wing.id,
                project.id
            );
        }

        let mut total_deps = 0i64;
        for project in &projects {
            for dep in registry.get_deps(project.id).expect("reading deps") {
                total_deps += 1;
                assert!(
                    projects.iter().any(|p| p.id == dep.id),
                    "wing {}: project {} depends on id {}, which is not a \
                     registered project; the link outlived its target",
                    wing.id,
                    project.id,
                    dep.id
                );
                assert_ne!(
                    dep.id, project.id,
                    "wing {}: project {} now depends on itself",
                    wing.id, project.id
                );
            }
        }
        assert_eq!(
            total_deps, wing.expect.dep_count,
            "wing {}: dependency links changed across the upgrade",
            wing.id
        );

        unsafe { std::env::remove_var("SPELUNK_REGISTRY_DIR") };
    }
}

// Criterion 2 (git-notes half): the ref carries blobs from three writing eras
// (legacy single-JSON, multi-line JSONL without entity_id, entity-keyed event
// log) and a current read must surface every one of them.

#[tokio::test]
#[serial_test::serial]
async fn git_notes_reads_every_era_on_the_ref() {
    let m = manifest();
    let notes_wings = wings_of_kind(&m, "git-notes");
    assert!(!notes_wings.is_empty(), "corpus has no git-notes wing");

    for wing in notes_wings {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = checkout(wing, tmp.path());
        let repo = tmp.path().join("repo");
        unbundle(&bundle, &repo);

        let backend = GitNotesBackend::with_root(repo.clone());
        let notes = backend
            .list(None, 500, true, None)
            .await
            .unwrap_or_else(|e| panic!("reading wing {} with the current build: {e}", wing.id));

        let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();
        for expected in &wing.expect.era_entries {
            let got = notes
                .iter()
                .find(|n| n.title == expected.title)
                .unwrap_or_else(|| {
                    panic!(
                        "wing {}: era entry {:?} is on the ref but the current \
                         reader missed it; saw {:?}",
                        wing.id, expected.title, titles
                    )
                });
            assert_eq!(
                got.kind, expected.kind,
                "wing {}: entry {:?} came back under the wrong kind",
                wing.id, expected.title
            );
            assert_eq!(
                got.body, expected.body,
                "wing {}: entry {:?} came back with the wrong body",
                wing.id, expected.title
            );
        }

        // Exactly the recorded entries, no more. The 0.9.3 era binary really
        // does write each of its entries twice into the log, so a reader that
        // stopped folding duplicates would hand the user the same decision
        // several times over and every title assertion above would still pass.
        assert_eq!(
            notes.len(),
            wing.expect.era_entries.len(),
            "wing {}: the reader returned {} entries for {} distinct records on \
             the ref; saw {:?}",
            wing.id,
            notes.len(),
            wing.expect.era_entries.len(),
            titles
        );
    }
}

// Criterion 3: a FLOAT[768] index is rebuilt empty as INT8[896] so a re-embed is
// required, rather than left holding vectors that cannot be compared with the
// ones the current embedder produces.

#[test]
#[serial_test::serial]
fn a_float768_wing_is_rebuilt_empty_as_int8_rather_than_serving_mixed_vectors() {
    register_sqlite_vec();
    let m = manifest();
    let float_wings: Vec<&Wing> = m
        .wings
        .iter()
        .filter(|w| w.kind == "index" && w.expect.vector_storage == "float768")
        .collect();
    assert!(
        !float_wings.is_empty(),
        "corpus has no 768-dimension index wing, so the dimension-upgrade path is untested"
    );

    for wing in float_wings {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = checkout(wing, tmp.path());

        // The captured artifact really is a 768-dimension index before the
        // current build touches it; without this the assertions below could
        // pass on an already-upgraded fixture.
        {
            let raw = rusqlite::Connection::open(&db_path).expect("opening the raw fixture");
            assert!(
                vector_column_type(&raw, "embeddings").contains("FLOAT[768]"),
                "wing {} was captured as a 768-dimension index but is not one",
                wing.id
            );
            assert!(
                wing.expect.embedding_count > 0,
                "wing {} has no stored vectors, so it cannot show that they are discarded",
                wing.id
            );
        }

        let db = Database::open(&db_path)
            .unwrap_or_else(|e| panic!("opening wing {} with the current build: {e}", wing.id));
        let upgraded = raw(&db_path);

        assert!(
            vector_column_type(&upgraded, "embeddings").contains("INT8[896]"),
            "wing {}: the 768-dimension vector table was not rebuilt as int8[896]",
            wing.id
        );
        assert_eq!(
            row_count(&upgraded, "embeddings_rowids"),
            0,
            "wing {}: stale 768-dimension vectors survived the rebuild and would be \
             ranked against 896-dimension query vectors",
            wing.id
        );
        assert_eq!(
            db.stats().expect("reading index stats").chunk_count,
            wing.expect.chunk_count,
            "wing {}: the dimension upgrade discarded chunks, not just vectors; \
             a re-embed would have nothing to rebuild from",
            wing.id
        );
    }
}

// Criterion 5: the second open is a no-op. Byte equality is the strongest
// statement of that and catches a migration that rewrites rows every time.

#[test]
#[serial_test::serial]
fn upgrading_a_wing_twice_changes_nothing_the_second_time() {
    register_sqlite_vec();
    let m = manifest();

    for wing in m.wings.iter().filter(|w| w.kind != "git-notes") {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = checkout(wing, tmp.path());

        open_for_kind(&wing.kind, &db_path, tmp.path());
        let after_first = content_image(&db_path);

        open_for_kind(&wing.kind, &db_path, tmp.path());
        let after_second = content_image(&db_path);

        assert_eq!(
            after_first.len(),
            after_second.len(),
            "wing {}: a second open changed the database size",
            wing.id
        );
        assert!(
            after_first == after_second,
            "wing {}: a second open rewrote the database; the upgrade is not idempotent",
            wing.id
        );
    }
}

fn open_for_kind(kind: &str, db_path: &Path, dir: &Path) {
    match kind {
        "index" => {
            Database::open(db_path).expect("opening index wing");
        }
        "memory" => {
            MemoryStore::open(db_path).expect("opening memory wing");
        }
        "registry" => {
            unsafe { std::env::set_var("SPELUNK_REGISTRY_DIR", dir) };
            Registry::open().expect("opening registry wing");
            unsafe { std::env::remove_var("SPELUNK_REGISTRY_DIR") };
        }
        other => panic!("no opener wired up for wing kind {other:?}"),
    }
}

// Criterion 4: a pinned old release opening a database the current build has
// already upgraded. The behaviour has to be defined and asserted rather than
// assumed, because a user who upgrades, then runs an older binary still on
// their PATH, hits exactly this.
//
// Measured behaviour, against v0.9.2, v0.9.3 and v0.9.5: a clean read, never a
// refusal. The old binary exits 0, reports the correct file/chunk/embedding
// counts, lists the memory entries, and returns full-text search hits, and no
// row is lost.
//
// One wrinkle the corpus surfaced, which is why the version is asserted
// separately from the data: a release whose own schema version is *below* the
// current one re-stamps `PRAGMA user_version` down to its own on close. v0.9.3
// rewinds an index.db from 15 to 14. v0.9.2 pre-dates the header entirely and
// never stamps; v0.9.5 stamps the same value it finds. The rewind loses no
// data, and the next current-build open heals it: the steps above the rewound
// version are individually idempotent, so they re-run as no-ops and re-stamp
// the current version. That heal is asserted here rather than assumed, because
// it is the only thing standing between a rewind and a re-run of migrations
// against a schema that already has them.
//
// Ignored by default because it needs a downloaded release. CI runs it with
// SPELUNK_OLD_BINARY pointing at one; run it locally the same way.

fn old_binary() -> PathBuf {
    let raw = std::env::var("SPELUNK_OLD_BINARY").expect(
        "SPELUNK_OLD_BINARY must point at a pinned released spelunk binary; \
         scripts/upgrade-corpus/generate.sh downloads one into its cache",
    );
    let path = PathBuf::from(raw);
    assert!(
        path.is_file(),
        "SPELUNK_OLD_BINARY does not exist: {}",
        path.display()
    );
    path
}

// A project directory holding both databases from the corpus, already upgraded
// to the current schema by the current build.
fn upgraded_project(tmp: &Path) -> PathBuf {
    let m = manifest();
    let project = tmp.join("project");
    let dot = project.join(".spelunk");
    std::fs::create_dir_all(&dot).unwrap();

    let index_wing = wings_of_kind(&m, "index")
        .into_iter()
        .find(|w| w.expect.vector_storage != "float768")
        .expect("corpus has no int8 index wing to upgrade");
    let memory_wing = wings_of_kind(&m, "memory")
        .into_iter()
        .next()
        .expect("corpus has no memory wing to upgrade");

    let staged_index = checkout(index_wing, tmp);
    std::fs::rename(&staged_index, dot.join("index.db")).unwrap();
    let staged_memory = checkout(memory_wing, tmp);
    std::fs::rename(&staged_memory, dot.join("memory.db")).unwrap();

    // A git repo, because the CLI resolves a project from one.
    for args in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "corpus@spelunk.invalid"],
        vec!["config", "user.name", "corpus"],
    ] {
        let ok = git_command(&project)
            .args(&args)
            .status()
            .expect("running git")
            .success();
        assert!(ok, "git {args:?} failed");
    }
    std::fs::write(project.join("seed.txt"), "corpus\n").unwrap();
    for args in [vec!["add", "-A"], vec!["commit", "--quiet", "-m", "seed"]] {
        let ok = git_command(&project)
            .args(&args)
            .status()
            .expect("running git")
            .success();
        assert!(ok, "git {args:?} failed");
    }

    register_sqlite_vec();
    Database::open(&dot.join("index.db")).expect("upgrading the index wing to current");
    MemoryStore::open(&dot.join("memory.db")).expect("upgrading the memory wing to current");
    project
}

fn table_counts(dot: &Path) -> (i32, i64, i64, i32, i64) {
    let index = raw(&dot.join("index.db"));
    let memory = raw(&dot.join("memory.db"));
    (
        read_user_version(&index),
        index
            .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))
            .unwrap(),
        row_count(&index, "embeddings_rowids"),
        read_user_version(&memory),
        memory
            .query_row("SELECT count(*) FROM notes", [], |r| r.get(0))
            .unwrap(),
    )
}

#[test]
#[ignore = "needs a downloaded release binary in SPELUNK_OLD_BINARY"]
#[serial_test::serial]
fn a_pinned_old_binary_reads_a_current_database_cleanly_and_loses_no_data() {
    let bin = old_binary();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".config").join("spelunk")).unwrap();
    std::fs::write(home.join(".config").join("spelunk").join("config.toml"), "").unwrap();
    let project = upgraded_project(tmp.path());
    let dot = project.join(".spelunk");

    let before = table_counts(&dot);

    let run = |args: &[&str]| -> std::process::Output {
        std::process::Command::new(&bin)
            .current_dir(&project)
            .args(args)
            // An old binary predates the file secret-store default and would
            // otherwise reach the OS keychain and block on a prompt.
            .env("SPELUNK_SECRET_STORE", "file")
            .env("HOME", &home)
            .env("SPELUNK_CONFIG_DIR", home.join(".config").join("spelunk"))
            .env("SPELUNK_REGISTRY_DIR", home.join(".config").join("spelunk"))
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .unwrap_or_else(|e| panic!("running the old binary with {args:?}: {e}"))
    };

    let status = run(&["status"]);
    assert!(
        status.status.success(),
        "the old binary refused a current index.db: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status_out = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_out.contains(&format!("Chunks:     {}", before.1)),
        "the old binary did not report the current index's chunk count; got:\n{status_out}"
    );

    let listing = run(&["memory", "list"]);
    assert!(
        listing.status.success(),
        "the old binary refused a current memory.db: {}",
        String::from_utf8_lossy(&listing.stderr)
    );
    assert!(
        String::from_utf8_lossy(&listing.stdout)
            .contains("Index must stay usable without a network"),
        "the old binary read a current memory.db but not its entries"
    );

    let search = run(&["search", "parse_manifest", "--mode", "text"]);
    assert!(
        search.status.success(),
        "the old binary failed full-text search on a current index.db: {}",
        String::from_utf8_lossy(&search.stderr)
    );
    assert!(
        String::from_utf8_lossy(&search.stdout).contains("parse_manifest"),
        "the old binary's full-text search lost pre-existing content"
    );

    let after = table_counts(&dot);
    assert_eq!(
        (after.1, after.2, after.4),
        (before.1, before.2, before.4),
        "the old binary lost rows from a database the current build had already \
         upgraded; a read must never cost data"
    );
    assert!(
        after.0 <= before.0 && after.3 <= before.3,
        "the old binary stamped a schema version above the current build's \
         ({after:?} vs {before:?}); a newer build would then skip migrations it \
         has never actually run"
    );

    // The heal. Re-opening with the current build must restore the current
    // version without disturbing anything the old binary left behind.
    Database::open(&dot.join("index.db")).expect("re-opening the index after the old binary");
    MemoryStore::open(&dot.join("memory.db")).expect("re-opening memory after the old binary");
    assert_eq!(
        table_counts(&dot),
        before,
        "re-opening with the current build did not restore the state it had \
         before the old binary ran"
    );
}
