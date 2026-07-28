-- spelunk-server migration 006
-- Narrow `remote_id` uniqueness to per-project, matching cloud-api's
-- `(project_id, external_id)` partial unique index (006_memory_entries_additions.sql
-- there). Migration 004 indexed `remote_id` alone (global): two different
-- projects pushing the same external_id collided at the DB layer even though
-- POST /memory/batch's idempotency lookup (`find_by_remote_ids`) is scoped to
-- project_id, so an unrelated project's push failed instead of creating.

DROP INDEX IF EXISTS idx_notes_remote_id;

CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_project_remote_id
    ON notes(project_id, remote_id) WHERE remote_id IS NOT NULL;
