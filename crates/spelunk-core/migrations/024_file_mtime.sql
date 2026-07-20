-- Persist each file's filesystem modification time (unix seconds) so the embed
-- queue can order chunks by file recency without a filesystem stat at
-- queue-build time. The detached embed worker rebuilds the queue purely from
-- the DB, so recency must live in a column, not be re-stat()'d.
-- DEFAULT 0 keeps pre-migration rows deterministic: 0 sorts last under the
-- queue's `mtime DESC` ordering, and never errors.
ALTER TABLE files ADD COLUMN mtime INTEGER NOT NULL DEFAULT 0;
