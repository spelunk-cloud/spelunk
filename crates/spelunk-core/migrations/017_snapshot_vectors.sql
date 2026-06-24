-- Vector index for snapshot embeddings.
-- Applied after sqlite-vec extension is loaded (same pattern as 002_vectors.sql).
-- Dimension: 896 (F2LLM-v2-330M default). Existing 768-dim databases are
-- upgraded by apply_dim_upgrade_migration() in db.rs.
CREATE VIRTUAL TABLE IF NOT EXISTS snapshot_embeddings USING vec0(
    chunk_id  INTEGER PRIMARY KEY,
    embedding FLOAT[896]
);
