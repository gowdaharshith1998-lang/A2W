//! [`NodeKind::ScheduleTrigger`] executor.
//!
//! Like the webhook trigger, the scheduled trigger is a workflow entry point. In
//! production a scheduler fires the run on a cron interval; the engine seeds the
//! trigger's input with the root items, and this executor passes them straight
//! through (the engine re-stamps lineage to this node on the way out).

use async_trait::async_trait;

use a2w_engine::{Item, NodeContext, NodeError, NodeExecutor};

/// Executor for [`a2w_ir::NodeKind::ScheduleTrigger`].
#[derive(Debug, Default)]
pub struct ScheduleTrigger;

#[async_trait]
impl NodeExecutor for ScheduleTrigger {
    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(&self, _ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        Ok(input)
    }
}
