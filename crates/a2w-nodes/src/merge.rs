//! [`NodeKind::Merge`] executor.
//!
//! Merge combines multiple inbound branches into a single stream. The engine
//! already gathers a node's input by concatenating, in deterministic order, the
//! output items of every incoming connection — so Merge is a pass-through over
//! that already-merged input. (Lineage is re-stamped to this node by the engine,
//! so merged items correctly trace to the Merge as their immediate producer.)

use async_trait::async_trait;

use a2w_engine::{Item, NodeContext, NodeError, NodeExecutor};

/// Executor for [`a2w_ir::NodeKind::Merge`].
#[derive(Debug, Default)]
pub struct Merge;

#[async_trait]
impl NodeExecutor for Merge {
    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(&self, _ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        // Audit-2 fix (CRITICAL — port-routing silent drop): pass-through
        // executors MUST reset `output_port` to 0 on every output item.
        // Items inherited from a port-routing producer (Branch, Switch, Loop)
        // arrive carrying the upstream port; if we forwarded them unchanged,
        // a downstream single-port edge (`from_port=0`) would filter them out
        // in the engine's `gather_input`, silently dropping the whole stream.
        Ok(input.into_iter().map(|i| i.on_port(0)).collect())
    }
}
