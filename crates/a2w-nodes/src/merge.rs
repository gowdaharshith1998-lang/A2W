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
        Ok(input)
    }
}
