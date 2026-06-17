//! The node-executor abstraction: the trait every node kind implements, plus the
//! per-execution context and error type.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use a2w_ir::NodeKind;

use crate::item::Item;

/// How a node (and the whole run) should behave: a real run vs. a dry run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Execute for real, including side effects (HTTP requests, tool calls).
    Run,
    /// Validate the shape of the run without side effects: side-effecting nodes
    /// return mocked output instead of performing their action.
    DryRun,
}

/// Everything a node needs to know about the single execution it is performing.
///
/// A node's *behaviour* is keyed by [`NodeKind`] (one executor per kind in the
/// registry); its *configuration* arrives per-node here via `params`.
#[derive(Clone)]
pub struct NodeContext {
    /// The id of the current run.
    pub run_id: String,
    /// The id of the node being executed.
    pub node_id: String,
    /// The kind of the node being executed.
    pub kind: NodeKind,
    /// The node's `params` from the IR (untyped JSON for M2).
    pub params: serde_json::Value,
    /// Whether this is a real run or a dry run.
    pub mode: ExecutionMode,
    /// Optional run-time credential resolver (vault-backed). Nodes resolve
    /// `credential_ref`s through this so plaintext secrets never live in the IR
    /// or in persisted run records. `None` in dry runs and tests.
    pub credentials: Option<Arc<dyn CredentialResolver>>,
}

impl NodeContext {
    /// Resolve a `credential_ref` to its secret value via the configured
    /// resolver. Returns `Ok(None)` when no resolver is configured or no such
    /// credential exists — the caller decides whether that is an error.
    ///
    /// # Errors
    /// Propagates a [`CredentialError`] if the resolver's lookup fails.
    pub async fn resolve_credential(
        &self,
        credential_ref: &str,
    ) -> Result<Option<String>, CredentialError> {
        match &self.credentials {
            Some(resolver) => resolver.resolve(credential_ref).await,
            None => Ok(None),
        }
    }
}

impl fmt::Debug for NodeContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The credential resolver is deliberately not printed (it is opaque and
        // may guard secrets); show only whether one is present.
        f.debug_struct("NodeContext")
            .field("run_id", &self.run_id)
            .field("node_id", &self.node_id)
            .field("kind", &self.kind)
            .field("params", &self.params)
            .field("mode", &self.mode)
            .field("credentials", &self.credentials.as_ref().map(|_| "<resolver>"))
            .finish()
    }
}

/// Errors a node executor can return.
#[derive(Debug, Error)]
pub enum NodeError {
    /// The behaviour is not yet implemented (e.g. a milestone gap).
    #[error("not implemented: {0}")]
    NotImplemented(String),
    /// The node's `params` were missing or malformed.
    #[error("bad params: {0}")]
    BadParams(String),
    /// An HTTP-layer failure (transport, status, decoding).
    #[error("http error: {0}")]
    Http(String),
    /// A generic runtime failure during execution.
    #[error("runtime error: {0}")]
    Runtime(String),
}

/// Resolves a workflow's `credential_ref`s to their secret values at run time,
/// so plaintext secrets never appear in the workflow IR or persisted runs.
///
/// In production this is backed by the encrypted credential vault. It is absent
/// (`None` on [`NodeContext`]) during dry runs and unit tests.
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    /// Resolve a credential reference to its secret value, or `Ok(None)` if no
    /// credential is registered under that reference.
    ///
    /// # Errors
    /// Returns [`CredentialError`] if the underlying lookup or decryption fails.
    async fn resolve(&self, credential_ref: &str) -> Result<Option<String>, CredentialError>;
}

/// An error from a [`CredentialResolver`] lookup.
#[derive(Debug, Error)]
pub enum CredentialError {
    /// The credential store/vault lookup or decryption failed.
    #[error("credential resolution failed: {0}")]
    Lookup(String),
}

/// A unit of executable behaviour for one [`NodeKind`].
///
/// Object-safe via `#[async_trait]` so the registry can hold
/// `Arc<dyn NodeExecutor>`.
#[async_trait]
pub trait NodeExecutor: Send + Sync {
    /// Whether executing this node has observable side effects (network, tool
    /// calls, etc.). Pure nodes are safe to actually run during a dry run.
    fn has_side_effects(&self) -> bool;

    /// Execute the node against its `input` items, producing output items.
    ///
    /// # Errors
    /// Returns a [`NodeError`] if the node's params are invalid or execution
    /// fails. The engine maps this onto the node's `on_error` policy.
    async fn execute(&self, ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError>;

    /// Dry-run the node.
    ///
    /// Default behaviour: a side-effecting node returns a single mocked item
    /// (`{ "_mock": true }`) so the run shape can be validated without touching
    /// the outside world; a pure node simply runs [`NodeExecutor::execute`],
    /// since that is side-effect free. Side-effecting nodes are expected to
    /// override this with a more faithful mock.
    ///
    /// # Errors
    /// Propagates any error from `execute` for pure nodes.
    async fn dry_run(&self, ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        if self.has_side_effects() {
            Ok(vec![Item::produced(
                serde_json::json!({ "_mock": true }),
                ctx.node_id.clone(),
                0,
            )])
        } else {
            self.execute(ctx, input).await
        }
    }
}
