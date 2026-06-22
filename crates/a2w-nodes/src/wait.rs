//! [`NodeKind::Wait`] executor — pause for a duration.
//!
//! Params: `{ "duration_ms": <u64> }`. Defaults to 0 ms when absent (acts as a
//! pass-through). The wait happens once per execution (not per item) so a node
//! with 100 input items still waits a single duration before passing them all
//! through unchanged.
//!
//! Dry-run does NOT actually sleep — it just passes through.

use std::time::Duration;

use async_trait::async_trait;

use a2w_engine::{ExecutionMode, Item, NodeContext, NodeError, NodeExecutor};

/// Executor for [`a2w_ir::NodeKind::Wait`](a2w_ir::NodeKind::Wait).
#[derive(Debug, Default, Clone)]
pub struct Wait;

impl Wait {
    fn duration(params: &serde_json::Value) -> Result<Duration, NodeError> {
        let ms = params
            .get("duration_ms")
            .map(|v| v.as_u64())
            .unwrap_or(Some(0))
            .ok_or_else(|| {
                NodeError::BadParams("Wait `duration_ms` must be a non-negative integer".into())
            })?;
        // Cap the wait to 60 minutes to defend against `duration_ms: u64::MAX`
        // bringing the engine to a halt for a workflow author's mistake.
        const MAX_MS: u64 = 60 * 60 * 1000;
        let ms = ms.min(MAX_MS);
        Ok(Duration::from_millis(ms))
    }
}

#[async_trait]
impl NodeExecutor for Wait {
    fn has_side_effects(&self) -> bool {
        // Wall-clock effect; dry_run must not actually sleep.
        true
    }

    async fn execute(&self, ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        // Audit-2 fix (CRITICAL — silent unselected-branch side effect): if
        // the upstream port-routing producer (Branch/Switch/Loop) routed every
        // item to a port other than the one feeding this node, our input is
        // empty. Sleeping anyway is a side-effect on the unselected arm —
        // refuse to sleep when there is nothing to act on.
        if input.is_empty() {
            return Ok(Vec::new());
        }
        if ctx.mode == ExecutionMode::Run {
            let d = Self::duration(&ctx.params)?;
            if !d.is_zero() {
                tokio::time::sleep(d).await;
            }
        }
        // Reset output_port to 0 — see merge.rs for rationale.
        Ok(input.into_iter().map(|i| i.on_port(0)).collect())
    }

    async fn dry_run(&self, _ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        // No sleep on dry-run; pass through with port reset.
        Ok(input.into_iter().map(|i| i.on_port(0)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2w_ir::NodeKind;

    fn ctx(params: serde_json::Value, mode: ExecutionMode) -> NodeContext {
        NodeContext {
            run_id: "r".into(),
            node_id: "w".into(),
            kind: NodeKind::Wait,
            params,
            mode,
            credentials: None,
            sub_workflows: None,
            sub_workflow_depth: 0,
            workflow_id: None,
            approvals: None,
            metrics: None,
        }
    }

    #[tokio::test]
    async fn zero_duration_is_pass_through() {
        let w = Wait;
        let c = ctx(serde_json::json!({}), ExecutionMode::Run);
        let input = vec![Item::root(serde_json::json!({ "a": 1 }))];
        let out = w.execute(&c, input).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].json["a"], 1);
    }

    #[tokio::test]
    async fn duration_actually_waits_in_run_mode() {
        let w = Wait;
        let c = ctx(serde_json::json!({ "duration_ms": 50 }), ExecutionMode::Run);
        // Non-empty input — audit-2: Wait now skips the sleep on empty input
        // (unselected-branch arm) and only fires when there is real work to
        // act on.
        let input = vec![Item::root(serde_json::json!({ "id": 1 }))];
        let start = std::time::Instant::now();
        let _ = w.execute(&c, input).await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(40),
            "expected ~50ms wait, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn empty_input_skips_sleep_audit2() {
        // Audit-2 regression: confirm an empty input does NOT trigger a sleep.
        let w = Wait;
        let c = ctx(
            serde_json::json!({ "duration_ms": 1000 }),
            ExecutionMode::Run,
        );
        let start = std::time::Instant::now();
        let out = w.execute(&c, vec![]).await.unwrap();
        let elapsed = start.elapsed();
        assert!(out.is_empty(), "empty input must yield empty output");
        assert!(
            elapsed < Duration::from_millis(100),
            "empty input must NOT sleep, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn dry_run_does_not_wait() {
        let w = Wait;
        let c = ctx(
            serde_json::json!({ "duration_ms": 5000 }),
            ExecutionMode::DryRun,
        );
        let start = std::time::Instant::now();
        let _ = w.dry_run(&c, vec![]).await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "dry_run waited: {elapsed:?}"
        );
    }
}
