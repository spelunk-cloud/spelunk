// Pushed-vector fast-path tests for `super::push_local`.

use super::super::test_support::register_sqlite_vec;
use super::*;

// ── pushed-vector fast path ─────────────────────────────────────────────
// A note with a local fp32/896 embedding carries that vector (+ model tag
// + precision "fp32") to a server advertising `accepts_pushed_vectors`, so
// the server stores it as-is; against a server without the capability the
// same note is pushed text-only even though the vector is available. This
// exercises the full `push_local` wiring: it reads the local embedding and
// consults the gate, which the `maybe_attach_vector` unit test cannot.

// Insert an active note plus a valid L2-normalised fp32/896 embedding,
// returning its local id + external uuid.
fn note_with_embedding(store: &MemoryStore) -> (i64, String) {
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();
    let dim = spelunk_core::embeddings::EMBEDDING_DIM;
    let vec: Vec<f32> = vec![1.0 / (dim as f32).sqrt(); dim];
    let blob = spelunk_core::embeddings::vec_to_blob(&vec);
    let rows = store.rows_for_sync(false).unwrap();
    assert_eq!(rows.len(), 1);
    store.insert_embedding(rows[0].local_id, &blob).unwrap();
    (rows[0].local_id, rows[0].uuid.clone())
}

#[tokio::test]
async fn push_local_attaches_vector_when_server_accepts() {
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    let (_id, uuid) = note_with_embedding(&store);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0,
            "results": [{"status": "created", "external_id": uuid, "id": "cloud-1"}]
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    // accepts_pushed_vectors = true → the fp32/896 vector reaches the wire.
    push_local(&store, &client, false, true, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();

    let reqs = server.received_requests().await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let entry = &json["entries"][0];
    assert_eq!(
        entry["vector"].as_array().map(Vec::len),
        Some(spelunk_core::embeddings::EMBEDDING_DIM),
        "server that accepts vectors must receive the 896-dim vector: {entry}"
    );
    assert_eq!(entry["vector_model"], "F2LLM-v2-330M");
    assert_eq!(entry["vector_precision"], "fp32");
}

#[tokio::test]
async fn push_local_stays_text_only_when_server_declines() {
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    let (_id, uuid) = note_with_embedding(&store);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0,
            "results": [{"status": "created", "external_id": uuid, "id": "cloud-1"}]
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    // accepts_pushed_vectors = false → text-only, despite a local vector.
    push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();

    let reqs = server.received_requests().await.unwrap();
    let body = String::from_utf8(reqs[0].body.clone()).unwrap();
    assert!(
        !body.contains("vector"),
        "server without the capability must get a text-only push: {body}"
    );
}

// `note_embeddings` is a `vec0` virtual table with a `FLOAT[896]` column
// (migration `004_memory.sql`): sqlite-vec enforces that exact
// dimension AT INSERT TIME, for every write path (there is only one:
// `insert_embedding`). So a "leftover pre-896 768-dim row" (unlike the
// code-chunk `embeddings` table, which DID have a legacy 768-dim era
// with an explicit recreate-on-open migration in `db.rs`) can never
// actually be written for memory notes: there was never a 768-dim
// memory-embedding vintage to migrate from, and the store itself
// refuses the write. Confirmed here rather than assumed, since it is
// exactly the scenario `push_local`'s dimension guard names in its
// comment.
#[tokio::test]
async fn insert_embedding_rejects_wrong_dimension_vector() {
    use tempfile::TempDir;

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();
    let rows = store.rows_for_sync(false).unwrap();

    let stale_768_blob = spelunk_core::embeddings::vec_to_blob(&vec![1.0f32; 768]);
    let err = store
        .insert_embedding(rows[0].local_id, &stale_768_blob)
        .unwrap_err();
    assert!(
        err.to_string().contains("896") && err.to_string().contains("768"),
        "the vec0 FLOAT[896] column must refuse a 768-dim insert outright \
             (this is what makes a wrong-dimension row unreachable via any \
             application write path): {err}"
    );
}
