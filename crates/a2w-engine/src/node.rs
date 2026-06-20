//! The node-executor abstraction: the trait every node kind implements, plus the
//! per-execution context and error type.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use a2w_ir::{NodeKind, Workflow};

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
    /// Optional resolver of stored workflows. The SubWorkflow executor uses
    /// this to load a workflow by id and run it as a sub-routine. `None`
    /// when SubWorkflow support is not wired (the executor then accepts only
    /// the inline `workflow` param form).
    pub sub_workflows: Option<Arc<dyn SubWorkflowResolver>>,
    /// SubWorkflow recursion depth, starting at `0` for the top-level run.
    /// The executor refuses to descend past
    /// [`crate::engine::DEFAULT_MAX_SUB_WORKFLOW_DEPTH`].
    pub sub_workflow_depth: u8,
    /// R4 audit-fix: the id of the workflow that owns this node, so the
    /// SubWorkflow executor can pass it to [`SubWorkflowResolver::get_workflow`]
    /// for owner-scoped lookups. `None` only when the engine is invoked
    /// outside a top-level workflow run (e.g. unit tests).
    pub workflow_id: Option<String>,
    /// Optional approval gate. The `Approval` executor uses it to record a
    /// pending approval row and poll for a decision. `None` makes the
    /// executor return a clear "approvals not configured" error.
    pub approvals: Option<Arc<dyn ApprovalGate>>,
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
        // Resolvers are opaque and may guard secrets; show presence only.
        f.debug_struct("NodeContext")
            .field("run_id", &self.run_id)
            .field("node_id", &self.node_id)
            .field("kind", &self.kind)
            .field("params", &self.params)
            .field("mode", &self.mode)
            .field(
                "credentials",
                &self.credentials.as_ref().map(|_| "<resolver>"),
            )
            .field(
                "sub_workflows",
                &self.sub_workflows.as_ref().map(|_| "<resolver>"),
            )
            .field("sub_workflow_depth", &self.sub_workflow_depth)
            .field("approvals", &self.approvals.as_ref().map(|_| "<gate>"))
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

/// Outcome of an approval gate poll. Drives the `Approval` executor's
/// port-0 (approved) / port-1 (rejected) routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// The approval was granted.
    Approved {
        /// Free-text attribution from `POST /approvals/{id}`.
        decided_by: Option<String>,
    },
    /// The approval was explicitly rejected.
    Rejected {
        /// Free-text attribution.
        decided_by: Option<String>,
    },
}

/// A pluggable approval gate. The `Approval` executor records a pending row
/// via `request` then asks `poll` on a backoff until a decision arrives or
/// the configured timeout elapses (timeout = rejection by policy).
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    /// Create a pending approval row for `(run_id, node_id, idx)` with
    /// `payload_json` shown to the human approver. Returns the approval id
    /// that downstream `poll` calls reference.
    ///
    /// # Errors
    /// [`CredentialError`] (re-used to keep the error type set small) on
    /// underlying store failure.
    async fn request(
        &self,
        run_id: &str,
        node_id: &str,
        idx: usize,
        payload_json: &str,
    ) -> Result<String, CredentialError>;

    /// Poll for the decision on `approval_id`. `Ok(None)` means "still
    /// pending"; `Ok(Some(_))` is the final decision.
    ///
    /// # Errors
    /// [`CredentialError`] on a store read failure.
    async fn poll(&self, approval_id: &str) -> Result<Option<ApprovalOutcome>, CredentialError>;
}

/// Resolves a stored workflow by its id, enabling the SubWorkflow executor to
/// invoke another workflow as a sub-routine. Backed by `a2w_store::Store` in
/// production; tests inject a deterministic mock.
///
/// R4 audit-fix: the resolver carries an OWNER context — the workflow id
/// that is making the lookup — so a multi-tenant store implementation can
/// enforce that workflow A may only resolve workflow B when A and B share
/// an owner. The default single-tenant store ignores it.
#[async_trait]
pub trait SubWorkflowResolver: Send + Sync {
    /// Fetch a workflow by id, or `Ok(None)` when absent.
    ///
    /// `caller_workflow_id` is the id of the workflow whose SubWorkflow
    /// node is making the lookup. Owner-scoped stores use it to refuse
    /// cross-tenant reads.
    ///
    /// # Errors
    /// Returns a [`CredentialError`] (re-used to avoid a third error type) on
    /// a lookup failure; the executor surfaces this as a `NodeError::Runtime`.
    async fn get_workflow(
        &self,
        caller_workflow_id: &str,
        workflow_id: &str,
    ) -> Result<Option<Workflow>, CredentialError>;
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
