use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;

/// Wraps the SQLite connection and provides typed access to the schema.
/// Methods are implemented across sub-modules in the `storage` package.
pub struct Database {
    pub(super) conn: Connection,
}

/// Latest local schema version. Append-only: never renumber an existing step.
/// The runner in `Database::open` gates each migration on this via
/// `PRAGMA user_version`; steps are numbered in the order they run (the field
/// order), not filename order.
pub(super) const CURRENT_SCHEMA_VERSION: i32 = 15;

/// One entry in the migration runner: (target version, migration body).
type MigrationStep = (i32, fn(&Database) -> Result<()>);

impl Database {
    /// Open (or create) the database at `path` and run all migrations.
    ///
    /// Assumes `sqlite3_auto_extension` has already been called in `main` to
    /// load the sqlite-vec extension into every new connection.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating db directory {}", parent.display()))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("opening database at {}", path.display()))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        super::apply_test_page_cap(&conn)?;

        let db = Self { conn };
        db.run_migrations()?;
        Ok(db)
    }

    /// Forward-only migration runner gated on `PRAGMA user_version`.
    ///
    /// A `user_version=0` DB is either brand-new (no user tables) or a
    /// pre-`user_version` field DB. New DBs run every step from 1. Field DBs
    /// have their true version inferred from table shapes / the
    /// `schema_int8_embeddings` marker, stamped, and only later steps run —
    /// blindly re-running all steps would drive the guarded 008–010 ALTERs
    /// through their `duplicate column name` branch needlessly. Each step is
    /// idempotent, so a conservative (one-low) inference stays safe.
    ///
    /// Returns immediately, before any write, when the on-disk header
    /// already reads `CURRENT_SCHEMA_VERSION`: `PRAGMA user_version = N`
    /// opens a write transaction even to re-set an unchanged value, so an
    /// unconditional stamp on every open would make every `Database::open`,
    /// including a read-only command, contend for the write lock against a
    /// concurrent writer instead of only doing so for a genuine migration.
    fn run_migrations(&self) -> Result<()> {
        // Read the RAW on-disk value before any inference below adjusts the
        // working `version`: the skip-write gate further down must key on
        // what is actually stamped in the file header, not on an in-memory
        // value that was only just inferred and has not been persisted yet
        // (a legacy pre-user_version DB reads 0 here but may infer straight
        // to `CURRENT_SCHEMA_VERSION` - that inference still needs writing).
        let raw_version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .context("reading user_version")?;
        if raw_version == CURRENT_SCHEMA_VERSION {
            return Ok(());
        }

        let mut version = raw_version;
        if version == 0 && !self.is_fresh_db()? {
            version = self.infer_legacy_version()?;
        }

        // Each entry: (target_version, migration body). Ordered by call order.
        // Append new steps at the end; never renumber.
        let steps: &[MigrationStep] = &[
            (1, Self::migrate),
            (2, Self::apply_vector_migration),
            (3, Self::apply_graph_migration),
            (4, Self::apply_spec_migration),
            (5, Self::apply_fts_migration),
            (6, Self::apply_token_count_migration),
            (7, Self::apply_graph_rank_migration),
            (8, Self::apply_summary_migration),
            (9, Self::apply_usage_migration),
            (10, Self::apply_compound_graph_idx_migration),
            (11, Self::apply_conventions_migration),
            (12, Self::apply_dim_upgrade_migration),
            (13, Self::apply_drop_snapshots_migration),
            (14, Self::apply_index_meta_migration),
            (15, Self::apply_file_mtime_migration),
        ];
        debug_assert_eq!(
            steps.last().map(|(v, _)| *v),
            Some(CURRENT_SCHEMA_VERSION),
            "steps table must end at CURRENT_SCHEMA_VERSION"
        );

        for (target, body) in steps {
            if *target > version {
                body(self)?;
            }
        }
        // user_version is a header i32; the value is a code-controlled constant.
        self.conn
            .execute_batch(&format!("PRAGMA user_version = {CURRENT_SCHEMA_VERSION}"))
            .context("stamping user_version")?;
        Ok(())
    }

    /// True when the file has no user tables — a freshly created DB that must
    /// run every migration from step 1.
    fn is_fresh_db(&self) -> Result<bool> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master \
                 WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .context("counting user tables")?;
        Ok(n == 0)
    }

    /// Infer the schema version of a pre-`user_version` field DB from its table
    /// shapes. Walks the ladder top-down; the first unmet predicate fixes the
    /// version. A conservative (one-low) result is safe: the re-run step is a
    /// no-op guard that then advances the version.
    fn infer_legacy_version(&self) -> Result<i32> {
        let has_table = |name: &str| -> Result<bool> {
            Ok(self
                .conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                    rusqlite::params![name],
                    |_| Ok(()),
                )
                .optional()
                .context("probing table")?
                .is_some())
        };
        let has_index = |name: &str| -> Result<bool> {
            Ok(self
                .conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1",
                    rusqlite::params![name],
                    |_| Ok(()),
                )
                .optional()
                .context("probing index")?
                .is_some())
        };
        let chunks_has_column = |col: &str| -> Result<bool> {
            let mut stmt = self.conn.prepare("PRAGMA table_info(chunks)")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                if row.get::<_, String>(1)? == col {
                    return Ok(true);
                }
            }
            Ok(false)
        };
        let files_has_column = |col: &str| -> Result<bool> {
            let mut stmt = self.conn.prepare("PRAGMA table_info(files)")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                if row.get::<_, String>(1)? == col {
                    return Ok(true);
                }
            }
            Ok(false)
        };

        let ladder: [(i32, bool); 15] = [
            (1, has_table("chunks")?),
            (2, has_table("embeddings")?),
            (3, has_table("graph_edges")?),
            (4, has_table("specs")?),
            (5, has_table("chunks_fts")?),
            (6, chunks_has_column("token_count")?),
            (7, chunks_has_column("graph_rank")?),
            (8, chunks_has_column("summary")?),
            (9, has_table("usage")?),
            (10, has_index("graph_edges_source_name_kind")?),
            (11, has_table("conventions")?),
            (12, has_table("schema_int8_embeddings")?),
            (13, !has_table("snapshots")?),
            (14, has_table("index_meta")?),
            (15, files_has_column("mtime")?),
        ];
        // Highest version whose predicate and all lower ones hold.
        let mut version = 0;
        for (v, satisfied) in ladder {
            if !satisfied {
                break;
            }
            version = v;
        }
        Ok(version)
    }

    fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/001_initial.sql"))
            .context("running base migrations")?;
        Ok(())
    }

    /// Create the sqlite-vec virtual table. Idempotent (`IF NOT EXISTS`).
    pub fn apply_vector_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/002_vectors.sql"))
            .context("running vector migration (is the sqlite-vec extension loaded?)")?;
        Ok(())
    }

    /// Create the graph_edges table. Idempotent (`IF NOT EXISTS`).
    pub fn apply_graph_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/003_graph.sql"))
            .context("running graph migration")?;
        Ok(())
    }

    /// Create the specs and spec_links tables. Idempotent (`IF NOT EXISTS`).
    pub fn apply_spec_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/006_specs.sql"))
            .context("running spec migration")?;
        Ok(())
    }

    /// Create the FTS5 virtual table and sync triggers. Idempotent (`IF NOT EXISTS`).
    /// Also backfills any existing chunks not yet in the FTS index.
    pub fn apply_fts_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/007_fts.sql"))
            .context("running FTS migration")?;
        self.conn
            .execute_batch(
                "INSERT INTO chunks_fts(rowid, name, content, node_type)
                 SELECT id, name, content, node_type FROM chunks
                 WHERE id NOT IN (SELECT rowid FROM chunks_fts);",
            )
            .context("backfilling FTS index")?;
        Ok(())
    }

    /// Add token_count column to chunks table.
    /// `ALTER TABLE` has no `IF NOT EXISTS`; only the already-applied error is
    /// tolerated so a genuine failure propagates out of `Database::open`.
    pub fn apply_token_count_migration(&self) -> Result<()> {
        match self
            .conn
            .execute_batch(include_str!("../../migrations/008_token_counts.sql"))
        {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e).context("running token_count migration"),
        }
        Ok(())
    }

    /// Add graph_rank column to chunks table.
    pub fn apply_graph_rank_migration(&self) -> Result<()> {
        match self
            .conn
            .execute_batch(include_str!("../../migrations/009_graph_rank.sql"))
        {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e).context("running graph_rank migration"),
        }
        Ok(())
    }

    /// Add summary column to chunks table.
    pub fn apply_summary_migration(&self) -> Result<()> {
        match self
            .conn
            .execute_batch(include_str!("../../migrations/010_summaries.sql"))
        {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e).context("running summary migration"),
        }
        Ok(())
    }

    /// Create the usage table. Idempotent (`IF NOT EXISTS`).
    pub fn apply_usage_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/011_usage.sql"))
            .context("running usage migration")?;
        Ok(())
    }

    /// Upgrade the sqlite-vec embedding tables from 768-dim (Nomic) to 896-dim (F2LLM-v2-330M).
    ///
    /// Idempotent — guarded by the `schema_v896_embeddings` marker table. On
    /// fresh databases the table is already created at 896-dim by
    /// `apply_vector_migration`, so this is a fast no-op. On existing 768-dim
    /// databases the table is dropped and recreated; a full `spelunk index`
    /// re-run is required afterwards.
    pub fn apply_dim_upgrade_migration(&self) -> Result<()> {
        let already: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_int8_embeddings'",
                [],
                |_| Ok(true),
            )
            .optional()
            .context("checking v896 migration marker")?
            .is_some();
        if already {
            return Ok(());
        }

        // Detect whether existing vec0 tables were created with FLOAT[768].
        let upgrade_needed = |table: &str| -> Result<bool> {
            Ok(self
                .conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                    rusqlite::params![table],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .context("querying sqlite_master")?
                // Any float-typed vector table (768 or 896 dim) is rebuilt as
                // int8[896]; F2LLM embeddings are L2-normalised so int8 is
                // lossless enough for ranking and 4× smaller on disk.
                .map(|sql| sql.contains("FLOAT["))
                .unwrap_or(false))
        };

        if upgrade_needed("embeddings")? {
            self.conn
                .execute_batch(
                    "DROP TABLE IF EXISTS embeddings; \
                     CREATE VIRTUAL TABLE embeddings USING vec0(\
                         chunk_id INTEGER PRIMARY KEY, embedding INT8[896]\
                     );",
                )
                .context("upgrading embeddings table to int8[896]")?;
            tracing::info!(
                "embedding storage upgraded to int8[896] (F2LLM-v2-330M); \
                 re-run `spelunk index` to rebuild"
            );
        }
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_int8_embeddings \
                 (sentinel INTEGER PRIMARY KEY);",
            )
            .context("creating int8 migration marker")?;
        Ok(())
    }

    /// Create compound indexes on graph_edges for LinearRAG mention lookups. Idempotent.
    pub fn apply_compound_graph_idx_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!(
                "../../migrations/018_graph_edges_compound_idx.sql"
            ))
            .context("running compound graph index migration")?;
        Ok(())
    }

    /// Drop the snapshot storage tables.
    ///
    /// `snapshots`/`snapshot_files`/`snapshot_chunks` were created by
    /// `016_snapshots.sql` and `snapshot_embeddings` by
    /// `017_snapshot_vectors.sql`, but nothing ever populated them (`spelunk
    /// search --as-of` always errored with "no snapshot found"). Removed for
    /// v1.0 rather than gated behind a flag. `IF EXISTS` makes this a no-op on
    /// fresh databases, which never create these tables in the first place.
    pub fn apply_drop_snapshots_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/021_drop_snapshots.sql"))
            .context("running drop-snapshots migration")?;
        Ok(())
    }

    /// Add the `mtime` column to the files table (unix seconds; recency signal
    /// for the embed queue). `ALTER TABLE` has no `IF NOT EXISTS`, so only the
    /// already-applied error is tolerated; a genuine failure propagates.
    pub fn apply_file_mtime_migration(&self) -> Result<()> {
        match self
            .conn
            .execute_batch(include_str!("../../migrations/024_file_mtime.sql"))
        {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e).context("running file mtime migration"),
        }
        Ok(())
    }

    /// Create the index_meta KV table (embedding provenance). Idempotent.
    pub fn apply_index_meta_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/022_index_meta.sql"))
            .context("running index_meta migration")?;
        Ok(())
    }

    /// Read the recorded embedding model id, or `None` if never stamped (a DB
    /// predating provenance, treated as "matches anything" until first write).
    pub fn embedding_model(&self) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'embedding_model'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("reading embedding_model")
    }

    /// Assert the current model matches this DB's provenance before writing
    /// embeddings, stamping it on a fresh/legacy DB. A recorded id that differs
    /// is a hard error: mixing two model ids in one KNN space is corruption.
    pub fn ensure_embedding_model(&self, model_id: &str) -> Result<()> {
        match self.embedding_model()? {
            Some(recorded) if recorded == model_id => Ok(()),
            Some(recorded) => anyhow::bail!(
                "index was built with embedding model '{recorded}' but this build uses \
                 '{model_id}'. Vectors from two models must not share one search index. \
                 Re-index from scratch: `spelunk index . --force` (or delete .spelunk/index.db)."
            ),
            None => {
                self.conn
                    .execute(
                        "INSERT OR REPLACE INTO index_meta (key, value) \
                         VALUES ('embedding_model', ?1), ('embedding_dim', ?2)",
                        rusqlite::params![model_id, crate::embeddings::EMBEDDING_DIM.to_string()],
                    )
                    .context("stamping embedding provenance")?;
                Ok(())
            }
        }
    }

    /// Read the recorded chunker config id (`chunker::chunker_config_id`), or
    /// `None` if never stamped (a DB predating this provenance key).
    pub fn chunker_config(&self) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'chunker_config'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("reading chunker_config")
    }

    /// Compare the running build's chunker config against this DB's
    /// provenance, stamping it on a fresh/legacy DB. Unlike
    /// [`ensure_embedding_model`](Self::ensure_embedding_model), a mismatch
    /// here is same-model/same-dimension drift (e.g. a changed chunk-token
    /// cap), not a hard incompatibility: old and new chunks coexist in the
    /// same vector space at different granularity, so this never errors.
    /// Returns the stale recorded value on a mismatch so the caller can warn
    /// without failing the run; the stamp is left as-is (not overwritten) so
    /// the warning keeps firing until a `--force` run re-chunks everything
    /// under the current config and calls
    /// [`stamp_chunker_config`](Self::stamp_chunker_config) to refresh it.
    pub fn ensure_chunker_config(&self, config: &str) -> Result<Option<String>> {
        match self.chunker_config()? {
            Some(recorded) if recorded == config => Ok(None),
            Some(recorded) => Ok(Some(recorded)),
            None => {
                self.conn
                    .execute(
                        "INSERT OR REPLACE INTO index_meta (key, value) VALUES ('chunker_config', ?1)",
                        rusqlite::params![config],
                    )
                    .context("stamping chunker config provenance")?;
                Ok(None)
            }
        }
    }

    /// Unconditionally record `config` as this DB's chunker-config provenance,
    /// regardless of what (if anything) was previously stamped. A `--force`
    /// re-index re-chunks every file, so once it finishes every stored chunk
    /// was cut under `config`; refreshing the stamp here is what makes
    /// [`ensure_chunker_config`](Self::ensure_chunker_config) stop warning on
    /// the next normal run, until the config next changes.
    pub fn stamp_chunker_config(&self, config: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO index_meta (key, value) VALUES ('chunker_config', ?1)",
                rusqlite::params![config],
            )
            .context("stamping chunker config provenance")?;
        Ok(())
    }

    /// Insert or replace an embedding for a chunk.
    ///
    /// Takes the raw float vector; it is int8-quantised here for the
    /// `embeddings` `int8[896]` column (see `embeddings::vec_to_int8_blob`).
    pub fn insert_embedding(&self, chunk_id: i64, vector: &[f32]) -> Result<()> {
        let blob = crate::embeddings::vec_to_int8_blob(vector);
        // The `embeddings` table is a sqlite-vec `vec0` virtual table, which does
        // not honour `INSERT OR REPLACE`/`ON CONFLICT`: a second insert for an
        // existing `chunk_id` raises a hard UNIQUE-constraint error instead of
        // overwriting. Emulate replace with an explicit delete-then-insert, kept
        // atomic under one transaction so a repeated `chunk_id` is genuine
        // last-write-wins (re-embed-on-change, `index --force`). When a caller
        // already holds a transaction (batch flush) we join it rather than
        // nesting a BEGIN, which vec0/SQLite would reject.
        // sqlite-vec treats a raw BLOB as float32; vec_int8() reinterprets the
        // bytes as the int8 vector the column expects.
        let write = |conn: &Connection| -> rusqlite::Result<()> {
            conn.execute(
                "DELETE FROM embeddings WHERE chunk_id = ?1",
                rusqlite::params![chunk_id],
            )?;
            conn.execute(
                "INSERT INTO embeddings (chunk_id, embedding) VALUES (?1, vec_int8(?2))",
                rusqlite::params![chunk_id, blob],
            )?;
            Ok(())
        };
        if self.conn.is_autocommit() {
            let tx = self.conn.unchecked_transaction()?;
            write(&tx)?;
            tx.commit()?;
        } else {
            write(&self.conn)?;
        }
        Ok(())
    }

    /// Insert or replace a whole batch of embeddings in a single transaction.
    ///
    /// Same per-row replace shape as [`insert_embedding`] (the `embeddings`
    /// vec0 table doesn't honour `INSERT OR REPLACE`, so a repeated
    /// `chunk_id` is emulated with delete-then-insert), but one commit for
    /// the whole batch instead of one implicit autocommit per row (mirrors
    /// the `update_graph_ranks` batch pattern). The embed phase already holds
    /// the whole batch's vectors in memory by the time it writes them, so the
    /// commit boundary is the batch: on an untimely kill the transaction is
    /// rolled back atomically and `chunks_missing_embeddings` re-queues the
    /// entire batch, never a partial one.
    pub fn insert_embeddings(&self, rows: &[(i64, Vec<f32>)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (chunk_id, vector) in rows {
            let blob = crate::embeddings::vec_to_int8_blob(vector);
            tx.execute(
                "DELETE FROM embeddings WHERE chunk_id = ?1",
                rusqlite::params![chunk_id],
            )?;
            tx.execute(
                "INSERT INTO embeddings (chunk_id, embedding) VALUES (?1, vec_int8(?2))",
                rusqlite::params![chunk_id, blob],
            )?;
        }
        // Held before commit (not after): the crash-safety suite needs the
        // write lock genuinely open here to test a concurrent reader/writer
        // against it, and a real SIGKILL landed here exercises the same
        // uncommitted-batch window `insert_embeddings_shaped_batch_leaves_
        // nothing_after_a_hard_process_exit` proves with a simulated exit.
        super::pause_for_crash_test("embed_tx_open");
        tx.commit()?;
        Ok(())
    }

    /// Delete all embeddings associated with chunks of a given file.
    pub fn delete_embeddings_for_file(&self, file_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM embeddings WHERE chunk_id IN (
                 SELECT id FROM chunks WHERE file_id = ?1
             )",
            rusqlite::params![file_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{CURRENT_SCHEMA_VERSION, Database};
    use rusqlite::{Connection, OptionalExtension};
    use std::sync::OnceLock;

    fn register_sqlite_vec() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )));
            }
        });
    }

    fn user_version(conn: &Connection) -> i32 {
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    /// A freshly created DB runs every migration and ends stamped at the latest
    /// version.
    #[test]
    fn fresh_db_stamps_current_version() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open fresh");
        assert_eq!(user_version(&db.conn), CURRENT_SCHEMA_VERSION);
    }

    /// Opening an already-migrated DB a second time is a clean no-op that keeps
    /// the version.
    #[test]
    fn reopen_is_idempotent() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::open(tmp.path()).expect("first open");
        drop(db);
        let db = Database::open(tmp.path()).expect("second open");
        assert_eq!(user_version(&db.conn), CURRENT_SCHEMA_VERSION);
    }

    /// A DB built by the previous binary reports `user_version = 0` but has all
    /// tables. It must be inferred at the latest version, stamped, and re-run
    /// zero erroring migration bodies.
    #[test]
    fn legacy_fully_migrated_db_is_inferred_and_stamped() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let db = Database::open(tmp.path()).expect("build via runner");
            // Simulate a pre-user_version binary: reset the header stamp.
            db.conn
                .execute_batch("PRAGMA user_version = 0")
                .expect("reset version");
        }
        let db = Database::open(tmp.path()).expect("reopen legacy");
        assert_eq!(
            user_version(&db.conn),
            CURRENT_SCHEMA_VERSION,
            "a fully-migrated legacy DB must be inferred at the latest version"
        );
    }

    // `run_migrations` used to stamp `PRAGMA user_version` unconditionally on
    // every open, even when nothing needed migrating - and setting that
    // pragma always opens a write transaction, so a concurrent reader
    // (`spelunk search` while `spelunk index` runs) could fail with
    // "database is locked" on this alone, never touching a genuine
    // migration. Reopening an already-current DB while another connection
    // holds an open writer transaction must now succeed.
    #[test]
    fn opening_an_already_current_db_never_writes_so_a_concurrent_writer_cannot_lock_it_out() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        Database::open(tmp.path()).expect("build and fully migrate");
        assert_eq!(
            user_version(&Database::open(tmp.path()).unwrap().conn),
            CURRENT_SCHEMA_VERSION
        );

        let locker = Connection::open(tmp.path()).expect("open locker connection");
        locker
            .execute_batch(
                "BEGIN IMMEDIATE; \
                 INSERT INTO files (path, hash, indexed_at) VALUES ('x', 'y', 0);",
            )
            .expect("take the write lock");

        let reopened = Database::open(tmp.path());
        locker.execute_batch("ROLLBACK;").expect("release the lock");

        reopened.expect(
            "opening an already-migrated DB must never attempt a write, so it must succeed even \
             while another connection holds the write lock",
        );
    }

    /// A partially-migrated legacy DB (chunks without `summary`, no index_meta,
    /// version 0) is inferred at 7 and only the later steps run to reach the
    /// latest version.
    #[test]
    fn partially_migrated_legacy_db_is_inferred_then_completed() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            // Build a real DB, then strip it back to a version-7 shape: drop the
            // later columns/tables so inference lands at 7.
            let db = Database::open(tmp.path()).expect("build");
            db.conn
                .execute_batch(
                    "ALTER TABLE chunks DROP COLUMN summary; \
                     DROP TABLE IF EXISTS usage; \
                     DROP INDEX IF EXISTS graph_edges_source_name_kind; \
                     DROP TABLE IF EXISTS conventions; \
                     DROP TABLE IF EXISTS schema_int8_embeddings; \
                     DROP TABLE IF EXISTS index_meta; \
                     PRAGMA user_version = 0;",
                )
                .expect("strip to v7 shape");
            assert!(super::Database::infer_legacy_version(&db).unwrap() == 7);
        }
        let db = Database::open(tmp.path()).expect("reopen partial");
        assert_eq!(user_version(&db.conn), CURRENT_SCHEMA_VERSION);
        // The later step (index_meta) actually ran.
        assert!(db.embedding_model().unwrap().is_none());
        db.ensure_embedding_model("m").unwrap();
        assert_eq!(db.embedding_model().unwrap().as_deref(), Some("m"));
    }

    /// A genuine failure in the guarded 008–010 ALTERs (not a duplicate column)
    /// propagates out rather than being swallowed. We exercise the guard by
    /// dropping the whole `chunks` table so the ALTER fails with "no such
    /// table", which must surface as an `Err`.
    #[test]
    fn token_count_migration_propagates_non_duplicate_error() {
        register_sqlite_vec();
        let conn = Connection::open_in_memory().unwrap();
        let db = Database { conn };
        // No `chunks` table exists → the ALTER fails with "no such table".
        let err = db
            .apply_token_count_migration()
            .expect_err("missing chunks table must surface as an error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no such table") || msg.contains("token_count migration"),
            "a real migration failure must propagate, got: {msg}"
        );
    }

    /// The `files.mtime` migration is idempotent: applying it to a pre-existing
    /// (pre-column) index adds the column with default 0 without error, and
    /// applying it again on an already-migrated DB is a tolerated no-op.
    #[test]
    fn file_mtime_migration_is_idempotent() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");

        let has_mtime = |db: &Database| -> bool {
            let mut stmt = db.conn.prepare("PRAGMA table_info(files)").unwrap();
            let mut rows = stmt.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                if row.get::<_, String>(1).unwrap() == "mtime" {
                    return true;
                }
            }
            false
        };

        // Fresh DB already has the column from the full migration run.
        assert!(has_mtime(&db), "fresh DB has the mtime column");

        // Simulate a pre-column index: drop the column, then re-run the migration.
        db.conn
            .execute_batch("ALTER TABLE files DROP COLUMN mtime")
            .expect("drop mtime to simulate a pre-migration index");
        assert!(!has_mtime(&db), "column dropped to model a legacy index");

        db.apply_file_mtime_migration()
            .expect("migration must add the column to a pre-column index without error");
        assert!(has_mtime(&db), "migration re-added the mtime column");

        // A legacy row inserted before the column existed reads back as 0.
        db.conn
            .execute(
                "INSERT INTO files (path, language, hash, indexed_at) VALUES ('legacy.rs', 'rust', 'h', 1)",
                [],
            )
            .unwrap();
        let mtime: i64 = db
            .conn
            .query_row(
                "SELECT mtime FROM files WHERE path = 'legacy.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mtime, 0, "a row lacking an explicit mtime defaults to 0");

        // Re-applying on the already-migrated DB is a tolerated no-op.
        db.apply_file_mtime_migration()
            .expect("re-applying the migration on a migrated DB is a no-op");
        assert!(has_mtime(&db));
    }

    /// Exercises the actual legacy-inference rung for this migration (ladder
    /// entry `(15, files_has_column("mtime"))`), not just the direct
    /// `apply_file_mtime_migration` idempotency check above. A DB frozen at v14
    /// (every table/column through `index_meta` present, `files.mtime` not yet
    /// added, `user_version` reset to 0 — the real shape of an index built by
    /// the previous binary) must be inferred at exactly 14 through
    /// `Database::open`'s normal migration runner, and only step 15 must run to
    /// bring it current.
    #[test]
    fn legacy_db_frozen_at_v14_infers_14_and_applies_only_mtime_step() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let db = Database::open(tmp.path()).expect("build fully-migrated DB");
            // Roll back to the pre-024 shape: drop only the mtime column,
            // leaving every other v1..14 table/column intact, then reset the
            // version stamp so `Database::open` must re-infer it from shape.
            db.conn
                .execute_batch("ALTER TABLE files DROP COLUMN mtime; PRAGMA user_version = 0;")
                .expect("roll back to pre-mtime shape");
            assert_eq!(
                Database::infer_legacy_version(&db).unwrap(),
                14,
                "with every v1..14 predicate true and only the mtime rung false, \
                 inference must land exactly at 14"
            );
            // A row inserted while at this legacy shape has no mtime column at
            // all yet (pre-migration data).
            db.conn
                .execute(
                    "INSERT INTO files (path, language, hash, indexed_at) VALUES ('old.rs', 'rust', 'h', 1)",
                    [],
                )
                .unwrap();
        }

        // Reopen through the normal runner (not calling apply_file_mtime_migration
        // directly): this is the real "agent upgrades the binary" path.
        let db = Database::open(tmp.path()).expect("reopen legacy v14 DB");
        assert_eq!(user_version(&db.conn), CURRENT_SCHEMA_VERSION);

        let mtime: i64 = db
            .conn
            .query_row("SELECT mtime FROM files WHERE path = 'old.rs'", [], |r| {
                r.get(0)
            })
            .expect("mtime column must exist and be queryable after inferred upgrade");
        assert_eq!(
            mtime, 0,
            "a pre-existing row defaults to mtime 0, not an error"
        );
    }

    /// Defends the ladder's early-break behaviour against a hand-tampered /
    /// corrupted DB where a *later* rung's predicate is true but an *earlier*
    /// one is false — a state the normal forward-only migration path can never
    /// produce, but one a manual `ALTER TABLE` (or a hand-restored backup)
    /// could. `Database::open` must still complete without error or data
    /// corruption: the ladder takes the lowest satisfied version (ignoring the
    /// spuriously-true later rung), and every step from there re-applies
    /// idempotently rather than double-erroring on the already-present column.
    #[test]
    fn migration_ladder_tolerates_out_of_order_manually_tampered_state() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let db = Database::open(tmp.path()).expect("build fully-migrated DB");
            // Simulate tampering: drop the `usage` table (rung 9) while
            // `files.mtime` (rung 15) is left present — a combination the real
            // forward-only runner would never produce on its own.
            db.conn
                .execute_batch("DROP TABLE IF EXISTS usage; PRAGMA user_version = 0;")
                .expect("tamper: drop usage, keep mtime");
            assert_eq!(
                Database::infer_legacy_version(&db).unwrap(),
                8,
                "the ladder must break at the first false predicate (rung 9, usage table) \
                 and ignore the spuriously-true rung 15"
            );
        }

        // Reopening must not error even though this replays step 15
        // (files.mtime already exists) on top of a version-8 inference.
        let db = Database::open(tmp.path())
            .expect("reopening a tampered-but-recoverable DB must not error or corrupt state");
        assert_eq!(user_version(&db.conn), CURRENT_SCHEMA_VERSION);

        let has_usage: bool = db
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='usage'",
                [],
                |_| Ok(true),
            )
            .optional()
            .unwrap()
            .is_some();
        assert!(
            has_usage,
            "the usage table must be recreated by the replayed step 9"
        );
    }

    /// Model provenance round-trips through index_meta, and a mismatch is a hard
    /// error while an absent model is backfilled.
    #[test]
    fn embedding_model_stamp_reject_and_backfill() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        // Absent → backfilled (no error).
        assert!(db.embedding_model().unwrap().is_none());
        db.ensure_embedding_model("F2LLM-v2-330M@896")
            .expect("first stamp");
        assert_eq!(
            db.embedding_model().unwrap().as_deref(),
            Some("F2LLM-v2-330M@896")
        );
        // Same model → no-op.
        db.ensure_embedding_model("F2LLM-v2-330M@896")
            .expect("same model is fine");
        // Different model → hard error instructing a re-index.
        let err = db
            .ensure_embedding_model("some-other-model@896")
            .expect_err("model mismatch must be a hard error");
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("re-index"),
            "message must instruct re-index: {msg}"
        );
    }

    /// Chunker config provenance round-trips like `embedding_model`, but a
    /// mismatch returns the stale value instead of erroring, and the run
    /// keeps going (a chunk-cap change doesn't corrupt the vector space).
    #[test]
    fn chunker_config_stamp_and_warn_on_mismatch() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        // Absent → backfilled (no error).
        assert!(db.chunker_config().unwrap().is_none());
        assert_eq!(
            db.ensure_chunker_config("max_chunk_tokens=512")
                .expect("first stamp"),
            None
        );
        assert_eq!(
            db.chunker_config().unwrap().as_deref(),
            Some("max_chunk_tokens=512")
        );
        // Same config → no-op, no warning.
        assert_eq!(
            db.ensure_chunker_config("max_chunk_tokens=512")
                .expect("same config is fine"),
            None
        );
        // Different config → the stale value comes back for the caller to
        // warn with, not an error, and the stamp is left untouched.
        let recorded = db
            .ensure_chunker_config("max_chunk_tokens=2048")
            .expect("mismatch must not fail")
            .expect("mismatch must surface the recorded value");
        assert_eq!(recorded, "max_chunk_tokens=512");
        assert_eq!(
            db.chunker_config().unwrap().as_deref(),
            Some("max_chunk_tokens=512"),
            "a mismatch must not overwrite the stamp"
        );
    }

    /// A DB stamped under an old chunker config still lets normal
    /// (non-`--force`) indexing proceed: `ensure_chunker_config` never
    /// blocks the caller, it only reports the drift.
    #[test]
    fn chunker_config_mismatch_does_not_block_incremental_indexing() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        db.ensure_chunker_config("max_chunk_tokens=2048")
            .expect("stamp old config");

        // Simulates a build upgraded to the new default: the check reports
        // the drift but returns `Ok`, so a normal `spelunk index` run keeps
        // going (incremental skip-by-hash still applies to unchanged files).
        let warned = db
            .ensure_chunker_config("max_chunk_tokens=512")
            .expect("a config mismatch must be Ok, not an error");
        assert_eq!(warned.as_deref(), Some("max_chunk_tokens=2048"));
    }

    /// `stamp_chunker_config` is the refresh mechanism a `--force` re-index
    /// uses to silence the drift warning: stamp old, detect the mismatch,
    /// force-refresh, then confirm the same config no longer reports drift.
    #[test]
    fn stamp_chunker_config_silences_a_prior_mismatch() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        db.ensure_chunker_config("max_chunk_tokens=2048")
            .expect("stamp old config");

        // Drift is detected before the refresh.
        assert_eq!(
            db.ensure_chunker_config("max_chunk_tokens=512")
                .expect("mismatch must not fail"),
            Some("max_chunk_tokens=2048".to_string()),
            "the old stamp must still be reported as drift before any refresh"
        );

        // A `--force` run re-chunks everything and refreshes the stamp,
        // unconditionally, not just on a first-ever write.
        db.stamp_chunker_config("max_chunk_tokens=512")
            .expect("force refresh");
        assert_eq!(
            db.chunker_config().unwrap().as_deref(),
            Some("max_chunk_tokens=512")
        );

        // The next normal (non-`--force`) run now sees a match: no drift.
        assert_eq!(
            db.ensure_chunker_config("max_chunk_tokens=512")
                .expect("post-refresh check must not fail"),
            None,
            "after the refresh, the same config must no longer be reported as drift"
        );
    }

    fn embedding_count(db: &Database) -> i64 {
        db.conn
            .query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))
            .unwrap()
    }

    /// The batch insert writes every row of a batch in one call.
    #[test]
    fn insert_embeddings_commits_the_whole_batch() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let rows = vec![
            (1i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM]),
            (2i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM]),
            (3i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM]),
        ];
        db.insert_embeddings(&rows).expect("batch insert");
        assert_eq!(embedding_count(&db), 3, "all three rows persist");
    }

    /// The batch is a single transaction: if any row fails, none commit. This
    /// is the guarantee the resume story rests on — a process killed while a
    /// batch is being written leaves zero partial rows behind, so
    /// `chunks_missing_embeddings` re-queues the whole batch cleanly. A per-row
    /// autocommit loop would instead leak the rows written before the failure.
    #[test]
    fn insert_embeddings_is_atomic_a_failing_row_rolls_back_the_whole_batch() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        // The second row has the wrong dimension; sqlite-vec rejects it at
        // insert time, aborting the transaction after the first row was staged.
        let rows = vec![
            (1i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM]),
            (2i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM - 1]),
        ];
        db.insert_embeddings(&rows)
            .expect_err("a wrong-dimension row must fail the whole batch");
        assert_eq!(
            embedding_count(&db),
            0,
            "an atomic batch leaves zero rows when any row fails; the first, valid \
             row must not survive the aborted transaction"
        );
    }

    /// An empty batch is a deliberate no-op, not an error. `run_embed_phase`
    /// never constructs one today (batches are only built from a non-empty
    /// slice of the work queue), but the boundary must still be safe.
    #[test]
    fn insert_embeddings_empty_batch_is_a_no_op() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        db.insert_embeddings(&[])
            .expect("an empty batch must not error");
        assert_eq!(embedding_count(&db), 0);
    }

    /// A batch of exactly one row commits normally — the boundary case
    /// closest to the old per-row behaviour must not silently regress to a
    /// non-transactional bypass.
    #[test]
    fn insert_embeddings_single_row_batch_commits() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let rows = vec![(1i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM])];
        db.insert_embeddings(&rows)
            .expect("single-row batch insert");
        assert_eq!(embedding_count(&db), 1);
    }

    /// Was a bug (see `git blame`/ADR-070): `insert_embedding`'s doc-comment
    /// promises "insert or replace", but plain `INSERT OR REPLACE` against the
    /// `embeddings` vec0 virtual table does not honour the conflict clause —
    /// a second call for the same `chunk_id` raised `UNIQUE constraint
    /// failed` instead of overwriting. This mattered because the run-level
    /// resume test's own comment and the batch engineer's handoff note both
    /// cited OR-REPLACE idempotency as a safety property to lean on. Fixed by
    /// emulating replace with an explicit delete-then-insert (see
    /// `insert_embedding`); this test now pins the fixed, promised behaviour.
    #[test]
    fn insert_embedding_single_row_path_does_not_actually_replace_a_repeated_chunk_id() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        db.insert_embedding(1, &vec![0.1f32; crate::embeddings::EMBEDDING_DIM])
            .expect("first insert");
        db.insert_embedding(1, &vec![0.9f32; crate::embeddings::EMBEDDING_DIM])
            .expect(
                "replacing an already-committed chunk_id must not error — this is the doc-comment's \
                 promised behaviour and currently fails",
            );
        assert_eq!(embedding_count(&db), 1);
    }

    /// Same underlying bug as the test above, exercised through the batch
    /// path this story added: a batch containing the same `chunk_id` twice
    /// (still legitimate input — nothing in `insert_embeddings`'s contract
    /// forbids it) used to hit the identical `UNIQUE constraint failed`
    /// error, because it was the same OR-REPLACE-against-vec0 gap, not
    /// something the transaction wrapper introduced. `insert_embeddings` now
    /// applies the same delete-then-insert-per-row fix inside its batch
    /// transaction, so a repeated id within one batch collapses to a single
    /// last-write-wins row instead of erroring.
    #[test]
    fn insert_embeddings_duplicate_chunk_id_within_one_batch_last_write_wins() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let rows = vec![
            (1i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM]),
            (1i64, vec![0.9f32; crate::embeddings::EMBEDDING_DIM]),
        ];
        db.insert_embeddings(&rows)
            .expect("a duplicate chunk_id within a batch must not error");
        assert_eq!(
            embedding_count(&db),
            1,
            "one logical chunk_id must produce exactly one row, not two"
        );
    }

    /// The batch ceiling is 256 chunks (`resolve_batch_ceiling`'s default) —
    /// confirm the transaction wrapper itself has no lower internal limit
    /// (e.g. SQLite's bound statement/variable count) that would make a
    /// full-size real batch behave differently from the small batches every
    /// other test here uses.
    #[test]
    fn insert_embeddings_handles_a_full_size_256_batch() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let rows: Vec<(i64, Vec<f32>)> = (1i64..=256)
            .map(|id| (id, vec![0.1f32; crate::embeddings::EMBEDDING_DIM]))
            .collect();
        db.insert_embeddings(&rows).expect("full-size batch insert");
        assert_eq!(embedding_count(&db), 256);
    }

    /// The other atomicity test triggers rollback via a sqlite-vec dimension
    /// check, which is an application-level guard, not a generic SQLite
    /// failure. Prove the same "whole batch or nothing" guarantee holds for a
    /// genuine SQLite runtime error too: hold the file's write lock from a
    /// second connection (no `busy_timeout` is configured — see
    /// `Database::open`) so `insert_embeddings`'s own write hits `SQLITE_BUSY`
    /// on the very first row, unrelated to any row's content.
    #[test]
    fn insert_embeddings_rolls_back_on_a_real_sqlite_error_not_just_bad_dimension() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::open(tmp.path()).expect("open");

        // A second connection takes and holds the file's write lock.
        let locker = Connection::open(tmp.path()).expect("second connection");
        locker
            .execute_batch(
                "BEGIN IMMEDIATE; \
                 INSERT OR REPLACE INTO index_meta (key, value) VALUES ('lock_probe', '1');",
            )
            .expect("acquire the write lock");

        let rows = vec![
            (1i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM]),
            (2i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM]),
        ];
        let err = db
            .insert_embeddings(&rows)
            .expect_err("a locked database must surface as a real error, not silently succeed");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("lock") || msg.contains("busy"),
            "expected a locking error, got: {msg}"
        );

        locker.execute_batch("COMMIT;").expect("release the lock");
        assert_eq!(
            embedding_count(&db),
            0,
            "a batch that fails under lock contention leaves zero rows, the same atomicity \
             guarantee the bad-dimension case exercises"
        );

        // The connection recovers cleanly once the lock is released — this
        // was not a poisoned/half-open transaction.
        db.insert_embeddings(&rows)
            .expect("insert succeeds once the lock is released");
        assert_eq!(embedding_count(&db), 2);
    }

    /// The batch change makes the write transaction live for the whole batch
    /// instead of a single row, so it holds the writer lock longer than the
    /// old per-row autocommit ever did. WAL mode should still let a concurrent
    /// reader (e.g. `spelunk search` running mid-embed) proceed rather than
    /// blocking or erroring — verify this empirically instead of assuming it.
    #[test]
    fn open_batch_transaction_does_not_block_a_concurrent_reader() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::open(tmp.path()).expect("open");

        // Hold open the same transaction/insert shape `insert_embeddings`
        // uses, uncommitted, mimicking a batch write still in flight.
        let tx = db.conn.unchecked_transaction().expect("begin");
        let blob =
            crate::embeddings::vec_to_int8_blob(&vec![0.2f32; crate::embeddings::EMBEDDING_DIM]);
        tx.execute(
            "INSERT OR REPLACE INTO embeddings (chunk_id, embedding) VALUES (?1, vec_int8(?2))",
            rusqlite::params![1i64, blob],
        )
        .expect("staged write inside the open transaction");

        // A second connection, mimicking a concurrent `spelunk search` reader.
        let reader = Connection::open(tmp.path()).expect("second connection");
        let count: i64 = reader
            .query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))
            .expect(
                "a concurrent reader must not be blocked or errored by an open, uncommitted \
                 batch write transaction — WAL mode is expected to allow this",
            );
        assert_eq!(
            count, 0,
            "the reader sees the pre-transaction snapshot, not the uncommitted staged row \
             (WAL snapshot isolation)"
        );

        // The real code path a concurrent `spelunk search` takes — a sqlite-vec
        // KNN `MATCH` query, not a plain `SELECT count(*)` — against the same
        // virtual table the open transaction is writing into. `Database` opens
        // its own connection, so build a second `Database` over the reader's
        // (already-migrated) file rather than a raw `Connection`.
        let reader_db = Database { conn: reader };
        reader_db
            .search_similar(&vec![0.2f32; crate::embeddings::EMBEDDING_DIM], 5)
            .expect(
                "a concurrent KNN MATCH query against the embeddings vec0 table must not be \
                 blocked or errored by the open writer transaction either — vec0 virtual \
                 tables don't always share ordinary tables' WAL locking behaviour, so this is \
                 checked separately from the plain SELECT above",
            );

        tx.commit().expect("writer commits");
        let count_after: i64 = reader_db
            .conn
            .query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))
            .expect("reader still works after the writer commits");
        assert_eq!(count_after, 1, "reader's next read sees the committed row");
    }

    /// The run-level resume regression test (`embed_phase.rs`) simulates an
    /// interrupted batch by never calling `insert_embeddings` at all (the
    /// mock server 500s before the batch write would happen) — a weaker
    /// guarantee than the spec's "kill mid-batch" acceptance criterion, since
    /// it never proves anything about a transaction that *was* opened and
    /// *was* partway through writing when the process died.
    ///
    /// This test closes that gap literally: a child process opens the same
    /// on-disk DB, stages every row of a batch inside an open transaction,
    /// then hard-exits via `std::process::exit` — which runs no destructors,
    /// so neither `COMMIT` nor `ROLLBACK` is ever sent, the closest safe
    /// stand-in for a `SIGKILL` mid-commit (a real signal would skip Drop the
    /// same way; unlike an in-process leak, `std::process::exit` still lets
    /// the OS release the file lock, so the parent can reopen cleanly — a
    /// leaked `Connection` in the same process cannot be observed this way,
    /// since the lock would never clear). The child prints a marker after
    /// staging so a filter/argv mismatch can never silently no-op this test
    /// into a false pass.
    #[test]
    fn insert_embeddings_shaped_batch_leaves_nothing_after_a_hard_process_exit() {
        const HELPER_ENV: &str = "SPELUNK_TEST_CRASH_MID_BATCH_DB_PATH";
        const STAGED_MARKER: &str = "SPELUNK_TEST_CRASH_MID_BATCH_STAGED";

        if let Ok(path) = std::env::var(HELPER_ENV) {
            // Child mode: stage a 3-row batch inside an open transaction using
            // the exact insert shape `insert_embeddings` uses, then hard-exit
            // before commit or rollback.
            register_sqlite_vec();
            let db = Database::open(std::path::Path::new(&path)).expect("child open");
            let tx = db.conn.unchecked_transaction().expect("child begin");
            for chunk_id in 1i64..=3 {
                let blob = crate::embeddings::vec_to_int8_blob(&vec![
                    0.3f32;
                    crate::embeddings::EMBEDDING_DIM
                ]);
                tx.execute(
                    "INSERT OR REPLACE INTO embeddings (chunk_id, embedding) VALUES \
                     (?1, vec_int8(?2))",
                    rusqlite::params![chunk_id, blob],
                )
                .expect("child staged write");
            }
            println!("{STAGED_MARKER}");
            std::process::exit(0);
        }

        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Pre-create the schema so the child doesn't race the parent on
        // migrations.
        Database::open(tmp.path()).expect("pre-create schema");

        let exe = std::env::current_exe().expect("current test binary");
        let output = std::process::Command::new(exe)
            .arg("--exact")
            .arg(
                "storage::db::tests::insert_embeddings_shaped_batch_leaves_nothing_after_a_hard_process_exit",
            )
            .arg("--test-threads=1")
            .arg("--nocapture")
            .env(HELPER_ENV, tmp.path())
            .output()
            .expect("spawn the crash-simulation child");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "the child must hard-exit cleanly (code 0); stdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains(STAGED_MARKER),
            "the child must actually reach and execute the staged-write path (guards against \
             a test-name/filter mismatch silently matching zero tests and false-passing); \
             stdout:\n{stdout}"
        );

        let reopened = Database::open(tmp.path()).expect("reopen after the simulated crash");
        assert_eq!(
            embedding_count(&reopened),
            0,
            "a batch abandoned by a hard process exit before commit must leave zero rows — the \
             literal 'kill mid-batch' scenario, not just an in-process Err short-circuit"
        );
    }

    /// Two independent, fully-committed `insert_embedding` calls for the same
    /// `chunk_id` must leave exactly one row holding the *second* vector — the
    /// re-embed-on-content-change idempotency the resume/`index --force` paths
    /// assume. On a `vec0` virtual table plain `INSERT OR REPLACE` silently
    /// fails to do this (the conflict clause isn't honoured), so this pins the
    /// delete-then-insert fix.
    #[test]
    fn insert_embedding_single_row_path_replaces_a_repeated_chunk_id() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let dim = crate::embeddings::EMBEDDING_DIM;

        let mut first = vec![0f32; dim];
        first[0] = 1.0;
        let mut second = vec![0f32; dim];
        second[10] = 1.0;

        db.insert_embedding(1, &first).expect("first insert");
        db.insert_embedding(1, &second)
            .expect("second insert (replace)");

        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE chunk_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "a repeated chunk_id must leave exactly one row");

        let stored: Vec<u8> = db
            .conn
            .query_row(
                "SELECT embedding FROM embeddings WHERE chunk_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            crate::embeddings::vec_to_int8_blob(&second),
            "the second insert must overwrite the first (last-write-wins)"
        );
    }

    /// The same duplicate-`chunk_id` sequence inside a single explicit
    /// transaction (mirroring a batch embed that flushes many rows under one
    /// `BEGIN`) must also collapse to one last-write-wins row.
    #[test]
    fn insert_embedding_duplicate_chunk_id_within_one_transaction_last_write_wins() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let dim = crate::embeddings::EMBEDDING_DIM;

        let mut a = vec![0f32; dim];
        a[1] = 1.0;
        let mut b = vec![0f32; dim];
        b[2] = 1.0;
        let mut c = vec![0f32; dim];
        c[3] = 1.0;

        {
            let tx = db.conn.unchecked_transaction().unwrap();
            db.insert_embedding(7, &a).unwrap();
            db.insert_embedding(7, &b).unwrap();
            db.insert_embedding(7, &c).unwrap();
            tx.commit().unwrap();
        }

        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE chunk_id = 7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "duplicate chunk_ids in one batch collapse to one row"
        );

        let stored: Vec<u8> = db
            .conn
            .query_row(
                "SELECT embedding FROM embeddings WHERE chunk_id = 7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            crate::embeddings::vec_to_int8_blob(&c),
            "the last write in the batch must win"
        );
    }

    /// Replacing a `chunk_id` that has never been inserted must be a harmless
    /// no-op DELETE followed by a normal INSERT — not an error. This is the
    /// overwhelmingly common real-world call pattern (indexing a chunk for the
    /// first time), so it must not regress under the delete-then-insert fix.
    #[test]
    fn insert_embedding_of_nonexistent_chunk_id_is_a_harmless_delete_no_op() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let dim = crate::embeddings::EMBEDDING_DIM;

        let mut vector = vec![0f32; dim];
        vector[3] = 1.0;

        db.insert_embedding(42, &vector)
            .expect("inserting a never-before-seen chunk_id must succeed");

        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE chunk_id = 42",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "the first insert for a fresh id must land exactly once"
        );

        let stored: Vec<u8> = db
            .conn
            .query_row(
                "SELECT embedding FROM embeddings WHERE chunk_id = 42",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, crate::embeddings::vec_to_int8_blob(&vector));
    }

    /// The strongest test of "joins the existing transaction" vs. "just happens
    /// not to error": call `insert_embedding` for a repeated `chunk_id` from
    /// WITHIN a transaction the caller already opened, then roll that outer
    /// transaction back. If the delete+insert genuinely joined the caller's
    /// transaction (rather than, say, silently nesting a SAVEPOINT that
    /// commits independently), rolling back the outer transaction must undo
    /// both the delete and the insert, restoring the pre-transaction row
    /// exactly.
    #[test]
    fn insert_embedding_joins_callers_transaction_and_rolls_back_with_it() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let dim = crate::embeddings::EMBEDDING_DIM;

        let mut first = vec![0f32; dim];
        first[0] = 1.0;
        db.insert_embedding(1, &first)
            .expect("seed row (autocommit)");

        let mut second = vec![0f32; dim];
        second[1] = 1.0;

        {
            let tx = db
                .conn
                .unchecked_transaction()
                .expect("caller opens an outer transaction");
            assert!(
                !db.conn.is_autocommit(),
                "precondition: connection must be mid-transaction, exercising the \
                 is_autocommit() guard's join branch rather than its own-BEGIN branch"
            );

            // Must not attempt a nested BEGIN (vec0/SQLite would reject it) —
            // simply not erroring here already covers that. The real test is
            // below: did it join *this* transaction, or silently commit on its
            // own?
            db.insert_embedding(1, &second)
                .expect("replacing inside the caller's open transaction must not nest a BEGIN");

            tx.rollback().expect("roll back the outer transaction");
        }

        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE chunk_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "rollback must not leave the row deleted — the DELETE half of the \
             replace was part of the outer transaction and must roll back with it"
        );

        let stored: Vec<u8> = db
            .conn
            .query_row(
                "SELECT embedding FROM embeddings WHERE chunk_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            crate::embeddings::vec_to_int8_blob(&first),
            "rollback must restore the pre-transaction (first) vector — if the \
             delete+insert had committed independently of the caller's \
             transaction, the row would still hold `second` here"
        );
    }

    /// The `embeddings` table runs in WAL mode (`Database::open`). A repeated
    /// `chunk_id` replace is delete-then-insert; if those two statements were
    /// not wrapped in one atomic transaction, a concurrent reader (e.g. a
    /// search query racing an index refresh) could observe a window with zero
    /// rows for that id between the DELETE committing and the INSERT
    /// committing. Drive many replaces on one connection while a second,
    /// independent connection continuously polls the row count, and assert
    /// the reader never observes zero.
    #[test]
    fn insert_embedding_replace_has_no_zero_row_window_visible_to_a_concurrent_reader() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        let db = Database::open(&path).expect("open file-backed db (WAL mode)");
        let dim = crate::embeddings::EMBEDDING_DIM;

        let mut seed = vec![0f32; dim];
        seed[0] = 1.0;
        db.insert_embedding(1, &seed).expect("seed row");

        let reader = Connection::open(&path).expect("independent reader connection");
        reader
            .execute_batch("PRAGMA busy_timeout = 5000;")
            .expect("reader busy timeout");

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saw_zero = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let iterations_observed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop_reader = stop.clone();
        let saw_zero_reader = saw_zero.clone();
        let iterations_reader = iterations_observed.clone();

        let reader_thread = std::thread::spawn(move || {
            while !stop_reader.load(std::sync::atomic::Ordering::Relaxed) {
                let count: i64 = reader
                    .query_row(
                        "SELECT COUNT(*) FROM embeddings WHERE chunk_id = 1",
                        [],
                        |r| r.get(0),
                    )
                    .expect("reader query must not error under WAL");
                iterations_reader.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count == 0 {
                    saw_zero_reader.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });

        let mut v = vec![0f32; dim];
        for i in 0..500 {
            v[i % dim] = 1.0;
            db.insert_embedding(1, &v).expect("replace");
        }

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        reader_thread.join().expect("reader thread must not panic");

        assert!(
            iterations_observed.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "sanity check: the reader must actually have raced the writer"
        );
        assert!(
            !saw_zero.load(std::sync::atomic::Ordering::Relaxed),
            "a concurrent WAL reader must never observe zero rows for chunk_id=1 \
             mid-replace — the delete+insert must commit atomically as one \
             transaction, not as two independently-visible statements"
        );
    }
}
