//! [`NodeKind::WebhookTrigger`] executor.
//!
//! The trigger is the workflow entry point. The engine seeds the trigger's
//! input with the root items derived from `trigger_input`, so this executor
//! simply passes that input straight through.

use async_trait::async_trait;

use a2w_engine::{Item, NodeContext, NodeError, NodeExecutor};

/// Executor for [`a2w_ir::NodeKind::WebhookTrigger`].
#[derive(Debug, Default)]
pub struct WebhookTrigger;

#[async_trait]
impl NodeExecutor for WebhookTrigger {
    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(&self, _ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        // The engine already seeded the trigger items; pass them through. The
        // engine re-stamps lineage to this node on the way out.
        Ok(input)
    }
}
