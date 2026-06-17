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

use a2w_engine::{RunResult, RunStatus, StepEvent};
use a2w_ir::Workflow;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;
use thiserror::Error;

pub use resolver::StoreCredentialResolver;
pub use vault::Vault;

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
        // A single connection is required for `sqlite::memory:` (each new
        // connection is its own empty DB). It is harmless for file URLs, so we
        // apply it unconditionally for a consistent, race-free schema setup.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(url)
            .await?;
        let store = Store { pool };
        store.init().await?;
        Ok(store)
    }

    /// Create the schema if it does not already exist.
    async fn init(&self) -> Result<(), StoreError> {
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
        .execute(&self.pool)
        .await?;
        Ok(())
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
        sqlx::query("DELETE FROM workflows WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ----------------------------------------------------------------------
    // Run history
    // ----------------------------------------------------------------------

    /// Persist a run's outcome: its id, owning workflow, status, and the
    /// serialized step-event stream.
    ///
    /// # Errors
    /// [`StoreError::Serde`] if the events cannot be serialized, or
    /// [`StoreError::Sqlx`] on a write failure.
    pub async fn save_run(
        &self,
        workflow_id: &str,
        result: &RunResult,
    ) -> Result<(), StoreError> {
        let status = run_status_str(result.status);
        let events_json = serde_json::to_string(&result.events)?;
        let created_at = now_unix_seconds();
        sqlx::query(
            "INSERT INTO runs (run_id, workflow_id, status, events_json, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(run_id) DO UPDATE SET \
               workflow_id = excluded.workflow_id, \
               status = excluded.status, \
               events_json = excluded.events_json",
        )
        .bind(&result.run_id)
        .bind(workflow_id)
        .bind(status)
        .bind(&events_json)
        .bind(created_at)
        .execute(&self.pool)
        .await?;
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
        let row: Option<(String, String, String, String)> = sqlx::query_as(
            "SELECT run_id, workflow_id, status, events_json FROM runs WHERE run_id = ?1",
        )
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some((run_id, workflow_id, status, events_json)) => {
                let events: Vec<StepEvent> = serde_json::from_str(&events_json)?;
                Ok(Some(StoredRun {
                    run_id,
                    workflow_id,
                    status,
                    events,
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
            listed.iter().any(|(id, name)| id == &wf.id && name == &wf.name),
            "listing must contain the saved workflow"
        );

        store.delete_workflow(&wf.id).await.expect("delete workflow");
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

        store
            .save_run(&wf.id, &result)
            .await
            .expect("save run");

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

        let missing = vault
            .get_secret(&store, "nope")
            .await
            .expect("get missing");
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
