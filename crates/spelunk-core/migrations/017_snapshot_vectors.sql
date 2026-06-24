-- Vector index for snapshot embeddings.
-- Applied after sqlite-vec extension is loaded (same pattern as 002_vectors.sql).
-- Dimension: 896 (F2LLM-v2-330M default). Existing 768-dim databases are
-- upgraded by apply_dim_upgrade_migration() in db.rs.
-- Storage: int8 scalar-quantised, matching the `embeddings` table (see
-- 002_vectors.sql and embeddings::vec_to_int8_blob).
CREATE VIRTUAL TABLE IF NOT EXISTS snapshot_embeddings USING vec0(
    chunk_id  INTEGER PRIMARY KEY,
    embedding INT8[896]
);
