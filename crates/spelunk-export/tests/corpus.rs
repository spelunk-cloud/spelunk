// Export from artifacts written by actual released binaries.
//
// The ladder fixtures next door cover what the schema *is* at each version.
// They cannot cover what a released binary put in a file, and they are a hand
// copy of a ladder that lives somewhere else: a copy drifts and no test
// notices. These artifacts were captured by running downloaded releases, and
// the expectations they are checked against were read out of them with plain
// SQL at capture time, before any current-build code opened them. That makes
// this the only evidence here that is independent of the branch it is testing.
//
// The corpus is shared with the CLI's own upgrade suite and is reached by path.
// This crate still depends on no other crate in the workspace.
//
// Regenerate with scripts/upgrade-corpus/generate.sh.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use spelunk_export::{ExportOutcome, dump, export};

#[derive(Deserialize)]
struct Manifest {
    wings: Vec<Wing>,
}

#[derive(Deserialize)]
struct Wing {
    id: String,
    producer: String,
    kind: String,
    artifact: String,
    sha256: String,
    #[serde(default)]
    expect: Expect,
}

// Only the fields this suite reads. The CLI's upgrade suite reads the rest.
#[derive(Default, Deserialize)]
struct Expect {
    #[serde(default)]
    note_count: usize,
    #[serde(default)]
    active_note_count: usize,
    #[serde(default)]
    archived_title: String,
    #[serde(default)]
    superseded_title: String,
    #[serde(default)]
    successor_title: String,
    #[serde(default)]
    entity_id_present: bool,
    #[serde(default)]
    project_count: usize,
    #[serde(default)]
    dep_count: usize,
}

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("spelunk-cli")
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

// Expand a wing into a temp dir, after confirming the packed bytes are the ones
// the expectations were read from. An artifact swapped without recapturing
// would leave every assertion below checking one release's output against
// another's.
fn checkout(wing: &Wing, tmp: &Path) -> PathBuf {
    let src = corpus_root()
        .join("wings")
        .join(&wing.id)
        .join(&wing.artifact);
    let packed = std::fs::read(&src)
        .unwrap_or_else(|e| panic!("reading wing {} at {}: {e}", wing.id, src.display()));
    let hex: String = Sha256::digest(&packed)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        hex, wing.sha256,
        "wing {} no longer matches the artifact its expectations were captured from",
        wing.id
    );

    let dst = tmp.join(wing.artifact.trim_end_matches(".gz"));
    let mut reader = flate2::read::GzDecoder::new(std::io::Cursor::new(packed));
    let mut out =
        std::fs::File::create(&dst).unwrap_or_else(|e| panic!("creating {}: {e}", dst.display()));
    std::io::copy(&mut reader, &mut out)
        .unwrap_or_else(|e| panic!("expanding wing {}: {e}", wing.id));
    dst
}

fn wings_of_kind(m: &Manifest, kind: &str) -> Vec<usize> {
    (0..m.wings.len())
        .filter(|i| m.wings[*i].kind == kind)
        .collect()
}

struct Exported {
    outcome: ExportOutcome,
    records: Vec<Value>,
    text: String,
}

impl Exported {
    fn entities(&self, kind: &str) -> Vec<&Value> {
        self.records
            .iter()
            .filter(|r| r["record"] == "entity" && r["type"] == kind)
            .collect()
    }

    fn relationships(&self) -> Vec<&Value> {
        self.records
            .iter()
            .filter(|r| r["record"] == "relationship")
            .collect()
    }

    fn by_ref(&self) -> BTreeMap<&str, &Value> {
        self.records
            .iter()
            .filter(|r| r["record"] == "entity")
            .map(|r| (r["ref"].as_str().unwrap(), r))
            .collect()
    }
}

// Export a wing and assert the source came out of it untouched. Every wing goes
// through here, so the read-only property is asserted once per real artifact
// rather than once for the suite.
fn export_wing(wing: &Wing, dir: &Path) -> Exported {
    let store = checkout(wing, dir);
    let out = dir.join("dump.jsonl");
    let before = std::fs::read(&store).unwrap();

    let outcome = export(&store, &out, 1_700_000_000).unwrap_or_else(|e| {
        panic!(
            "exporting wing {} (written by {}): {e:#}",
            wing.id, wing.producer
        )
    });

    assert!(
        std::fs::read(&store).unwrap() == before,
        "wing {}: the artifact was modified by exporting it",
        wing.id
    );
    let sidecars = store.file_name().unwrap().to_str().unwrap().to_string();
    assert!(
        !dir.join(format!("{sidecars}-journal")).exists(),
        "wing {}: a rollback journal means the artifact was opened for writing",
        wing.id
    );
    let log = dir.join(format!("{sidecars}-wal"));
    if log.exists() {
        assert_eq!(
            std::fs::metadata(&log).unwrap().len(),
            0,
            "wing {}: a read must not append anything to the log",
            wing.id
        );
    }

    let text = std::fs::read_to_string(&out).unwrap();
    dump::verify_rendered(&text).expect("the dump written from a real artifact must verify");
    let records = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    Exported {
        outcome,
        records,
        text,
    }
}

#[test]
fn the_corpus_is_present_and_covers_the_kinds_this_suite_reads() {
    let m = manifest();
    for kind in ["memory", "registry", "index"] {
        assert!(
            !wings_of_kind(&m, kind).is_empty(),
            "the corpus has no {kind} wing, so the assertions over it pass vacuously"
        );
    }
    assert!(
        wings_of_kind(&m, "memory")
            .iter()
            .any(|i| !m.wings[*i].expect.entity_id_present),
        "every memory wing was captured after entity_id existed, so the \
         omit-an-absent-column path is never exercised on a real artifact"
    );
}

// The manifest's counts, statuses and supersede pair, read back out of a dump
// of the artifact they were recorded from.
#[test]
fn every_memory_wing_a_released_binary_wrote_exports_whole() {
    let m = manifest();
    for i in wings_of_kind(&m, "memory") {
        let wing = &m.wings[i];
        let dir = tempfile::tempdir().unwrap();
        let e = export_wing(wing, dir.path());

        assert_eq!(
            e.outcome.counts.entity.get("memory_entry"),
            Some(&wing.expect.note_count),
            "wing {}: entries were lost or invented",
            wing.id
        );
        assert!(
            e.outcome.warnings.is_empty(),
            "wing {}: nothing in a captured artifact is broken, so nothing may be \
             reported as dropped: {:?}",
            wing.id,
            e.outcome.warnings
        );

        let entries = e.entities("memory_entry");
        let active = entries.iter().filter(|n| n["status"] != "archived").count();
        assert_eq!(
            active, wing.expect.active_note_count,
            "wing {}: the archived entries did not come out archived",
            wing.id
        );
        let archived = entries
            .iter()
            .find(|n| n["title"] == wing.expect.archived_title.as_str())
            .unwrap_or_else(|| panic!("wing {}: the archived entry is not in the dump", wing.id));
        assert_eq!(archived["status"], "archived");

        // An identifier the artifact does not carry must be absent from the
        // dump rather than minted, and this is the one place that is checked
        // against a column a real release genuinely did not have.
        for entry in &entries {
            assert_eq!(
                entry.get("entity_id").is_some(),
                wing.expect.entity_id_present,
                "wing {}: entity_id presence does not match what the capturing \
                 release wrote",
                wing.id
            );
            assert!(
                entry.get("uuid").is_none(),
                "wing {}: these artifacts carry no uuid, so one in the dump was \
                 minted here",
                wing.id
            );
        }

        // The orientation trap, on a store a real binary wrote: both artifacts
        // hold the edge and the lifecycle column at once, pointing opposite
        // ways, and the manifest names which entry is the successor.
        let links = e.relationships();
        assert_eq!(
            links.len(),
            1,
            "wing {}: the edge and the lifecycle column encode one fact and must \
             not become two links: {links:?}",
            wing.id
        );
        assert_eq!(links[0]["type"], "supersedes");
        let by_ref = e.by_ref();
        assert_eq!(
            by_ref[links[0]["from"].as_str().unwrap()]["title"],
            wing.expect.successor_title.as_str(),
            "wing {}: the supersede link points away from the successor",
            wing.id
        );
        assert_eq!(
            by_ref[links[0]["to"].as_str().unwrap()]["title"],
            wing.expect.superseded_title.as_str(),
            "wing {}: the supersede link does not land on the superseded entry",
            wing.id
        );

        for derived in ["memory_fts", "note_embeddings", "schema_v896"] {
            assert!(
                !e.text.contains(derived),
                "wing {}: {derived} is derived and must not be carried",
                wing.id
            );
        }
    }
}

#[test]
fn every_registry_wing_a_released_binary_wrote_exports_whole() {
    let m = manifest();
    for i in wings_of_kind(&m, "registry") {
        let wing = &m.wings[i];
        let dir = tempfile::tempdir().unwrap();
        let e = export_wing(wing, dir.path());

        assert_eq!(
            e.outcome.counts.entity.get("project"),
            Some(&wing.expect.project_count),
            "wing {}: registered projects were lost",
            wing.id
        );
        assert_eq!(
            e.outcome.counts.relationship.get("depends_on"),
            Some(&wing.expect.dep_count),
            "wing {}: dependency links were lost",
            wing.id
        );

        let by_ref = e.by_ref();
        for link in e.relationships() {
            for end in ["from", "to"] {
                let target = link[end].as_str().unwrap();
                assert!(
                    by_ref[target]["root_path"].is_string(),
                    "wing {}: a dependency names {target}, which is not a project \
                     in the dump",
                    wing.id
                );
            }
        }
        assert!(
            !e.text.contains("index.db"),
            "wing {}: the registry's derived store path is in the dump, and it is \
             wrong for any reader laying its stores out differently",
            wing.id
        );
    }
}

// An index store holds one authored table, `usage`, and these artifacts have no
// rows in it. Everything else in them is a reindex away, so the whole file must
// come out as an empty dump rather than as chunks and vectors.
#[test]
fn an_index_wing_a_released_binary_wrote_carries_nothing_derived() {
    let m = manifest();
    for i in wings_of_kind(&m, "index") {
        let wing = &m.wings[i];
        let dir = tempfile::tempdir().unwrap();
        let e = export_wing(wing, dir.path());

        assert!(
            e.outcome.counts.entity.is_empty() && e.outcome.counts.relationship.is_empty(),
            "wing {}: an index store holds only usage counts, and these have \
             none: {:?}",
            wing.id,
            e.outcome.counts
        );
        assert_eq!(
            e.records.len(),
            2,
            "wing {}: an empty dump is a header and a footer",
            wing.id
        );
        assert!(
            e.outcome
                .summary(Path::new("dump.jsonl"))
                .contains("nothing to carry"),
            "wing {}: the run must say the store held nothing rather than imply \
             a full export",
            wing.id
        );
    }
}
