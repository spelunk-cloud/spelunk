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

// The temporary's path is derived from the output's, so it can collide with a
// file that was already there. Overwriting it and then unlinking it on failure
// destroys a user's file, silently, in the one tool whose contract is that it
// removes nothing. Creating the temporary exclusively turns the collision into
// a refusal that names the path, and the file is still there afterwards.
#[test]
fn a_file_already_at_the_temporary_path_stops_the_run_and_survives_it() {
    let dir = tmp();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = memory_store_at(&store, LATEST);
        add_entry(&conn, LATEST, "a", 1_000);
    }
    let temp = out.with_extension("partial");
    std::fs::write(&temp, b"something the user put here").unwrap();

    let err = format!("{:#}", export(&store, &out, 1_700_000_000).unwrap_err());

    assert!(err.contains("already exists"), "got: {err}");
    assert!(
        err.contains("dump.partial"),
        "the refusal must name it: {err}"
    );
    assert_eq!(
        std::fs::read(&temp).unwrap(),
        b"something the user put here",
        "the file was destroyed by an export that did not create it"
    );
    assert!(!out.exists(), "an unproved dump must not be published");
}

// There used to be a whole-binary version of the two above, which put a
// symlink to /dev/null at the temporary's path so the write succeeded and the
// readback came up empty. Exclusive creation refuses anything already sitting
// there, symlink included, so that route is gone and with it the class of
// failure it stood for: reaching a sink through this path now needs a file the
// export declined to open. What the readback still guards against, bytes that
// land differently from how they were written, is exercised directly on
// `write_verified` above, where it can be provoked without a filesystem trick.

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
