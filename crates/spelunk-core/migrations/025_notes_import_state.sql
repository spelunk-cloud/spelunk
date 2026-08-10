-- ADR-077: gate the read-path git-notes import on notes-ref OID movement.
--
-- One row (id = 0) records the OIDs seen at the last merge and the last import,
-- so a read whose notes refs have not moved since skips both the merge
-- subprocess and the import walk. The marker lives here, in memory.db, so the
-- imported working-ref OID is written in the SAME transaction as the imported
-- rows (crash-atomic) and is discarded when the store is deleted/rebuilt.
CREATE TABLE IF NOT EXISTS notes_import_state (
    id INTEGER PRIMARY KEY CHECK (id = 0),
    last_merged_tracking_oid TEXT,
    last_imported_working_oid TEXT
);
