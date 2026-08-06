// The dump is proved before it is published, and nothing is left behind when
// the proof fails.
//
// These exercise the write-verify-rename path itself, which is reachable only
// once extraction has succeeded. A test that fails earlier, inside the read,
// never reaches the temporary file and so establishes nothing about it.

mod support;

use std::path::Path;

use spelunk_export::{ExportOutcome, dump, export, source, write_verified};
use support::{LATEST, add_entry, memory_store_at};

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

// Render what an export of a store holding `titles` would write.
fn rendered(dir: &Path, name: &str, titles: &[&str]) -> (String, Vec<String>) {
    let store = dir.join(name);
    {
        let conn = memory_store_at(&store, LATEST);
        for (n, t) in titles.iter().enumerate() {
            add_entry(&conn, LATEST, t, 1_000 + n as i64);
        }
    }
    let conn = source::open_read_only(&store).unwrap();
    let extracted = source::extract(&conn).unwrap();
    let (text, per_record, _) = extracted.dump.render(1_700_000_000).unwrap();
    (text, per_record)
}

#[test]
fn a_verified_dump_is_moved_into_place_and_no_temporary_survives() {
    let dir = tmp();
    let out = dir.path().join("dump.jsonl");
    let (text, per_record) = rendered(dir.path(), "memory.db", &["a", "b"]);

    write_verified(&text, &per_record, &out).unwrap();

    assert_eq!(std::fs::read_to_string(&out).unwrap(), text);
    assert!(
        !out.with_extension("partial").exists(),
        "the temporary must not survive a successful run"
    );
}

#[test]
fn a_file_that_reads_back_as_a_different_dump_is_refused_and_removed() {
    let dir = tmp();
    let out = dir.path().join("dump.jsonl");
    let (text, _) = rendered(dir.path(), "one.db", &["a", "b"]);
    let (_, other_records) = rendered(dir.path(), "two.db", &["c"]);

    // Both are whole, self-consistent dumps, so the footer's own digest check
    // passes on the file. Only the comparison against what this run read from
    // the store can tell them apart, which is the comparison being pinned here.
    let err = write_verified(&text, &other_records, &out).unwrap_err();

    assert!(
        err.to_string().contains("does not match"),
        "unexpected refusal: {err}"
    );
    assert!(!out.exists(), "an unproved dump must not be published");
    assert!(
        !out.with_extension("partial").exists(),
        "the unproved file must not be left behind either"
    );
}

#[test]
fn a_file_that_reads_back_truncated_is_refused_and_removed() {
    let dir = tmp();
    let out = dir.path().join("dump.jsonl");
    let (text, per_record) = rendered(dir.path(), "memory.db", &["a", "b"]);
    let short: String = text
        .lines()
        .take(text.lines().count() - 1)
        .map(|l| format!("{l}\n"))
        .collect();

    let err = write_verified(&short, &per_record, &out).unwrap_err();

    assert!(err.to_string().contains("truncated"), "unexpected: {err}");
    assert!(!out.exists(), "a truncated dump must not be published");
    assert!(!out.with_extension("partial").exists());
}

// A whole-binary version of the two above: the store reads fine, the write
// succeeds, and the bytes that land are still not the bytes that were written.
// A sink swallows the write and reads back empty, which is what a full disk or
// a misdirected path does to a dump.
#[cfg(unix)]
#[test]
fn an_export_whose_bytes_do_not_land_publishes_nothing_and_leaves_nothing() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = memory_store_at(&store, LATEST);
        add_entry(&conn, LATEST, "a", 1_000);
    }
    let temp = out.with_extension("partial");
    std::os::unix::fs::symlink("/dev/null", &temp).unwrap();

    assert!(export(&store, &out, 1_700_000_000).is_err());
    assert!(!out.exists(), "an unproved dump must not be published");
    assert!(
        !temp.exists() && std::fs::symlink_metadata(&temp).is_err(),
        "the unproved file must not be left behind"
    );
}

#[test]
fn a_clean_run_claims_only_what_it_proved() {
    let dir = tmp();
    let out = dir.path().join("dump.jsonl");
    let store = dir.path().join("memory.db");
    {
        let conn = memory_store_at(&store, LATEST);
        add_entry(&conn, LATEST, "a", 1_000);
    }
    let summary = export(&store, &out, 1_700_000_000).unwrap().summary(&out);

    assert!(summary.contains("1 memory_entry"), "got: {summary}");
    assert!(summary.contains("reads back"), "got: {summary}");
    assert!(
        !summary.to_lowercase().contains("not carried"),
        "nothing was dropped, so nothing may be reported as dropped: {summary}"
    );
}

#[test]
fn a_run_that_dropped_a_link_says_so_where_it_reports_success() {
    let outcome = ExportOutcome {
        counts: {
            let mut c = dump::Counts::default();
            c.add_entity("memory_entry");
            c
        },
        warnings: vec!["a 'relates_to' link ... is not carried".into()],
    };

    let summary = outcome.summary(Path::new("/tmp/dump.jsonl"));

    assert!(
        summary.contains("1 link(s) were NOT carried"),
        "the count of what was dropped belongs beside the count of what was \
         carried: {summary}"
    );
}
