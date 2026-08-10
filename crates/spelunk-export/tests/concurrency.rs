// A store that is being written while it is read must still export as one
// coherent thing.
//
// Every table an export reads is read separately, so without a snapshot the
// entries come from one instant and the links from a later one. A writer that
// commits an entry and then a link referencing it, in two transactions, lands
// between those reads and produces a link whose endpoint appears not to exist.
// That link is indistinguishable from one whose endpoint really is gone, so it
// gets dropped and reported, and the export still succeeds. The data is real,
// the loss is not detectable afterwards, and the dump certifies itself.
//
// These tests pin the interleave exactly rather than racing for it.

mod support;

use std::sync::mpsc;
use std::time::Duration;

use rusqlite::Connection;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use spelunk_export::{export, source};
use support::{LATEST, add_entry, memory_store_at, wal_memory_store_at};

const WAIT: Duration = Duration::from_secs(30);

// Commit an entry, then a link referencing it, as two separate transactions.
// This is what any ordinary write of a superseding entry looks like from
// outside: the product writes the entry, then the edge.
fn commit_entry_then_link(path: &std::path::Path) -> rusqlite::Result<i64> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(WAIT)?;
    conn.execute(
        "INSERT INTO notes (kind, title, body, created_at) VALUES ('decision', 'late', 'b', 9000)",
        [],
    )?;
    let id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO memory_edges (from_id, to_id, kind, created_at) VALUES (?1, 1, 'supersedes', 9000)",
        rusqlite::params![id],
    )?;
    Ok(id)
}

// Drive `extract` to the point where it is about to read the link table, let a
// writer commit both of its transactions there, and then let the read continue.
//
// The authorizer fires while the link query is being prepared, which is after
// the entry query has been fully consumed. That is precisely the window the
// concurrent writer has to hit by luck, so pinning it here turns an occasional
// failure into a deterministic one.
fn extract_with_a_write_between_the_tables(
    store: &std::path::Path,
) -> (spelunk_export::source::Extract, i64) {
    let (want_tx, want_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<rusqlite::Result<i64>>();

    let path = store.to_path_buf();
    let writer = std::thread::spawn(move || {
        if want_rx.recv().is_err() {
            return;
        }
        let _ = done_tx.send(commit_entry_then_link(&path));
    });

    let conn = source::open_read_only(store).unwrap();
    let mut fired = false;
    conn.authorizer(Some(move |ctx: AuthContext<'_>| {
        if let AuthAction::Read { table_name, .. } = ctx.action
            && table_name == "memory_edges"
            && !fired
        {
            fired = true;
            want_tx
                .send(())
                .expect("the writer thread should be waiting");
            done_rx
                .recv_timeout(WAIT)
                .expect("the writer thread should finish")
                .expect("the concurrent write should succeed");
        }
        Authorization::Allow
    }))
    .unwrap();

    let extracted = source::extract(&conn).unwrap();
    // Dropping the connection drops the authorizer and with it the sender the
    // writer is waiting on, so a run where the interleave never fired ends in a
    // failed assertion rather than a hang.
    drop(conn);
    writer.join().unwrap();

    let after = Connection::open(store).unwrap();
    let written: i64 = after
        .query_row("SELECT count(*) FROM memory_edges", [], |r| r.get(0))
        .unwrap();
    (extracted, written)
}

#[test]
fn a_write_landing_between_two_tables_cannot_manufacture_a_missing_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("memory.db");
    {
        let conn = wal_memory_store_at(&store, LATEST);
        add_entry(&conn, LATEST, "first", 1_000);
        add_entry(&conn, LATEST, "second", 2_000);
    }

    let (extracted, links_in_store) = extract_with_a_write_between_the_tables(&store);
    assert_eq!(links_in_store, 1, "the concurrent write must have landed");

    assert!(
        extracted.warnings.is_empty(),
        "a committed write is not a broken store, so nothing may be reported as \
         missing: {:?}",
        extracted.warnings
    );

    let present: std::collections::HashSet<&str> = extracted
        .dump
        .entries
        .iter()
        .map(|e| e.reference.as_str())
        .collect();
    for link in &extracted.dump.relationships {
        assert!(
            present.contains(link.from.as_str()) && present.contains(link.to.as_str()),
            "link {} -> {} names an entry the same read did not return",
            link.from,
            link.to
        );
    }

    let carried_the_entry = extracted.dump.entries.iter().any(|e| e.title == "late");
    let carried_the_link = !extracted.dump.relationships.is_empty();
    assert_eq!(
        carried_the_entry, carried_the_link,
        "the entry and the link it is an endpoint of were written together and \
         must be read together"
    );
}

// Holding the read open across every table means a writer on an older
// rollback-journal store can now lock the export out, where before each
// statement took its own momentary lock. A store held that way is refused with
// its own name and leaves nothing behind, rather than producing a dump of
// whatever it managed to read.
#[test]
fn a_store_locked_by_another_writer_is_refused_by_name_and_leaves_no_partial_dump() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    let holder = memory_store_at(&store, LATEST);
    add_entry(&holder, LATEST, "only", 1_000);
    holder.execute_batch("BEGIN EXCLUSIVE").unwrap();

    let err = export(&store, &out, 1_700_000_000).unwrap_err();

    assert!(
        format!("{err:#}").contains("memory.db"),
        "the refusal must name the store it could not read: {err:#}"
    );
    assert!(!out.exists());
    assert!(!out.with_extension("partial").exists());
    holder.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn a_dump_written_under_a_concurrent_writer_verifies_and_is_whole() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("memory.db");
    let out = dir.path().join("dump.jsonl");
    {
        let conn = wal_memory_store_at(&store, LATEST);
        for n in 0..200 {
            add_entry(&conn, LATEST, &format!("entry {n}"), 1_000 + n);
        }
    }

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let path = store.clone();
    let flag = stop.clone();
    let writer = std::thread::spawn(move || {
        let mut written = 0;
        while !flag.load(std::sync::atomic::Ordering::Relaxed) {
            if commit_entry_then_link(&path).is_ok() {
                written += 1;
            }
        }
        written
    });

    let outcome = export(&store, &out, 1_700_000_000).unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let written = writer.join().unwrap();
    assert!(written > 0, "the writer must have committed something");

    assert!(
        outcome.warnings.is_empty(),
        "nothing in the store was ever broken: {:?}",
        outcome.warnings
    );
    spelunk_export::dump::verify_rendered(&std::fs::read_to_string(&out).unwrap()).unwrap();
}
