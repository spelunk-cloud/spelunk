-- spelunk-server migration 007
-- Server-minted, arrival-ordered identity for the delta-pull cursor
-- (`/memory/since?since_id=`). Distinct from `remote_id` (a pushing client's
-- own external_id, used only for push idempotency): `sync_id` is minted by
-- this server at insert time, so it sorts in the order rows actually arrived
-- here, not whatever local clock/uuid a pushing client used. That is what
-- keeps cursor pagination monotonic. Additive and nullable at the schema
-- level; every row is guaranteed one by the backfill in `ServerDb::migrate`.

ALTER TABLE notes ADD COLUMN sync_id TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_sync_id
    ON notes(sync_id) WHERE sync_id IS NOT NULL;
