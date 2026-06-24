-- Project memory: decisions, context, requirements, and notes.
-- Stored separately from the code index so it can be queried independently.

CREATE TABLE IF NOT EXISTS notes (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    kind          TEXT    NOT NULL DEFAULT 'note',   -- decision | context | requirement | note
    title         TEXT    NOT NULL,
    body          TEXT    NOT NULL,
    tags          TEXT,                              -- comma-separated
    linked_files  TEXT,                              -- comma-separated file paths
    created_at    INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Semantic embeddings for notes (one row per note).
CREATE VIRTUAL TABLE IF NOT EXISTS note_embeddings USING vec0(
    note_id    INTEGER PRIMARY KEY,
    embedding  FLOAT[896]
);
