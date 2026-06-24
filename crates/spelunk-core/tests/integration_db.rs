//! Integration tests for `spelunk_core::storage::Database`.
//!
//! These tests open real (in-memory) SQLite databases with the sqlite-vec
//! extension loaded.  They must run serially because sqlite3_auto_extension
//! is process-global.

mod common;

use serial_test::serial;
use spelunk_core::indexer::graph::{Edge, EdgeKind};

// ── helpers ──────────────────────────────────────────────────────────────────

fn zero_vec(dim: usize) -> Vec<f32> {
    vec![0.0; dim]
}

fn unit_vec(dim: usize, pos: usize) -> Vec<f32> {
    let mut v = zero_vec(dim);
    v[pos] = 1.0;
    v
}

// ── files ────────────────────────────────────────────────────────────────────

#[test]
#[serial]
fn upsert_file_returns_stable_id() {
    let db = common::open_test_db();
    let id1 = db.upsert_file("src/lib.rs", Some("rust"), "hash1").unwrap();
    let id2 = db.upsert_file("src/lib.rs", Some("rust"), "hash2").unwrap();
    assert_eq!(id1, id2, "upsert must return the same row id");
}

#[test]
#[serial]
fn file_hash_round_trips() {
    let db = common::open_test_db();
    db.upsert_file("src/main.rs", Some("rust"), "abc123")
        .unwrap();
    let hash = db.file_hash("src/main.rs").unwrap();
    assert_eq!(hash.as_deref(), Some("abc123"));
}

#[test]
#[serial]
fn file_hash_returns_none_for_unknown() {
    let db = common::open_test_db();
    assert!(db.file_hash("does/not/exist.rs").unwrap().is_none());
}

// ── chunks ───────────────────────────────────────────────────────────────────

#[test]
#[serial]
fn insert_and_delete_chunks() {
    let db = common::open_test_db();
    let file_id = db.upsert_file("src/foo.rs", Some("rust"), "h").unwrap();
    let chunk_id = db
        .insert_chunk(
            file_id,
            "function",
            Some("foo"),
            1,
            10,
            "fn foo() {}",
            None,
            3,
        )
        .unwrap();
    assert!(chunk_id > 0);

    db.delete_chunks_for_file(file_id).unwrap();
    // After deletion, chunks_by_ids should return empty.
    let results = db.chunks_by_ids(&[chunk_id]).unwrap();
    assert!(results.is_empty());
}

// ── embeddings + KNN search ──────────────────────────────────────────────────

// Must match the dimension in migrations/002_vectors.sql (F2LLM-v2-330M, 896-dim).
const DIM: usize = spelunk_core::embeddings::EMBEDDING_DIM;

#[test]
#[serial]
fn knn_returns_closest_vector_first() {
    let db = common::open_test_db();

    // Insert two chunks with distinct embeddings.
    let fid = db.upsert_file("a.rs", Some("rust"), "h").unwrap();
    let cid1 = db
        .insert_chunk(
            fid,
            "function",
            Some("alpha"),
            1,
            5,
            "fn alpha() {}",
            None,
            4,
        )
        .unwrap();
    let cid2 = db
        .insert_chunk(
            fid,
            "function",
            Some("beta"),
            6,
            10,
            "fn beta() {}",
            None,
            4,
        )
        .unwrap();

    // alpha at position 0, beta at position 1
    db.insert_embedding(cid1, &unit_vec(DIM, 0)).unwrap();
    db.insert_embedding(cid2, &unit_vec(DIM, 1)).unwrap();

    // Query near position 0 → alpha should be closer.
    let results = db.search_similar(&unit_vec(DIM, 0), 2).unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].name.as_deref(), Some("alpha"));
    assert_eq!(results[1].name.as_deref(), Some("beta"));
    assert!(
        results[0].distance <= results[1].distance,
        "results must be sorted by ascending distance"
    );
}

#[test]
#[serial]
fn knn_limit_is_respected() {
    let db = common::open_test_db();
    let fid = db.upsert_file("b.rs", Some("rust"), "h").unwrap();

    for i in 0..5 {
        let cid = db
            .insert_chunk(fid, "function", Some(&format!("f{i}")), i, i, "x", None, 1)
            .unwrap();
        db.insert_embedding(cid, &unit_vec(DIM, i % DIM)).unwrap();
    }

    let results = db.search_similar(&unit_vec(DIM, 0), 3).unwrap();
    assert!(results.len() <= 3);
}

// ── graph edges ───────────────────────────────────────────────────────────────

#[test]
#[serial]
fn replace_edges_round_trips() {
    let db = common::open_test_db();
    let edges = vec![
        Edge {
            source_file: "src/main.rs".into(),
            source_name: Some("main".into()),
            target_name: "helper".into(),
            kind: EdgeKind::Calls,
            line: 10,
        },
        Edge {
            source_file: "src/main.rs".into(),
            source_name: None,
            target_name: "std::io".into(),
            kind: EdgeKind::Imports,
            line: 1,
        },
    ];
    db.replace_edges("src/main.rs", &edges).unwrap();

    // edges_for_symbol("helper") should include the call edge from main.
    let found = db.edges_for_symbol("helper").unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].source_name.as_deref(), Some("main"));
    assert_eq!(found[0].kind, "calls");
}

#[test]
#[serial]
fn replace_edges_removes_stale_edges() {
    let db = common::open_test_db();
    let e1 = vec![Edge {
        source_file: "src/a.rs".into(),
        source_name: None,
        target_name: "old_fn".into(),
        kind: EdgeKind::Calls,
        line: 5,
    }];
    db.replace_edges("src/a.rs", &e1).unwrap();

    // Re-index the file with different edges — old ones should be gone.
    let e2 = vec![Edge {
        source_file: "src/a.rs".into(),
        source_name: None,
        target_name: "new_fn".into(),
        kind: EdgeKind::Calls,
        line: 5,
    }];
    db.replace_edges("src/a.rs", &e2).unwrap();

    assert!(db.edges_for_symbol("old_fn").unwrap().is_empty());
    assert_eq!(db.edges_for_symbol("new_fn").unwrap().len(), 1);
}
