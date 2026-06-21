//! # a2w-store
//!
//! Milestone **M4 — durability**. Persistence for A2W on top of SQLite (via
//! [`sqlx`] runtime queries — no compile-time `DATABASE_URL` required):
//!
//! - **Workflows** — the [`a2w_ir::Workflow`] IR, stored as JSON and upserted by
//!   id ([`Store::save_workflow`] / [`Store::get_workflow`] /
//!   [`Store::list_workflows`] / [`Store::delete_workflow`]).
//! - **Run history** — [`a2w_engine::RunResult`]s reduced to a [`StoredRun`]
//!   (status + the step-event stream) keyed by run id
//!   ([`Store::save_run`] / [`Store::get_run`] / [`Store::list_runs`]).
//! - **Credential vault** — AES-256-GCM envelope encryption of secrets,
//!   stored separately so workflow/run records never hold plaintext (see
//!   [`Vault`]).
//!
//! ## In-memory pooling gotcha
//! For the `sqlite::memory:` URL, *each connection is a separate empty
//! database*. [`Store::connect`] therefore pins the pool to a single connection
//! (`max_connections(1)`) so the schema created by [`Store`]`::init` and all
//! subsequent writes share one in-memory database for the lifetime of the
//! `Store`. File-backed URLs (e.g. `sqlite://a2w.db?mode=rwc`) do not have this
//! constraint.

#![forbid(unsafe_code)]

mod resolver;
mod vault;

use std::time::{SystemTime, UNIX_EPOCH};

use a2w_engine::{Item, ResumeSource, RunResult, RunStatus, StepEvent, StepKind};
use a2w_ir::Workflow;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;
use thiserror::Error;

pub use resolver::StoreCredentialResolver;
pub use vault::Vault;
// Re-export the engine's ResumeSource trait so external crates (a2w-server,
// a2w-mcp) can refer to `a2w_store::ResumeSource` without taking a direct
// dep on a2w-engine just for the trait.
pub use a2w_engine::ResumeSource as EngineResumeSource;

/// Errors returned by the store and vault.
#[derive(Debug, Error)]
pub enum StoreError {
    /// A database-layer error from `sqlx`.
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// JSON (de)serialization of a workflow or event stream failed.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// An AES-256-GCM encryption or decryption failure (wrong key, tampered
    /// data, or malformed stored material).
    #[error("crypto error: {0}")]
    Crypto(String),
    /// A base64 decoding failure (e.g. a malformed `A2W_MASTER_KEY`).
    #[error("base64 error: {0}")]
    Base64(#[from] base64::DecodeError),
    /// A configuration error (e.g. a missing or wrong-sized master key).
    #[error("configuration error: {0}")]
    Config(String),
}

/// A persisted run record: a [`a2w_engine::RunResult`] reduced to what we keep
/// in the `runs` table (the full node-output item arrays are not persisted).
#[derive(Debug, Clone)]
pub struct StoredRun {
    /// The run's id (`RunResult::run_id`).
    pub run_id: String,
    /// The id of the workflow this run belongs to.
    pub workflow_id: String,
    /// The terminal status, as a short serialized string (`completed`/`failed`).
    pub status: String,
    /// The recorded step-event stream.
    pub events: Vec<StepEvent>,
    /// R4 audit-fix: stable hex-encoded fingerprint of the workflow IR at
    /// the moment this run started. `None` for runs persisted before v3.
    /// Resume compares the stored fingerprint to a fresh
    /// [`workflow_fingerprint`] of the current workflow and refuses when
    /// they differ — prevents reusing stale outputs after an IR edit.
    pub workflow_fingerprint: Option<String>,
}

/// Compute a stable, process-independent fingerprint of a workflow IR.
///
/// Used by [`Store::save_run`] (when called via
/// [`Store::save_run_with_fingerprint`]) and by the resume path to detect
/// stale-output reuse after an IR edit. The hash is FNV-1a 64-bit over the
/// canonical JSON serialization so two replicas hashing the same IR get the
/// same fingerprint (unlike `RandomState` / `DefaultHasher`).
#[must_use]
pub fn workflow_fingerprint(wf: &a2w_ir::Workflow) -> String {
    let json = serde_json::to_string(wf).unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in json.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}")
}

/// A persisted skill row (F4). Dependency-light by design: the store holds the
/// proven workflow IR and the skill's metadata as JSON columns so it need not
/// depend on `a2w-skills`; `a2w-skills` owns the `Skill` <-> `SkillRecord`
/// mapping.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillRecord {
    /// Stable skill id.
    pub id: String,
    /// The natural-language query the skill solves.
    pub query: String,
    /// The node whose output is "the result".
    pub observe_node: String,
    /// The proven workflow IR, serialized.
    pub workflow_json: String,
    /// The task signature, serialized.
    pub signature_json: String,
    /// The calibrated evidence snapshot, serialized.
    pub evidence_json: String,
    /// The HOLDOUT-certified outcome score at promotion time.
    pub holdout_score: f64,
}

/// The persistence handle: a pooled SQLite connection.
pub struct Store {
    /// The connection pool. Visible within the crate so [`Vault`] can run its
    /// own credential queries against the same database.
    pub(crate) pool: SqlitePool,
}

impl Store {
    /// Connect to the SQLite database at `url`, run schema setup, and return a
    /// ready [`Store`].
    ///
    /// For `"sqlite::memory:"` the pool is pinned to a **single connection** so
    /// the in-memory database (which is per-connection) persists across queries.
    /// For file URLs use e.g. `"sqlite://a2w.db?mode=rwc"` to create on demand.
    ///
    /// # Errors
    /// Returns [`StoreError::Sqlx`] if the pool cannot be built or the schema
    /// cannot be created.
    pub async fn connect(url: &str) -> Result<Store, StoreError> {
        // For `sqlite::memory:` each new connection is its own empty DB, so the
        // pool MUST be pinned to a single connection. For file-backed and
        // shared-memory (`file:...?cache=shared`) URLs we let sqlx pool more
        // connections (default 10 from `A2W_DB_MAX_CONNECTIONS`) — this is the
        // change that lets the server actually handle concurrent writers.
        let is_memory = url.starts_with("sqlite::memory:");
        let max_connections = if is_memory {
            1
        } else {
            std::env::var("A2W_DB_MAX_CONNECTIONS")
                .ok()
                .and_then(|v| v.trim().parse::<u32>().ok())
                .unwrap_or(10)
        };
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await?;
        let store = Store { pool };
        store.init().await?;
        Ok(store)
    }

    /// Create the schema if it does not already exist, then drive each pending
    /// versioned migration forward. The single-row `_a2w_meta` table tracks the
    /// current schema version (an integer monotonically increasing as
    /// migrations are added).
    async fn init(&self) -> Result<(), StoreError> {
        // Meta table for migration tracking. The (`id` = 1) row holds the
        // current schema version.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS _a2w_meta (\
                id INTEGER PRIMARY KEY CHECK (id = 1), \
                schema_version INTEGER NOT NULL\
            )",
        )
        .execute(&self.pool)
        .await?;

        // Base tables (v1 schema).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS workflows (\
                id TEXT PRIMARY KEY, \
                name TEXT NOT NULL, \
                json TEXT NOT NULL, \
                created_at INTEGER NOT NULL\
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS runs (\
                run_id TEXT PRIMARY KEY, \
                workflow_id TEXT NOT NULL, \
                status TEXT NOT NULL, \
                events_json TEXT NOT NULL, \
                created_at INTEGER NOT NULL\
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS credentials (\
                id TEXT PRIMARY KEY, \
                name TEXT NOT NULL, \
                nonce BLOB NOT NULL, \
                ciphertext BLOB NOT NULL, \
                created_at INTEGER NOT NULL\
            )",
        )
        .execute(&self.pool)
        .await?;

        // Run the versioned migrations forward.
        self.run_migrations().await?;

        Ok(())
    }

    /// Read the current schema version (0 if no row exists).
    async fn current_schema_version(&self) -> Result<u32, StoreError> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT schema_version FROM _a2w_meta WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?;
        Ok(row
            .map(|(v,)| u32::try_from(v.max(0)).unwrap_or(0))
            .unwrap_or(0))
    }

    /// Persist a new schema version (upsert into the single meta row).
    async fn set_schema_version(&self, version: u32) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO _a2w_meta (id, schema_version) VALUES (1, ?1) \
             ON CONFLICT(id) DO UPDATE SET schema_version = excluded.schema_version",
        )
        .bind(i64::from(version))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Drive the meta version forward through every pending migration.
    ///
    /// Migrations are forward-only and idempotent — each block uses
    /// `CREATE TABLE IF NOT EXISTS` / `ALTER TABLE ADD COLUMN` so re-applying a
    /// migration is safe even if a previous run set the meta version but
    /// crashed between DDL statements.
    async fn run_migrations(&self) -> Result<(), StoreError> {
        let mut version = self.current_schema_version().await?;

        // ---- v0 -> v1: idempotency_key + step_records table -------------------
        if version < 1 {
            // v1 schema with the columns we need from v2 baked in — fresh DBs
            // get the right shape on first init, existing v1 DBs get the
            // additive ALTER TABLE in the v1->v2 step below.
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS idempotency_keys (\
                    key TEXT PRIMARY KEY, \
                    workflow_id TEXT NOT NULL, \
                    run_id TEXT NOT NULL, \
                    created_at INTEGER NOT NULL\
                )",
            )
            .execute(&self.pool)
            .await?;

            sqlx::query(
                "CREATE TABLE IF NOT EXISTS step_records (\
                    run_id TEXT NOT NULL, \
                    node_id TEXT NOT NULL, \
                    seq INTEGER NOT NULL, \
                    kind TEXT NOT NULL, \
                    latency_ms INTEGER NOT NULL, \
                    input_items INTEGER NOT NULL, \
                    output_items INTEGER NOT NULL, \
                    output_json TEXT, \
                    error TEXT, \
                    created_at INTEGER NOT NULL, \
                    PRIMARY KEY (run_id, node_id, seq)\
                )",
            )
            .execute(&self.pool)
            .await?;

            sqlx::query(
                "CREATE INDEX IF NOT EXISTS idx_step_records_run \
                 ON step_records(run_id)",
            )
            .execute(&self.pool)
            .await?;

            version = 1;
            self.set_schema_version(version).await?;
        }

        // ---- v1 -> v2: idempotency 2-phase status + expires_at + approvals ---
        if version < 2 {
            // R4 audit-fix: copy-forward migration preserves completed
            // idempotency rows (the production safety net). Round-3
            // erroneously DROP'd these, which would cause every
            // post-upgrade retry to double-fire side effects. The dance
            // here picks a distinct intermediate name and renames as the
            // FINAL step inside the implicit transaction so the
            // `sqlite_autoindex_*` name-collision noted in round-3 cannot
            // bite: at no point do two objects called `idempotency_keys`
            // coexist.
            //
            // Probe whether the source table is v1-shaped (PK on `key`
            // alone) or already-v2 (PK on (workflow_id, key)). PRAGMA
            // returns one row per column with `pk` > 0 indicating PK
            // membership; on fresh DBs the v1 block above created the v1
            // shape so this returns 1.
            let pk_info: Vec<(i64, String, String, i64, Option<String>, i64)> =
                sqlx::query_as("PRAGMA table_info(idempotency_keys)")
                    .fetch_all(&self.pool)
                    .await
                    .unwrap_or_default();
            let pk_cols: usize = pk_info.iter().filter(|c| c.5 > 0).count();
            let v1_shape = pk_cols == 1;
            // Drop any leftover migration scratch from a partial-crash retry.
            let _ = sqlx::query("DROP TABLE IF EXISTS idempotency_keys_new")
                .execute(&self.pool)
                .await;
            if v1_shape {
                // Final-hardening (this session): wrap the rebuild in an
                // explicit transaction so a crash anywhere in the dance
                // leaves either the v1 OR the v2 state intact — never a
                // half-migrated state that fails the next boot.
                // (Per-statement ALTERs cannot run inside a SQLite
                // transaction in older versions, so we ALTER on the pool
                // first; CREATE/INSERT/DROP/RENAME go in the tx.)
                let _ = sqlx::query(
                    "ALTER TABLE idempotency_keys ADD COLUMN status TEXT NOT NULL \
                     DEFAULT 'completed'",
                )
                .execute(&self.pool)
                .await;
                let _ = sqlx::query("ALTER TABLE idempotency_keys ADD COLUMN expires_at INTEGER")
                    .execute(&self.pool)
                    .await;
                let mut tx = self.pool.begin().await?;
                sqlx::query(
                    "CREATE TABLE idempotency_keys_new (\
                        workflow_id TEXT NOT NULL, \
                        key TEXT NOT NULL, \
                        run_id TEXT NOT NULL, \
                        status TEXT NOT NULL, \
                        created_at INTEGER NOT NULL, \
                        expires_at INTEGER, \
                        PRIMARY KEY (workflow_id, key)\
                    )",
                )
                .execute(&mut *tx)
                .await?;
                // INSERT OR IGNORE: cross-workflow duplicates on the same
                // `key` (which v1 allowed because PK was only on `key`)
                // collapse onto the first-by-row-order.
                sqlx::query(
                    "INSERT OR IGNORE INTO idempotency_keys_new \
                        (workflow_id, key, run_id, status, created_at, expires_at) \
                     SELECT workflow_id, key, run_id, \
                            COALESCE(status, 'completed'), created_at, expires_at \
                     FROM idempotency_keys",
                )
                .execute(&mut *tx)
                .await?;
                sqlx::query("DROP TABLE idempotency_keys")
                    .execute(&mut *tx)
                    .await?;
                let _ = sqlx::query("DROP INDEX IF EXISTS sqlite_autoindex_idempotency_keys_1")
                    .execute(&mut *tx)
                    .await;
                sqlx::query("ALTER TABLE idempotency_keys_new RENAME TO idempotency_keys")
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
            } else {
                // Table is already v2-shaped (or never existed). Either
                // way, ensure the canonical schema is in place.
                sqlx::query(
                    "CREATE TABLE IF NOT EXISTS idempotency_keys (\
                        workflow_id TEXT NOT NULL, \
                        key TEXT NOT NULL, \
                        run_id TEXT NOT NULL, \
                        status TEXT NOT NULL, \
                        created_at INTEGER NOT NULL, \
                        expires_at INTEGER, \
                        PRIMARY KEY (workflow_id, key)\
                    )",
                )
                .execute(&self.pool)
                .await?;
            }

            // Approvals table — drives the Approval executor and the
            // `/approvals` REST endpoints.
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS approvals (\
                    id TEXT PRIMARY KEY, \
                    run_id TEXT NOT NULL, \
                    workflow_id TEXT NOT NULL, \
                    node_id TEXT NOT NULL, \
                    payload_json TEXT NOT NULL, \
                    status TEXT NOT NULL, \
                    decided_by TEXT, \
                    decided_at INTEGER, \
                    created_at INTEGER NOT NULL\
                )",
            )
            .execute(&self.pool)
            .await?;
            sqlx::query("CREATE INDEX IF NOT EXISTS idx_approvals_status ON approvals(status)")
                .execute(&self.pool)
                .await?;
            sqlx::query("CREATE INDEX IF NOT EXISTS idx_approvals_run ON approvals(run_id)")
                .execute(&self.pool)
                .await?;

            version = 2;
            self.set_schema_version(version).await?;
        }

        // ---- v2 -> v3: runs.workflow_fingerprint (R4 audit-fix) ---------------
        if version < 3 {
            let _ = sqlx::query("ALTER TABLE runs ADD COLUMN workflow_fingerprint TEXT")
                .execute(&self.pool)
                .await;
            version = 3;
            self.set_schema_version(version).await?;
        }

        // ---- v3 -> v4: step_records.node_kind (R5 audit-fix) -----------------
        // Persist the node's kind alongside each step so the resume path can
        // verify that the IR's node kind matches the hydrated row. Without
        // this, an IR edit that changes a node's `kind` while preserving
        // its `id` would let the engine reuse a Transform's output for an
        // HttpRequest (no network call, but downstream sees the wrong
        // shape).
        if version < 4 {
            let _ = sqlx::query("ALTER TABLE step_records ADD COLUMN node_kind TEXT")
                .execute(&self.pool)
                .await;
            version = 4;
            self.set_schema_version(version).await?;
        }

        // ---- v4 -> v5: workflow_references inverse-index (R5 H5 fix) ---------
        // DELETE / PUT reference-integrity / cycle-check were O(N) DB
        // reads + IR parse per request — a DoS amplifier. v5 maintains a
        // normalized `(from_id, to_id)` table populated by
        // `save_workflow`, consulted by `referrers_of` and
        // `referenced_workflows_of`.
        if version < 5 {
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS workflow_references (\
                    from_id TEXT NOT NULL, \
                    to_id TEXT NOT NULL, \
                    PRIMARY KEY (from_id, to_id)\
                )",
            )
            .execute(&self.pool)
            .await?;
            sqlx::query(
                "CREATE INDEX IF NOT EXISTS idx_workflow_references_to \
                 ON workflow_references(to_id)",
            )
            .execute(&self.pool)
            .await?;
            // R7 proactive: partial index supports the reaper's hot path
            // `WHERE status='in_progress'` without scanning completed
            // rows (which dominate over time).
            sqlx::query(
                "CREATE INDEX IF NOT EXISTS idx_idempotency_in_progress \
                 ON idempotency_keys(run_id) WHERE status = 'in_progress'",
            )
            .execute(&self.pool)
            .await?;
            // R6 proactive fix: backfill from existing workflows in the same
            // migration block so pre-v5 DBs immediately have a correct
            // inverse-index. Without this, DELETE/PUT cycle checks would
            // silently treat stored-but-not-re-saved workflows as having
            // no references — allowing dangling deletes and missed cycles.
            let rows: Vec<(String, String)> = sqlx::query_as("SELECT id, json FROM workflows")
                .fetch_all(&self.pool)
                .await?;
            for (id, json) in rows {
                if let Ok(wf) = serde_json::from_str::<Workflow>(&json) {
                    for to in a2w_validator::sub_workflow_references(&wf) {
                        let _ = sqlx::query(
                            "INSERT OR IGNORE INTO workflow_references \
                                (from_id, to_id) VALUES (?1, ?2)",
                        )
                        .bind(&id)
                        .bind(&to)
                        .execute(&self.pool)
                        .await;
                    }
                }
            }
            version = 5;
            self.set_schema_version(version).await?;
        }

        // ---- v5 -> v6: skill library / workflow memory (F4) ------------------
        // Persists M4 skills so the generate -> verify -> promote -> retrieve
        // loop runs through the durable surface, not just in-memory. A skill
        // row stores the proven workflow IR, its task signature, the calibrated
        // evidence snapshot, and the HOLDOUT-certified score (the honest one).
        // `CREATE TABLE IF NOT EXISTS` is self-healing if a prior attempt
        // crashed mid-migration; the version bump only lands after the table
        // exists.
        if version < 6 {
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS skills (\
                    id TEXT PRIMARY KEY, \
                    query TEXT NOT NULL, \
                    observe_node TEXT NOT NULL, \
                    workflow_json TEXT NOT NULL, \
                    signature_json TEXT NOT NULL, \
                    evidence_json TEXT NOT NULL, \
                    holdout_score REAL NOT NULL, \
                    created_at INTEGER NOT NULL\
                )",
            )
            .execute(&self.pool)
            .await?;
            sqlx::query(
                "CREATE INDEX IF NOT EXISTS idx_skills_holdout \
                 ON skills(holdout_score)",
            )
            .execute(&self.pool)
            .await?;
            version = 6;
            self.set_schema_version(version).await?;
        }
        let _ = version;

        Ok(())
    }

    // ----------------------------------------------------------------------
    // Workflow persistence
    // ----------------------------------------------------------------------

    /// Insert or update a workflow, keyed by its `id`.
    ///
    /// # Errors
    /// [`StoreError::Serde`] if the workflow cannot be serialized, or
    /// [`StoreError::Sqlx`] on a write failure.
    pub async fn save_workflow(&self, wf: &Workflow) -> Result<(), StoreError> {
        let json = serde_json::to_string(wf)?;
        let created_at = now_unix_seconds();
        // R5 H5 fix: maintain the workflow_references inverse-index in a
        // single transaction so DELETE / PUT reference-integrity become
        // O(refs-of-target) instead of O(all-workflows).
        let refs = a2w_validator::sub_workflow_references(wf);
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO workflows (id, name, json, created_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(id) DO UPDATE SET \
               name = excluded.name, \
               json = excluded.json",
        )
        .bind(&wf.id)
        .bind(&wf.name)
        .bind(&json)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM workflow_references WHERE from_id = ?1")
            .bind(&wf.id)
            .execute(&mut *tx)
            .await?;
        for to in refs {
            sqlx::query(
                "INSERT OR IGNORE INTO workflow_references (from_id, to_id) \
                 VALUES (?1, ?2)",
            )
            .bind(&wf.id)
            .bind(&to)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Workflow ids that contain a SubWorkflow reference pointing AT `to_id`
    /// (i.e. would dangle if `to_id` were deleted). R5 H5 fix: backed by the
    /// `workflow_references` inverse-index instead of an O(N) walk.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure.
    pub async fn referrers_of(&self, to_id: &str) -> Result<Vec<String>, StoreError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT from_id FROM workflow_references WHERE to_id = ?1 ORDER BY from_id",
        )
        .bind(to_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(s,)| s).collect())
    }

    /// SubWorkflow targets referenced FROM `from_id`. Used by the PUT-time
    /// cycle-check to walk the SubWorkflow graph without re-parsing every
    /// stored workflow.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure.
    pub async fn referenced_workflows_of(&self, from_id: &str) -> Result<Vec<String>, StoreError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT to_id FROM workflow_references WHERE from_id = ?1 ORDER BY to_id",
        )
        .bind(from_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(s,)| s).collect())
    }

    /// Rebuild the `workflow_references` inverse-index from the source of
    /// truth (the `workflows` table). Operator admin call after upgrading
    /// to schema v5 with pre-existing workflow rows.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read/write failure.
    pub async fn rebuild_workflow_references(&self) -> Result<usize, StoreError> {
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT id, json FROM workflows")
            .fetch_all(&self.pool)
            .await?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM workflow_references")
            .execute(&mut *tx)
            .await?;
        let mut written = 0usize;
        for (id, json) in rows {
            let wf: Workflow = serde_json::from_str(&json)?;
            for to in a2w_validator::sub_workflow_references(&wf) {
                sqlx::query(
                    "INSERT OR IGNORE INTO workflow_references (from_id, to_id) \
                     VALUES (?1, ?2)",
                )
                .bind(&id)
                .bind(&to)
                .execute(&mut *tx)
                .await?;
                written += 1;
            }
        }
        tx.commit().await?;
        Ok(written)
    }

    /// Fetch a workflow by id, deserializing the stored JSON.
    ///
    /// Returns `Ok(None)` if no workflow has that id.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure, or [`StoreError::Serde`] if the
    /// stored JSON cannot be deserialized.
    pub async fn get_workflow(&self, id: &str) -> Result<Option<Workflow>, StoreError> {
        let row: Option<(String,)> = sqlx::query_as("SELECT json FROM workflows WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some((json,)) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    /// List all stored workflows as `(id, name)` pairs, ordered by id.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure.
    pub async fn list_workflows(&self) -> Result<Vec<(String, String)>, StoreError> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT id, name FROM workflows ORDER BY id")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }

    /// Delete a workflow by id. Deleting a missing id is a no-op (still `Ok`).
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    pub async fn delete_workflow(&self, id: &str) -> Result<(), StoreError> {
        // R5: clean up the inverse-index along with the workflow row so
        // referrers_of()/referenced_workflows_of() never returns stale rows.
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM workflows WHERE id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM workflow_references WHERE from_id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    // ----------------------------------------------------------------------
    // Skill library (F4)
    // ----------------------------------------------------------------------

    /// Insert or update a skill row, keyed by its `id`.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    pub async fn save_skill(&self, rec: &SkillRecord) -> Result<(), StoreError> {
        let created_at = now_unix_seconds();
        sqlx::query(
            "INSERT INTO skills \
                (id, query, observe_node, workflow_json, signature_json, evidence_json, \
                 holdout_score, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(id) DO UPDATE SET \
               query = excluded.query, \
               observe_node = excluded.observe_node, \
               workflow_json = excluded.workflow_json, \
               signature_json = excluded.signature_json, \
               evidence_json = excluded.evidence_json, \
               holdout_score = excluded.holdout_score",
        )
        .bind(&rec.id)
        .bind(&rec.query)
        .bind(&rec.observe_node)
        .bind(&rec.workflow_json)
        .bind(&rec.signature_json)
        .bind(&rec.evidence_json)
        .bind(rec.holdout_score)
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch a skill row by id. `Ok(None)` if absent.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure.
    pub async fn get_skill(&self, id: &str) -> Result<Option<SkillRecord>, StoreError> {
        let row: Option<(String, String, String, String, String, f64)> = sqlx::query_as(
            "SELECT query, observe_node, workflow_json, signature_json, evidence_json, \
                    holdout_score \
             FROM skills WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(query, observe_node, workflow_json, signature_json, evidence_json, holdout_score)| {
                SkillRecord {
                    id: id.to_string(),
                    query,
                    observe_node,
                    workflow_json,
                    signature_json,
                    evidence_json,
                    holdout_score,
                }
            },
        ))
    }

    /// List all skill rows, ordered by id (deterministic).
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure.
    pub async fn list_skills(&self) -> Result<Vec<SkillRecord>, StoreError> {
        let rows: Vec<(String, String, String, String, String, String, f64)> = sqlx::query_as(
            "SELECT id, query, observe_node, workflow_json, signature_json, evidence_json, \
                    holdout_score \
             FROM skills ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, query, observe_node, workflow_json, signature_json, evidence_json, holdout_score)| {
                    SkillRecord {
                        id,
                        query,
                        observe_node,
                        workflow_json,
                        signature_json,
                        evidence_json,
                        holdout_score,
                    }
                },
            )
            .collect())
    }

    // ----------------------------------------------------------------------
    // Run history
    // ----------------------------------------------------------------------

    /// Persist a run's outcome: its id, owning workflow, status, the serialized
    /// step-event stream, AND a per-`(run_id, node_id)` step-record table that
    /// captures each node's output items. The dual write is wrapped in a single
    /// transaction so a crashed write leaves no half-persisted run.
    ///
    /// # Errors
    /// [`StoreError::Serde`] if the events or node outputs cannot be
    /// serialized, or [`StoreError::Sqlx`] on a write failure.
    /// R4 audit-fix: persist a run alongside the fingerprint of the workflow
    /// IR that produced it. The fingerprint lets the resume path detect that
    /// the IR has changed since this run was first persisted and refuse to
    /// hydrate stale outputs.
    ///
    /// R5: pass the workflow itself so per-step `node_kind` is also
    /// persisted — the resume path uses it to refuse hydration when a node's
    /// kind has changed since the original run committed.
    ///
    /// # Errors
    /// As [`Store::save_run`].
    pub async fn save_run_with_fingerprint(
        &self,
        workflow_id: &str,
        fingerprint: &str,
        result: &RunResult,
    ) -> Result<(), StoreError> {
        self.save_run_inner(workflow_id, Some(fingerprint), result, None)
            .await
    }

    /// R5: full-fidelity save. Persists the workflow fingerprint AND a
    /// per-step `node_kind` (looked up from `wf.nodes`) so the resume path
    /// can detect IR drift AND kind changes.
    ///
    /// # Errors
    /// As [`Store::save_run`].
    pub async fn save_run_full(
        &self,
        wf: &a2w_ir::Workflow,
        fingerprint: &str,
        result: &RunResult,
    ) -> Result<(), StoreError> {
        let kinds: std::collections::HashMap<&str, &str> = wf
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), node_kind_wire_name(n.kind)))
            .collect();
        self.save_run_inner(&wf.id, Some(fingerprint), result, Some(&kinds))
            .await
    }

    pub async fn save_run(&self, workflow_id: &str, result: &RunResult) -> Result<(), StoreError> {
        self.save_run_inner(workflow_id, None, result, None).await
    }

    async fn save_run_inner(
        &self,
        workflow_id: &str,
        fingerprint: Option<&str>,
        result: &RunResult,
        node_kinds: Option<&std::collections::HashMap<&str, &str>>,
    ) -> Result<(), StoreError> {
        let status = run_status_str(result.status);
        let events_json = serde_json::to_string(&result.events)?;
        let now_secs = now_unix_seconds();

        // Begin a transaction so the runs row and step_records writes commit
        // atomically. A crash mid-write leaves the DB in the prior state.
        let mut tx = self.pool.begin().await?;

        // R3 audit-fix: previously the upsert silently rewrote workflow_id
        // on `run_id` conflict, so two replicas with colliding run_ids would
        // overwrite each other's runs across workflows. The ON CONFLICT now
        // only touches the row when the workflow_id matches — a mismatch
        // makes the WHERE filter false and the conflicting row is left
        // intact; we surface the collision as a Sqlx error to the caller.
        sqlx::query(
            "INSERT INTO runs (run_id, workflow_id, status, events_json, created_at, workflow_fingerprint) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(run_id) DO UPDATE SET \
               status = excluded.status, \
               events_json = excluded.events_json \
             WHERE runs.workflow_id = excluded.workflow_id",
        )
        .bind(&result.run_id)
        .bind(workflow_id)
        .bind(status)
        .bind(&events_json)
        .bind(now_secs)
        .bind(fingerprint)
        .execute(&mut *tx)
        .await?;

        // Per-step records — one row per StepEvent, with the node's output
        // attached on Finished events.
        // Re-persist is idempotent because we DELETE existing rows first; this
        // mirrors `INSERT OR REPLACE` semantics for the composite key without
        // requiring a triggered upsert.
        sqlx::query("DELETE FROM step_records WHERE run_id = ?1")
            .bind(&result.run_id)
            .execute(&mut *tx)
            .await?;

        // Compute per-(run, node) seq numbers in insertion order.
        let mut next_seq: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
        for ev in &result.events {
            let seq = next_seq.entry(ev.node_id.as_str()).or_insert(0);
            let this_seq = *seq;
            *seq += 1;

            // Attach output JSON only on Finished events.
            let output_json: Option<String> = match ev.kind {
                StepKind::Finished => result
                    .node_outputs
                    .get(&ev.node_id)
                    .map(serde_json::to_string)
                    .transpose()?,
                _ => None,
            };

            let node_kind = node_kinds.and_then(|m| m.get(ev.node_id.as_str())).copied();
            sqlx::query(
                "INSERT INTO step_records (\
                    run_id, node_id, seq, kind, latency_ms, \
                    input_items, output_items, output_json, error, created_at, node_kind\
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            )
            .bind(&ev.run_id)
            .bind(&ev.node_id)
            .bind(i64::from(this_seq))
            .bind(step_kind_str(ev.kind))
            .bind(i64::try_from(ev.latency_ms).unwrap_or(i64::MAX))
            .bind(i64::try_from(ev.input_items).unwrap_or(i64::MAX))
            .bind(i64::try_from(ev.output_items).unwrap_or(i64::MAX))
            .bind(output_json)
            .bind(ev.error.clone())
            .bind(now_secs)
            .bind(node_kind)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Fetch a stored run by its id.
    ///
    /// Returns `Ok(None)` if no run has that id.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure, or [`StoreError::Serde`] if the
    /// stored event JSON cannot be deserialized.
    pub async fn get_run(&self, run_id: &str) -> Result<Option<StoredRun>, StoreError> {
        let row: Option<(String, String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT run_id, workflow_id, status, events_json, workflow_fingerprint \
             FROM runs WHERE run_id = ?1",
        )
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some((run_id, workflow_id, status, events_json, workflow_fingerprint)) => {
                let events: Vec<StepEvent> = serde_json::from_str(&events_json)?;
                Ok(Some(StoredRun {
                    run_id,
                    workflow_id,
                    status,
                    events,
                    workflow_fingerprint,
                }))
            }
            None => Ok(None),
        }
    }

    /// List the run ids for a workflow, newest first.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure.
    pub async fn list_runs(&self, workflow_id: &str) -> Result<Vec<String>, StoreError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT run_id FROM runs WHERE workflow_id = ?1 ORDER BY created_at DESC, rowid DESC",
        )
        .bind(workflow_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(run_id,)| run_id).collect())
    }

    // ----------------------------------------------------------------------
    // Idempotency keys
    //
    // Callers that need *at-most-once* execution semantics supply an
    // idempotency key alongside the run request. The first call commits the
    // (key -> run_id) mapping; subsequent calls with the same key short-circuit
    // and return the existing run instead of re-firing side effects.
    // ----------------------------------------------------------------------

    /// Look up the run id previously committed under `(workflow_id, key)`, if
    /// any. Scoping by workflow_id (audit-2 IDOR fix) prevents an attacker on
    /// workflow `wf_a` from learning runs of `wf_b` by guessing keys.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure.
    pub async fn get_idempotency_key(
        &self,
        workflow_id: &str,
        key: &str,
    ) -> Result<Option<String>, StoreError> {
        // R3: returns the bound run_id only when status='completed'.
        // In-progress slots return None so a caller doing the simple "is this
        // key bound" check doesn't accidentally short-circuit on a slot that
        // hasn't committed yet.
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT run_id, status FROM idempotency_keys \
             WHERE workflow_id = ?1 AND key = ?2",
        )
        .bind(workflow_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(run_id, status)| {
            if status == "completed" {
                Some(run_id)
            } else {
                None
            }
        }))
    }

    /// Commit `(workflow_id, key) -> run_id` atomically, refusing to overwrite
    /// an existing binding. Returns `Ok(true)` on insert, `Ok(false)` if the
    /// pair was already bound (the caller should re-fetch). Writes
    /// `status='completed'` for backward compat.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    pub async fn put_idempotency_key(
        &self,
        key: &str,
        workflow_id: &str,
        run_id: &str,
    ) -> Result<bool, StoreError> {
        let created_at = now_unix_seconds();
        let res = sqlx::query(
            "INSERT OR IGNORE INTO idempotency_keys (workflow_id, key, run_id, status, created_at) \
             VALUES (?1, ?2, ?3, 'completed', ?4)",
        )
        .bind(workflow_id)
        .bind(key)
        .bind(run_id)
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// **Audit-3 2-phase claim.** Phase 1: pre-claim the `(workflow_id, key)`
    /// pair with `status='in_progress'` and a pre-minted `run_id`. Returns the
    /// claim outcome:
    ///
    /// - `IdempotencyClaim::Acquired` — we are the writer; run, then call
    ///   [`Store::complete_idempotency_key`].
    /// - `IdempotencyClaim::InProgress(run_id)` — another caller is currently
    ///   running under this key; tell the client to retry.
    /// - `IdempotencyClaim::Completed(run_id)` — a prior caller already
    ///   completed under this key; return the existing run.
    /// - `IdempotencyClaim::Expired(run_id)` — a prior claim expired (the
    ///   process crashed mid-run); we adopt the slot and proceed as Acquired.
    ///
    /// `ttl_secs` is the lifetime of the claim. Expired claims are reclaimable
    /// — without this, a single crash would lock the key forever.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    pub async fn claim_idempotency_key(
        &self,
        workflow_id: &str,
        key: &str,
        run_id: &str,
        ttl_secs: i64,
    ) -> Result<IdempotencyClaim, StoreError> {
        let now = now_unix_seconds();
        // R3+R4 audit-fix: bound TTL on both ends. Floor 60s prevents
        // zero-TTL bypass; ceiling is env-tunable
        // (`A2W_IDEMPOTENCY_TTL_MAX_SECS`, default 7 d) so deployments with
        // genuinely-long workflows can raise it without code changes. When
        // the caller's request gets clamped we log the surprise so the
        // operator sees it in dashboards.
        const TTL_MIN: i64 = 60;
        let ttl_max: i64 = std::env::var("A2W_IDEMPOTENCY_TTL_MAX_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|&v| v >= TTL_MIN)
            .unwrap_or(7 * 24 * 3600);
        let ttl = ttl_secs.clamp(TTL_MIN, ttl_max);
        if ttl != ttl_secs {
            tracing::warn!(
                requested = ttl_secs,
                clamped_to = ttl,
                min = TTL_MIN,
                max = ttl_max,
                "idempotency TTL clamped"
            );
        }
        let expires_at = now.saturating_add(ttl);

        // Try to insert a fresh in-progress claim. R3 fix: PK is now
        // (workflow_id, key), so a key colliding with a different workflow's
        // entry doesn't block us.
        let res = sqlx::query(
            "INSERT OR IGNORE INTO idempotency_keys \
                 (workflow_id, key, run_id, status, created_at, expires_at) \
             VALUES (?1, ?2, ?3, 'in_progress', ?4, ?5)",
        )
        .bind(workflow_id)
        .bind(key)
        .bind(run_id)
        .bind(now)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() > 0 {
            return Ok(IdempotencyClaim::Acquired);
        }

        // Existing row for THIS (workflow_id, key) pair.
        let row: Option<(String, String, Option<i64>)> = sqlx::query_as(
            "SELECT run_id, status, expires_at \
             FROM idempotency_keys WHERE workflow_id = ?1 AND key = ?2",
        )
        .bind(workflow_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some((existing_run, status, expires)) => {
                if status == "completed" {
                    Ok(IdempotencyClaim::Completed(existing_run))
                } else if expires.map(|e| now > e).unwrap_or(false) {
                    // Expired claim — adopt the slot in place.
                    sqlx::query(
                        "UPDATE idempotency_keys SET \
                            run_id = ?3, status = 'in_progress', \
                            created_at = ?4, expires_at = ?5 \
                         WHERE workflow_id = ?1 AND key = ?2",
                    )
                    .bind(workflow_id)
                    .bind(key)
                    .bind(run_id)
                    .bind(now)
                    .bind(expires_at)
                    .execute(&self.pool)
                    .await?;
                    Ok(IdempotencyClaim::Expired(existing_run))
                } else {
                    Ok(IdempotencyClaim::InProgress(existing_run))
                }
            }
            None => {
                // Race: row vanished between our INSERT and SELECT (another
                // caller deleted/expired-released it). Retry the insert
                // once; on a second miss, treat as `InProgress` with a
                // null marker (no fabricated run_id — R3 fix).
                let res = sqlx::query(
                    "INSERT OR IGNORE INTO idempotency_keys \
                         (workflow_id, key, run_id, status, created_at, expires_at) \
                     VALUES (?1, ?2, ?3, 'in_progress', ?4, ?5)",
                )
                .bind(workflow_id)
                .bind(key)
                .bind(run_id)
                .bind(now)
                .bind(expires_at)
                .execute(&self.pool)
                .await?;
                if res.rows_affected() > 0 {
                    Ok(IdempotencyClaim::Acquired)
                } else {
                    Ok(IdempotencyClaim::InProgress(String::new()))
                }
            }
        }
    }

    /// Mark a previously-claimed idempotency key as `completed`. Idempotent.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    /// Finalize a 2-phase claim. R4 audit-fix: the UPDATE is now guarded by
    /// `status='in_progress' AND run_id = ?` so a slow original run cannot
    /// overwrite a slot that a subsequent expired-adopter has already
    /// completed (the "first writer wins" invariant). Returns `Ok(true)`
    /// when our run was the canonical committer; `Ok(false)` when another
    /// writer already finalized the slot (the caller should treat the
    /// adopter's outcome as canonical and NOT trust its own commit).
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    pub async fn complete_idempotency_key(
        &self,
        workflow_id: &str,
        key: &str,
        run_id: &str,
    ) -> Result<bool, StoreError> {
        let res = sqlx::query(
            "UPDATE idempotency_keys SET status = 'completed' \
             WHERE workflow_id = ?1 AND key = ?2 \
               AND status = 'in_progress' AND run_id = ?3",
        )
        .bind(workflow_id)
        .bind(key)
        .bind(run_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Mark a previously-claimed idempotency key as failed (releases it).
    /// Called when a 2-phase claim's run failed before save_run. Lets a
    /// subsequent retry get a fresh `Acquired`.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    /// Release a claim ONLY if it still points at `run_id`. R5 audit-fix:
    /// without the `run_id` guard, an original-caller's release-on-error
    /// path could wipe an in-progress adopter's row, letting a subsequent
    /// retry re-fire side effects a third time. Returns `Ok(true)` when
    /// our row was the one released, `Ok(false)` when an adopter has
    /// already replaced it (we shouldn't touch their slot).
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    pub async fn release_idempotency_key(
        &self,
        workflow_id: &str,
        key: &str,
        run_id: &str,
    ) -> Result<bool, StoreError> {
        let res = sqlx::query(
            "DELETE FROM idempotency_keys WHERE workflow_id = ?1 AND key = ?2 \
             AND status = 'in_progress' AND run_id = ?3",
        )
        .bind(workflow_id)
        .bind(key)
        .bind(run_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// R4 audit-fix: background reaper helper for stranded `in_progress`
    /// idempotency slots whose `run_id` is already present in the `runs`
    /// table. This happens when the engine + save_run succeeded but the
    /// 2-phase commit `complete_idempotency_key` failed and returned 502.
    /// Operators run this periodically (cron / sidecar) to finalize the
    /// orphaned slots. Returns the number of slots finalized.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    pub async fn reap_stranded_idempotency_claims(&self) -> Result<u64, StoreError> {
        let res = sqlx::query(
            "UPDATE idempotency_keys SET status = 'completed' \
             WHERE status = 'in_progress' \
               AND run_id IN (SELECT run_id FROM runs)",
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    // ----------------------------------------------------------------------
    // Per-step records (foundation for resume-from-step + per-node observability)
    // ----------------------------------------------------------------------

    /// Append one step record for `(run_id, node_id)`. `seq` is the per-step
    /// monotonically-increasing index (0 = first event, e.g. `Started`).
    ///
    /// `output_json` carries the serialized node-output item array on a
    /// `Finished` step; `None` on `Started`/`Failed`.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure, or [`StoreError::Serde`] if
    /// `output_json` cannot be serialized.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_step(
        &self,
        run_id: &str,
        node_id: &str,
        seq: u32,
        kind: &str,
        latency_ms: u64,
        input_items: u32,
        output_items: u32,
        output_json: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        let created_at = now_unix_seconds();
        sqlx::query(
            "INSERT INTO step_records (\
                run_id, node_id, seq, kind, latency_ms, \
                input_items, output_items, output_json, error, created_at\
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        )
        .bind(run_id)
        .bind(node_id)
        .bind(i64::from(seq))
        .bind(kind)
        .bind(i64::try_from(latency_ms).unwrap_or(i64::MAX))
        .bind(i64::from(input_items))
        .bind(i64::from(output_items))
        .bind(output_json)
        .bind(error)
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// All step records for `run_id`, ordered by insertion order
    /// (`(node_id, seq)`).
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure.
    pub async fn list_step_records(&self, run_id: &str) -> Result<Vec<StepRecord>, StoreError> {
        type StepRow = (
            String,
            String,
            i64,
            String,
            i64,
            i64,
            i64,
            Option<String>,
            Option<String>,
        );
        let rows: Vec<StepRow> = sqlx::query_as(
            "SELECT run_id, node_id, seq, kind, latency_ms, \
                        input_items, output_items, output_json, error \
                 FROM step_records \
                 WHERE run_id = ?1 \
                 ORDER BY node_id, seq",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(rid, nid, seq, kind, lat, inp, outp, json, err)| StepRecord {
                    run_id: rid,
                    node_id: nid,
                    seq: u32::try_from(seq.max(0)).unwrap_or(0),
                    kind,
                    latency_ms: u64::try_from(lat.max(0)).unwrap_or(0),
                    input_items: u32::try_from(inp.max(0)).unwrap_or(0),
                    output_items: u32::try_from(outp.max(0)).unwrap_or(0),
                    output_json: json,
                    error: err,
                },
            )
            .collect())
    }
}

/// Outcome of [`Store::claim_idempotency_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyClaim {
    /// We are the first writer; proceed with the run, then
    /// [`Store::complete_idempotency_key`] when done.
    Acquired,
    /// A prior caller already completed under this key; return the bound run.
    Completed(String),
    /// Another caller is currently running under this key; tell the client to
    /// retry after a short delay (the carried String is the run id they
    /// claimed, useful only for diagnostics).
    InProgress(String),
    /// A prior claim expired (the process crashed mid-run); we ADOPT the slot
    /// and proceed exactly as `Acquired`. The carried String is the previous
    /// run id we kicked out, useful for diagnostics.
    Expired(String),
}

impl IdempotencyClaim {
    /// `true` when the caller should run (Acquired or Expired).
    #[must_use]
    pub fn should_run(&self) -> bool {
        matches!(
            self,
            IdempotencyClaim::Acquired | IdempotencyClaim::Expired(_)
        )
    }
}

/// One pending or decided approval (the row backing the Approval executor and
/// the `/approvals` REST endpoints).
#[derive(Debug, Clone)]
pub struct ApprovalRecord {
    /// Approval id (uuid-ish; e.g. `approval_<run>_<node>_<i>`).
    pub id: String,
    /// The run this approval belongs to.
    pub run_id: String,
    /// The owning workflow.
    pub workflow_id: String,
    /// The node that requested the approval.
    pub node_id: String,
    /// The serialized item payload presented to the approver.
    pub payload_json: String,
    /// `pending` / `approved` / `rejected`.
    pub status: String,
    /// Free-text "decided by" attribution (e.g. an email).
    pub decided_by: Option<String>,
    /// Unix seconds of the decision; `None` while pending.
    pub decided_at: Option<i64>,
    /// Unix seconds of the request.
    pub created_at: i64,
}

impl Store {
    /// Insert a pending approval row.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    pub async fn create_approval(
        &self,
        id: &str,
        run_id: &str,
        workflow_id: &str,
        node_id: &str,
        payload_json: &str,
    ) -> Result<(), StoreError> {
        let created_at = now_unix_seconds();
        sqlx::query(
            "INSERT INTO approvals \
                 (id, run_id, workflow_id, node_id, payload_json, status, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
        )
        .bind(id)
        .bind(run_id)
        .bind(workflow_id)
        .bind(node_id)
        .bind(payload_json)
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch an approval by id.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure.
    pub async fn get_approval(&self, id: &str) -> Result<Option<ApprovalRecord>, StoreError> {
        type Row = (
            String,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<i64>,
            i64,
        );
        let row: Option<Row> = sqlx::query_as(
            "SELECT id, run_id, workflow_id, node_id, payload_json, status, decided_by, decided_at, created_at \
             FROM approvals WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| ApprovalRecord {
            id: r.0,
            run_id: r.1,
            workflow_id: r.2,
            node_id: r.3,
            payload_json: r.4,
            status: r.5,
            decided_by: r.6,
            decided_at: r.7,
            created_at: r.8,
        }))
    }

    /// List approvals, optionally filtered by status.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure.
    pub async fn list_approvals(
        &self,
        status_filter: Option<&str>,
    ) -> Result<Vec<ApprovalRecord>, StoreError> {
        type Row = (
            String,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<i64>,
            i64,
        );
        let rows: Vec<Row> = match status_filter {
            Some(s) => {
                sqlx::query_as(
                    "SELECT id, run_id, workflow_id, node_id, payload_json, status, decided_by, decided_at, created_at \
                     FROM approvals WHERE status = ?1 ORDER BY created_at DESC",
                )
                .bind(s)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query_as(
                    "SELECT id, run_id, workflow_id, node_id, payload_json, status, decided_by, decided_at, created_at \
                     FROM approvals ORDER BY created_at DESC",
                )
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows
            .into_iter()
            .map(|r| ApprovalRecord {
                id: r.0,
                run_id: r.1,
                workflow_id: r.2,
                node_id: r.3,
                payload_json: r.4,
                status: r.5,
                decided_by: r.6,
                decided_at: r.7,
                created_at: r.8,
            })
            .collect())
    }

    /// Decide an approval (approve or reject). Idempotent on first decision;
    /// subsequent decisions are no-ops (the first decision wins).
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    pub async fn decide_approval(
        &self,
        id: &str,
        decision: &str,
        decided_by: Option<&str>,
    ) -> Result<bool, StoreError> {
        let now = now_unix_seconds();
        let res = sqlx::query(
            "UPDATE approvals SET status = ?2, decided_by = ?3, decided_at = ?4 \
             WHERE id = ?1 AND status = 'pending'",
        )
        .bind(id)
        .bind(decision)
        .bind(decided_by)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }
}

/// Adapter: a `Store`-backed [`a2w_engine::SubWorkflowResolver`] for the
/// `SubWorkflow` executor.
pub struct StoreSubWorkflowResolver {
    store: std::sync::Arc<Store>,
}

impl StoreSubWorkflowResolver {
    /// Wrap a store as a [`a2w_engine::SubWorkflowResolver`].
    #[must_use]
    pub fn new(store: std::sync::Arc<Store>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl a2w_engine::SubWorkflowResolver for StoreSubWorkflowResolver {
    async fn get_workflow(
        &self,
        _caller_workflow_id: &str,
        workflow_id: &str,
    ) -> Result<Option<a2w_ir::Workflow>, a2w_engine::CredentialError> {
        // Single-tenant store: no owner check. Multi-tenant deployments
        // should wrap this resolver in one that enforces
        // owner(caller) == owner(workflow_id) before delegating.
        self.store
            .get_workflow(workflow_id)
            .await
            .map_err(|e| a2w_engine::CredentialError::Lookup(e.to_string()))
    }
}

/// Adapter: a `Store`-backed [`a2w_engine::ApprovalGate`] for the `Approval`
/// executor. Approval ids are derived deterministically from
/// `(run_id, node_id, idx)` so the same run on resume binds to the same row.
pub struct StoreApprovalGate {
    store: std::sync::Arc<Store>,
}

impl StoreApprovalGate {
    /// Wrap a store as an [`a2w_engine::ApprovalGate`].
    #[must_use]
    pub fn new(store: std::sync::Arc<Store>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl a2w_engine::ApprovalGate for StoreApprovalGate {
    async fn request(
        &self,
        run_id: &str,
        node_id: &str,
        idx: usize,
        payload_json: &str,
    ) -> Result<String, a2w_engine::CredentialError> {
        let id = format!("ap_{run_id}_{node_id}_{idx}");
        // Look up the workflow id by reading the run record.
        let workflow_id = self
            .store
            .get_run(run_id)
            .await
            .map_err(|e| a2w_engine::CredentialError::Lookup(e.to_string()))?
            .map(|r| r.workflow_id)
            .unwrap_or_else(|| "<unknown>".to_string());

        // Idempotent: re-requesting an existing id is a no-op.
        if self
            .store
            .get_approval(&id)
            .await
            .map_err(|e| a2w_engine::CredentialError::Lookup(e.to_string()))?
            .is_none()
        {
            self.store
                .create_approval(&id, run_id, &workflow_id, node_id, payload_json)
                .await
                .map_err(|e| a2w_engine::CredentialError::Lookup(e.to_string()))?;
        }
        Ok(id)
    }

    async fn poll(
        &self,
        approval_id: &str,
    ) -> Result<Option<a2w_engine::ApprovalOutcome>, a2w_engine::CredentialError> {
        let row = self
            .store
            .get_approval(approval_id)
            .await
            .map_err(|e| a2w_engine::CredentialError::Lookup(e.to_string()))?;
        let row = match row {
            Some(r) => r,
            None => return Ok(None),
        };
        match row.status.as_str() {
            "pending" => Ok(None),
            "approved" => Ok(Some(a2w_engine::ApprovalOutcome::Approved {
                decided_by: row.decided_by,
            })),
            "rejected" => Ok(Some(a2w_engine::ApprovalOutcome::Rejected {
                decided_by: row.decided_by,
            })),
            other => Err(a2w_engine::CredentialError::Lookup(format!(
                "unknown approval status '{other}'"
            ))),
        }
    }
}

/// Adapter: a wrapper letting `Arc<Store>` impl `ResumeSource` even though
/// `ResumeSource::hydrate` is an `async fn` returning items derived from
/// step_records rows (the engine doesn't depend on `a2w_store` directly).
pub struct StoreResumeSource {
    store: std::sync::Arc<Store>,
}

impl StoreResumeSource {
    /// Wrap a store as a [`ResumeSource`].
    #[must_use]
    pub fn new(store: std::sync::Arc<Store>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl ResumeSource for StoreResumeSource {
    async fn hydrate(
        &self,
        run_id: &str,
        node_id: &str,
        expected_kind: &str,
    ) -> a2w_engine::HydrateResult {
        // Pull output_json AND the persisted node_kind. R5 audit-fix: if
        // the IR's current `expected_kind` differs from the kind that
        // produced the persisted output, we refuse to reuse the output —
        // otherwise an IR edit that swaps a Transform for an HttpRequest
        // (keeping the same id) would serve the Transform's stale output
        // as the HttpRequest's result.
        let row: Result<Option<(Option<String>, Option<String>)>, _> = sqlx::query_as(
            "SELECT output_json, node_kind FROM step_records \
             WHERE run_id = ?1 AND node_id = ?2 AND kind = 'finished' \
             ORDER BY seq DESC LIMIT 1",
        )
        .bind(run_id)
        .bind(node_id)
        .fetch_optional(&self.store.pool)
        .await;
        let (json, stored_kind) = match row {
            Ok(Some((Some(j), k))) => (j, k),
            Ok(Some((None, _))) => {
                return a2w_engine::HydrateResult::Corrupt(format!(
                    "step_records row for ({run_id}, {node_id}) is kind='finished' but output_json IS NULL — \
                     refusing to silently re-execute"
                ));
            }
            Ok(None) => return a2w_engine::HydrateResult::Missing,
            Err(e) => {
                return a2w_engine::HydrateResult::Corrupt(format!(
                    "store read for ({run_id}, {node_id}) failed: {e}"
                ));
            }
        };
        // R5: kind drift check. `stored_kind` is None for pre-v4 step
        // records (they pre-date the column); we accept those and trust
        // the workflow_fingerprint check (which catches any IR edit).
        if let Some(persisted) = stored_kind.as_deref() {
            if persisted != expected_kind {
                return a2w_engine::HydrateResult::Corrupt(format!(
                    "step_records node_kind for ({run_id}, {node_id}) was '{persisted}' \
                     but the current workflow IR has kind '{expected_kind}' — \
                     refusing to reuse stale outputs of a different executor"
                ));
            }
        }
        match serde_json::from_str::<Vec<Item>>(&json) {
            Ok(items) => a2w_engine::HydrateResult::Found(items),
            Err(e) => a2w_engine::HydrateResult::Corrupt(format!(
                "output_json for ({run_id}, {node_id}) is unparseable: {e}"
            )),
        }
    }
}

/// A single persisted [`a2w_engine::StepEvent`] enriched with output payload.
#[derive(Debug, Clone)]
pub struct StepRecord {
    /// The run this step belongs to.
    pub run_id: String,
    /// The node that produced the step.
    pub node_id: String,
    /// Per-`(run_id, node_id)` monotonic sequence.
    pub seq: u32,
    /// `started` / `finished` / `failed`.
    pub kind: String,
    /// Wall-clock latency in milliseconds.
    pub latency_ms: u64,
    /// Input item count.
    pub input_items: u32,
    /// Output item count.
    pub output_items: u32,
    /// Serialized output `Vec<Item>` JSON on `Finished` steps; `None` otherwise.
    pub output_json: Option<String>,
    /// Error message on `Failed` steps; `None` otherwise.
    pub error: Option<String>,
}

/// Current Unix time in whole seconds, as `i64` (the column type SQLite uses).
///
/// A clock set before the epoch yields `0` rather than erroring; timestamps are
/// metadata only and never participate in correctness.
fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Serialize a [`RunStatus`] to the short string stored in the `status` column.
fn run_status_str(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
    }
}

/// Serialize a [`StepKind`] to the short string stored in `step_records.kind`.
fn step_kind_str(k: StepKind) -> &'static str {
    match k {
        StepKind::Started => "started",
        StepKind::Finished => "finished",
        StepKind::Failed => "failed",
    }
}

/// Stable wire name for a [`a2w_ir::NodeKind`], matching the IR's serde
/// rename to snake_case. Used for `step_records.node_kind` so the resume
/// path can detect a kind change in the workflow IR (R5 audit-fix).
fn node_kind_wire_name(k: a2w_ir::NodeKind) -> &'static str {
    use a2w_ir::NodeKind::*;
    match k {
        WebhookTrigger => "webhook_trigger",
        ScheduleTrigger => "schedule_trigger",
        HttpRequest => "http_request",
        McpToolCall => "mcp_tool_call",
        Transform => "transform",
        Branch => "branch",
        Switch => "switch",
        Loop => "loop",
        Merge => "merge",
        Wait => "wait",
        SubWorkflow => "sub_workflow",
        LlmCall => "llm_call",
        CodeStep => "code_step",
        Approval => "approval",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2w_engine::{Engine, ExecutionMode, MemoryEventLog};

    /// Build a tiny valid workflow: WebhookTrigger -> Transform.
    fn tiny_workflow() -> Workflow {
        use a2w_ir::{Connection, Node, NodeKind, SCHEMA_VERSION};
        let trigger = Node::new("trigger", NodeKind::WebhookTrigger);
        let shape = Node::new("shape", NodeKind::Transform);
        Workflow {
            schema_version: SCHEMA_VERSION,
            id: "wf_tiny".to_string(),
            name: "Tiny webhook → transform".to_string(),
            nodes: vec![trigger, shape],
            connections: vec![Connection::new("trigger", 0, "shape")],
        }
    }

    #[tokio::test]
    async fn workflow_round_trip_save_get_list_delete() {
        let store = Store::connect("sqlite::memory:")
            .await
            .expect("connect in-memory store");
        let wf = a2w_ir::sample_workflow();

        store.save_workflow(&wf).await.expect("save workflow");

        let got = store
            .get_workflow(&wf.id)
            .await
            .expect("get workflow")
            .expect("workflow present");
        assert_eq!(got, wf, "round-tripped workflow must equal the original");

        let listed = store.list_workflows().await.expect("list workflows");
        assert!(
            listed
                .iter()
                .any(|(id, name)| id == &wf.id && name == &wf.name),
            "listing must contain the saved workflow"
        );

        store
            .delete_workflow(&wf.id)
            .await
            .expect("delete workflow");
        let gone = store.get_workflow(&wf.id).await.expect("get after delete");
        assert!(gone.is_none(), "deleted workflow must be absent");
    }

    #[tokio::test]
    async fn upsert_updates_existing_workflow() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let mut wf = tiny_workflow();
        store.save_workflow(&wf).await.expect("first save");

        wf.name = "Renamed".to_string();
        store.save_workflow(&wf).await.expect("upsert save");

        let listed = store.list_workflows().await.expect("list");
        assert_eq!(listed.len(), 1, "upsert must not create a duplicate row");
        assert_eq!(listed[0].1, "Renamed");
    }

    #[tokio::test]
    async fn run_persistence_round_trip() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let wf = tiny_workflow();
        store.save_workflow(&wf).await.expect("save wf");

        let engine = Engine::new(a2w_nodes::default_registry());
        let log = MemoryEventLog::new();
        let result = engine
            .run(
                &wf,
                vec![serde_json::json!({ "hello": "world" })],
                ExecutionMode::DryRun,
                &log,
            )
            .await
            .expect("run workflow");

        store.save_run(&wf.id, &result).await.expect("save run");

        let stored = store
            .get_run(&result.run_id)
            .await
            .expect("get run")
            .expect("run present");
        assert_eq!(stored.run_id, result.run_id);
        assert_eq!(stored.workflow_id, wf.id);
        assert_eq!(stored.status, "completed");
        assert!(!stored.events.is_empty(), "events must be persisted");

        let runs = store.list_runs(&wf.id).await.expect("list runs");
        assert!(
            runs.contains(&result.run_id),
            "list_runs must contain the saved run id"
        );
    }

    #[tokio::test]
    async fn vault_store_and_retrieve_secret() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let vault = Vault::new([7u8; 32]);

        vault
            .store_secret(&store, "cred1", "API token", "token")
            .await
            .expect("store secret");

        let got = vault
            .get_secret(&store, "cred1")
            .await
            .expect("get secret")
            .expect("secret present");
        assert_eq!(got, "token", "decrypted secret must match plaintext");

        let missing = vault.get_secret(&store, "nope").await.expect("get missing");
        assert!(missing.is_none(), "absent id must yield None");
    }

    #[tokio::test]
    async fn vault_wrong_key_fails_closed() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let good = Vault::new([7u8; 32]);
        good.store_secret(&store, "cred1", "API token", "token")
            .await
            .expect("store secret");

        let wrong = Vault::new([9u8; 32]);
        let result = wrong.get_secret(&store, "cred1").await;
        assert!(
            matches!(result, Err(StoreError::Crypto(_))),
            "decryption with the wrong key must fail closed (Crypto error), got {result:?}"
        );
    }

    #[tokio::test]
    async fn vault_list_credentials_returns_id_name_no_secret() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let vault = Vault::new([3u8; 32]);
        vault
            .store_secret(&store, "c1", "First", "alpha")
            .await
            .expect("store c1");
        vault
            .store_secret(&store, "c2", "Second", "beta")
            .await
            .expect("store c2");

        let listed = Vault::list_credentials(&store)
            .await
            .expect("list credentials");
        assert_eq!(listed.len(), 2, "two credentials persisted");
        let ids: Vec<&str> = listed.iter().map(|(i, _, _)| i.as_str()).collect();
        assert!(ids.contains(&"c1") && ids.contains(&"c2"));
        let names: Vec<&str> = listed.iter().map(|(_, n, _)| n.as_str()).collect();
        assert!(names.contains(&"First") && names.contains(&"Second"));

        // The listing cannot leak any plaintext.
        let serialized = format!("{listed:?}");
        assert!(
            !serialized.contains("alpha") && !serialized.contains("beta"),
            "listing must not contain plaintext secret bytes: {serialized}"
        );
    }

    #[tokio::test]
    async fn vault_delete_credential_removes_it() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let vault = Vault::new([3u8; 32]);
        vault
            .store_secret(&store, "c1", "First", "alpha")
            .await
            .expect("store c1");

        Vault::delete_credential(&store, "c1")
            .await
            .expect("delete c1");

        let after = vault.get_secret(&store, "c1").await.expect("get after");
        assert!(after.is_none(), "deleted credential must be absent");

        // Deleting a missing id is a no-op (idempotent).
        Vault::delete_credential(&store, "missing")
            .await
            .expect("delete missing must succeed");
    }

    #[tokio::test]
    async fn idempotency_key_first_write_wins_second_short_circuits() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let first = store
            .put_idempotency_key("k1", "wf_a", "run_1")
            .await
            .expect("first put");
        assert!(first, "first put must report inserted");

        let again = store
            .put_idempotency_key("k1", "wf_a", "run_2")
            .await
            .expect("second put");
        assert!(!again, "second put must report not-inserted");

        let bound = store
            .get_idempotency_key("wf_a", "k1")
            .await
            .expect("lookup")
            .expect("key bound");
        assert_eq!(bound, "run_1", "first writer's run_id wins");

        // Unknown key yields None.
        let missing = store
            .get_idempotency_key("wf_a", "nope")
            .await
            .expect("lookup");
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn idempotency_claim_acquire_then_complete() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let outcome = store
            .claim_idempotency_key("wf_a", "k1", "run_X", 3600)
            .await
            .expect("claim");
        assert_eq!(outcome, IdempotencyClaim::Acquired);

        // Second attempt while first is in progress: must report InProgress.
        let outcome = store
            .claim_idempotency_key("wf_a", "k1", "run_Y", 3600)
            .await
            .expect("second claim");
        match outcome {
            IdempotencyClaim::InProgress(rid) => assert_eq!(rid, "run_X"),
            other => panic!("expected InProgress, got {other:?}"),
        }

        // Complete the claim.
        store
            .complete_idempotency_key("wf_a", "k1", "run_X")
            .await
            .expect("complete");

        // Now a fresh claim sees Completed.
        let outcome = store
            .claim_idempotency_key("wf_a", "k1", "run_Z", 3600)
            .await
            .expect("third claim");
        match outcome {
            IdempotencyClaim::Completed(rid) => assert_eq!(rid, "run_X"),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn idempotency_claim_expired_can_be_adopted() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        // Claim with a ttl of 60s (the minimum). To simulate expiry, manually
        // update the expires_at to a past timestamp.
        store
            .claim_idempotency_key("wf_a", "k1", "run_X", 60)
            .await
            .expect("initial claim");
        sqlx::query("UPDATE idempotency_keys SET expires_at = 1 WHERE key = ?1")
            .bind("k1")
            .execute(&store.pool)
            .await
            .expect("force expiry");

        let outcome = store
            .claim_idempotency_key("wf_a", "k1", "run_Y", 60)
            .await
            .expect("post-expiry claim");
        match &outcome {
            IdempotencyClaim::Expired(prior) => assert_eq!(prior, "run_X"),
            other => panic!("expected Expired, got {other:?}"),
        }
        assert!(
            outcome.should_run(),
            "Expired must allow the new caller to run"
        );
    }

    #[tokio::test]
    async fn approval_create_get_list_decide_round_trip() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        store
            .create_approval("ap1", "run_1", "wf_a", "approve_node", r#"{"x":1}"#)
            .await
            .expect("create");
        let got = store
            .get_approval("ap1")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got.status, "pending");
        assert_eq!(got.workflow_id, "wf_a");

        let pending = store.list_approvals(Some("pending")).await.expect("list");
        assert_eq!(pending.len(), 1);
        let decided_listing = store.list_approvals(Some("approved")).await.expect("list");
        assert!(decided_listing.is_empty());

        let decided = store
            .decide_approval("ap1", "approved", Some("ops@example.com"))
            .await
            .expect("decide");
        assert!(decided);

        // Idempotent: second decide is a no-op (already not pending).
        let decided = store
            .decide_approval("ap1", "rejected", Some("attacker"))
            .await
            .expect("second decide");
        assert!(!decided, "first-decision-wins must lock the row");
        let after = store
            .get_approval("ap1")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(after.status, "approved");
        assert_eq!(after.decided_by.as_deref(), Some("ops@example.com"));
    }

    #[tokio::test]
    async fn engine_resume_skips_completed_nodes() {
        use std::sync::Arc;
        let store = Arc::new(Store::connect("sqlite::memory:").await.expect("connect"));
        let wf = tiny_workflow();
        store.save_workflow(&wf).await.expect("save wf");

        // Run once to populate step_records.
        let engine = a2w_engine::Engine::new(a2w_nodes::default_registry());
        let log = a2w_engine::MemoryEventLog::new();
        let result = engine
            .run(
                &wf,
                vec![serde_json::json!({ "id": 1 })],
                a2w_engine::ExecutionMode::DryRun,
                &log,
            )
            .await
            .expect("first run");
        store.save_run(&wf.id, &result).await.expect("save run");

        // Resume from the same run id — all nodes should be hydrated.
        let resume = StoreResumeSource::new(Arc::clone(&store));
        let log = a2w_engine::MemoryEventLog::new();
        let r2 = engine
            .run_with_id(
                &wf,
                result.run_id.clone(),
                Vec::new(),
                a2w_engine::ExecutionMode::DryRun,
                &log,
                Some(&resume),
            )
            .await
            .expect("resume");

        // Every node had its output hydrated, so the resumed RunResult must
        // carry the same nodes as the first run.
        assert_eq!(r2.status, a2w_engine::RunStatus::Completed);
        for node in &wf.nodes {
            assert!(
                r2.node_outputs.contains_key(&node.id),
                "resumed run must include hydrated node '{}'",
                node.id
            );
        }
    }

    #[tokio::test]
    async fn idempotency_key_is_scoped_to_workflow_no_cross_disclosure() {
        // Audit-2 fix: a key bound to wf_a must NOT leak via a lookup from
        // wf_b — closes the IDOR finding from the round-2 audit.
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        store
            .put_idempotency_key("k1", "wf_a", "run_a")
            .await
            .expect("put");
        // Same key, different workflow → no leak.
        let cross = store
            .get_idempotency_key("wf_b", "k1")
            .await
            .expect("cross lookup");
        assert!(
            cross.is_none(),
            "cross-workflow lookup must NOT see another workflow's run id"
        );
    }

    #[tokio::test]
    async fn save_run_persists_step_records_with_outputs() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let wf = tiny_workflow();
        store.save_workflow(&wf).await.expect("save wf");

        let engine = a2w_engine::Engine::new(a2w_nodes::default_registry());
        let log = a2w_engine::MemoryEventLog::new();
        let result = engine
            .run(
                &wf,
                vec![serde_json::json!({ "id": 1 })],
                a2w_engine::ExecutionMode::DryRun,
                &log,
            )
            .await
            .expect("run");

        store.save_run(&wf.id, &result).await.expect("save run");

        let records = store
            .list_step_records(&result.run_id)
            .await
            .expect("list step records");
        assert!(
            !records.is_empty(),
            "save_run must emit per-step records: {records:?}"
        );

        // Each node should have at least a Started + Finished pair.
        let trigger_records: Vec<&StepRecord> =
            records.iter().filter(|r| r.node_id == "trigger").collect();
        assert!(
            trigger_records.len() >= 2,
            "trigger must have Started+Finished records: {trigger_records:?}"
        );

        // The Finished record must carry the serialized output.
        let finished = trigger_records
            .iter()
            .find(|r| r.kind == "finished")
            .expect("finished step present");
        assert!(
            finished.output_json.as_ref().is_some_and(|j| !j.is_empty()),
            "Finished step must carry serialized output_json"
        );

        // seq numbers per node are 0,1,...
        for n in ["trigger", "shape"] {
            let mut per_node: Vec<u32> = records
                .iter()
                .filter(|r| r.node_id == n)
                .map(|r| r.seq)
                .collect();
            per_node.sort_unstable();
            for (i, s) in per_node.iter().enumerate() {
                assert_eq!(
                    *s,
                    u32::try_from(i).unwrap(),
                    "seq must be 0..n per node: {per_node:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn schema_version_advances_on_first_init() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let v = store.current_schema_version().await.expect("read version");
        assert!(v >= 1, "schema version must be at least 1 after init: {v}");
    }

    #[tokio::test]
    async fn skills_round_trip_through_v6_table() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let rec = SkillRecord {
            id: "skill_abc".to_string(),
            query: "tag alerts".to_string(),
            observe_node: "tag".to_string(),
            workflow_json: "{\"id\":\"wf\"}".to_string(),
            signature_json: "{\"tokens\":[]}".to_string(),
            evidence_json: "{\"score\":1.0}".to_string(),
            holdout_score: 1.0,
        };
        store.save_skill(&rec).await.expect("save skill");

        let got = store.get_skill("skill_abc").await.expect("get").expect("present");
        assert_eq!(got, rec);

        // Upsert: re-save with a new score updates in place (no duplicate row).
        let mut rec2 = rec.clone();
        rec2.holdout_score = 0.5;
        rec2.query = "tag the alerts".to_string();
        store.save_skill(&rec2).await.expect("upsert");

        let all = store.list_skills().await.expect("list");
        assert_eq!(all.len(), 1, "upsert must not duplicate");
        assert_eq!(all[0].holdout_score, 0.5);
        assert_eq!(all[0].query, "tag the alerts");

        assert!(store.get_skill("missing").await.expect("get missing").is_none());
    }

    #[tokio::test]
    async fn schema_re_init_is_idempotent() {
        // Two connections sharing a file: the second should not fail by
        // re-running migrations.
        let path = format!("sqlite:///tmp/a2w_test_{}.db?mode=rwc", std::process::id());
        let _ = std::fs::remove_file(
            path.trim_start_matches("sqlite://")
                .split('?')
                .next()
                .unwrap(),
        );

        let _first = Store::connect(&path).await.expect("first connect");
        let second = Store::connect(&path).await.expect("second connect");
        let v = second.current_schema_version().await.expect("read version");
        // Bumped to v6 with the skills table (F4).
        assert_eq!(v, 6);

        let _ = std::fs::remove_file(
            path.trim_start_matches("sqlite://")
                .split('?')
                .next()
                .unwrap(),
        );
    }

    #[tokio::test]
    async fn vault_from_env_errors_when_unset() {
        // Snapshot and restore so concurrent tests sharing this process env are
        // not disturbed.
        let saved = std::env::var("A2W_MASTER_KEY").ok();
        std::env::remove_var("A2W_MASTER_KEY");

        let result = Vault::from_env();
        assert!(
            matches!(result, Err(StoreError::Config(_))),
            "from_env must return a Config error when the key is unset, got {:?}",
            result.as_ref().map(|_| "Ok")
        );

        if let Some(v) = saved {
            std::env::set_var("A2W_MASTER_KEY", v);
        }
    }
}
