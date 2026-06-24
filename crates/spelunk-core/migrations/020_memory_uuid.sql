-- ADR-037 D2: stable cross-store identity for memory entries.
--
-- Local IDs are autoincrement i64 and are NOT stable across machines, so they
-- cannot correlate a local row with its cloud-api counterpart. We add two
-- identity columns:
--
--   * `uuid`      — the entry's stable local identity (a fresh UUIDv7, Founder
--                   decision §3 — NOT a content-derived UUIDv5). Pushed to the
--                   cloud as `external_id`, which the cloud-api batch endpoint
--                   dedupes on. This makes re-pushing idempotent.
--   * `remote_id` — the cloud-minted entry id (cloud-api mints its own UUIDv7
--                   `id` independently of our `external_id`). Recorded on push
--                   (from the 207 batch result) and on pull. Pull dedupes on
--                   `remote_id` so an entry that originated locally is never
--                   re-inserted when it comes back down the `since` feed.
--
-- cloud-api defaults to UUIDv7 for both identifiers (ADR-032); we match it. The
-- local i64 `id` stays the in-process primary key. Both columns are nullable
-- (legacy rows get a `uuid` lazily on first sync; `remote_id` is set only after
-- a row has been seen on the server), with partial UNIQUE indexes so non-NULL
-- values stay unique without forcing a value onto every legacy row.

ALTER TABLE notes ADD COLUMN uuid TEXT;
ALTER TABLE notes ADD COLUMN remote_id TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_uuid
    ON notes(uuid) WHERE uuid IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_remote_id
    ON notes(remote_id) WHERE remote_id IS NOT NULL;
