-- Phase 4: vector index for embeddings.
-- This migration is applied by Database::apply_vector_migration(), called only
-- after the sqlite-vec extension has been loaded into the connection.
-- Dimension: 896 (F2LLM-v2-330M default). Existing 768-dim databases are
-- upgraded by apply_dim_upgrade_migration() in db.rs.
-- Storage: int8 scalar-quantised (4× smaller than f32). F2LLM embeddings are
-- L2-normalised, so int8 (round(x*127)) preserves L2 ranking. See
-- embeddings::vec_to_int8_blob.
CREATE VIRTUAL TABLE IF NOT EXISTS embeddings USING vec0(
    chunk_id INTEGER PRIMARY KEY,
    embedding INT8[896]
);
