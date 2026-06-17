//! The node-executor abstraction: the trait every node kind implements, plus the
//! per-execution context and error type.

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
#[derive(Debug, Clone)]
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
