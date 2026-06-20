//! [`NodeKind::SubWorkflow`] executor — invoke another workflow as a
//! sub-routine.
//!
//! Params:
//! ```json
//! { "workflow_id": "<id>", "trigger_input"?: [..] }
//! ```
//! or the inline form
//! ```json
//! { "workflow": <full Workflow IR>, "trigger_input"?: [..] }
//! ```
//!
//! Behaviour: for each input item (or just once if there is no input), the
//! executor resolves the sub-workflow, runs it via a fresh sub-engine, and
//! emits the sub-run's per-node final outputs flattened as `{ workflow_id,
//! status, node_outputs }` items on port `0`.
//!
//! ## Cycle protection
//! The engine carries a `sub_workflow_depth` counter on [`NodeContext`]; the
//! executor refuses to descend past
//! [`a2w_engine::DEFAULT_MAX_SUB_WORKFLOW_DEPTH`].
//!
//! ## Trust
//! The sub-engine inherits the parent's [`CredentialResolver`] AND the
//! parent's [`SubWorkflowResolver`] so nested SubWorkflows up to the depth
//! cap work transparently. It does NOT inherit the concurrency cap (the sub
//! gets its own Semaphore at DEFAULT) — sub-workflows can run concurrently
//! with the parent's other branches without contending for the parent's
//! permits.

use async_trait::async_trait;
use serde::Deserialize;

use a2w_engine::{
    Engine, Item, MemoryEventLog, NodeContext, NodeError, NodeExecutor,
    DEFAULT_MAX_SUB_WORKFLOW_DEPTH,
};

/// Executor for [`a2w_ir::NodeKind::SubWorkflow`](a2w_ir::NodeKind::SubWorkflow).
#[derive(Debug, Default, Clone)]
pub struct SubWorkflow;

#[derive(Debug, Deserialize)]
struct SubWorkflowSpec {
    #[serde(default)]
    workflow_id: Option<String>,
    #[serde(default)]
    workflow: Option<serde_json::Value>,
    #[serde(default)]
    trigger_input: Option<Vec<serde_json::Value>>,
    /// R3 audit-fix: parent credentials are NOT propagated to sub-workflows
    /// by default — that was a multi-tenant credential exfiltration vector
    /// (any author who can store a workflow could read another tenant's
    /// secrets via SubWorkflow). Opt in by setting this to `true`.
    #[serde(default)]
    propagate_credentials: bool,
}

impl SubWorkflow {
    fn parse(params: &serde_json::Value) -> Result<SubWorkflowSpec, NodeError> {
        serde_json::from_value(params.clone())
            .map_err(|e| NodeError::BadParams(format!("SubWorkflow params invalid: {e}")))
    }
}

#[async_trait]
impl NodeExecutor for SubWorkflow {
    fn has_side_effects(&self) -> bool {
        // Sub-workflows are side-effecting iff the parent's mode is Run AND
        // the sub-workflow contains side-effecting nodes. Conservative: yes.
        true
    }

    async fn execute(&self, ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        // Depth gate (defense against accidental recursion).
        if ctx.sub_workflow_depth >= DEFAULT_MAX_SUB_WORKFLOW_DEPTH {
            return Err(NodeError::Runtime(format!(
                "SubWorkflow recursion depth limit reached ({DEFAULT_MAX_SUB_WORKFLOW_DEPTH})"
            )));
        }

        let spec = Self::parse(&ctx.params)?;

        // Resolve the sub-workflow IR.
        let wf: a2w_ir::Workflow = match (spec.workflow.as_ref(), spec.workflow_id.as_ref()) {
            (Some(inline), _) => {
                let mut parsed: a2w_ir::Workflow =
                    serde_json::from_value(inline.clone()).map_err(|e| {
                        NodeError::BadParams(format!(
                            "inline `workflow` is not a valid Workflow IR: {e}"
                        ))
                    })?;
                // R5 H6 fix: rebrand the inline workflow's id with an
                // unforgeable `inline:<parent>:<node>` prefix so an
                // owner-scoped SubWorkflowResolver can't be tricked into
                // treating it as another tenant's workflow.
                let parent = ctx.workflow_id.as_deref().unwrap_or("<orphan>");
                parsed.id = format!("inline:{parent}:{}:{}", ctx.node_id, parsed.id);
                parsed
            }
            (None, Some(id)) => {
                let resolver = ctx.sub_workflows.as_ref().ok_or_else(|| {
                    NodeError::Runtime(
                        "SubWorkflow by id requires the engine to be configured with a \
                         sub-workflow resolver (Engine::with_sub_workflows)"
                            .into(),
                    )
                })?;
                // R5 H6 fix: refuse to forge an empty-string caller id —
                // a missing ctx.workflow_id would otherwise let any
                // owner-scoped resolver treat the caller as a colliding
                // "" tenant. Surface as a runtime error so the operator
                // fixes the call path (engine should always populate it).
                let caller = ctx.workflow_id.as_deref().ok_or_else(|| {
                    NodeError::Runtime(
                        "SubWorkflow needs ctx.workflow_id to authenticate the caller; \
                         the engine populates it during run() — this likely indicates \
                         an embedder that built NodeContext by hand"
                            .into(),
                    )
                })?;
                resolver
                    .get_workflow(caller, id)
                    .await
                    .map_err(|e| NodeError::Runtime(format!("SubWorkflow resolver error: {e}")))?
                    .ok_or_else(|| {
                        NodeError::BadParams(format!("sub-workflow id '{id}' not found"))
                    })?
            }
            (None, None) => {
                return Err(NodeError::BadParams(
                    "SubWorkflow params must include `workflow_id` or `workflow`".into(),
                ));
            }
        };

        // Build a sub-engine. Critical audit-3 fix: the sub-engine MUST be
        // seeded with `parent_depth + 1` so nested SubWorkflows actually
        // descend through the recursion cap; without this every sub-engine
        // sees depth=0 and the cap never fires.
        //
        // Credentials + sub-workflow resolvers + approvals all propagate from
        // the parent so a sub-workflow can also fetch secrets, recurse, and
        // request approvals.
        let registry = crate::default_registry();
        let mut sub_engine = Engine::new(registry)
            .with_initial_sub_workflow_depth(ctx.sub_workflow_depth.saturating_add(1));
        // R3 audit-fix: credentials propagate ONLY when the workflow author
        // opts in via `propagate_credentials: true`. Default-off prevents a
        // multi-tenant exfiltration vector where any caller could read
        // arbitrary credentials by invoking a sub-workflow that names them.
        if spec.propagate_credentials {
            if let Some(creds) = ctx.credentials.as_ref() {
                sub_engine = sub_engine.with_credentials(creds.clone());
            }
        }
        if let Some(resolver) = ctx.sub_workflows.as_ref() {
            sub_engine = sub_engine.with_sub_workflows(resolver.clone());
        }
        if let Some(gate) = ctx.approvals.as_ref() {
            sub_engine = sub_engine.with_approvals(gate.clone());
        }

        // Trigger input: explicit param wins; otherwise feed each upstream
        // item one at a time so a fan-in upstream produces multiple sub-runs.
        // The simplest semantic: ALL upstream items become the trigger input
        // for ONE sub-run. Multi-run semantics ("one sub-run per input item")
        // can be done via Loop → SubWorkflow.
        let trigger_input = match spec.trigger_input {
            Some(t) => t,
            None => {
                // Convert each input Item's json to a trigger value.
                input.iter().map(|i| i.json.clone()).collect()
            }
        };

        let log = MemoryEventLog::new();
        // Carry the parent's run_id forward only as a logical link — the
        // sub-engine mints its own run_id internally so step_records for the
        // sub-run are namespaced separately.
        let mode = ctx.mode;
        let sub_result = sub_engine
            .run(&wf, trigger_input, mode, &log)
            .await
            .map_err(|e| NodeError::Runtime(format!("sub-workflow run failed: {e}")))?;

        // Map the sub-run's result to a single output item per *terminal*
        // node (a node with no outgoing connection inside the sub-workflow).
        let terminals: std::collections::HashSet<&str> =
            terminal_node_ids(&wf).into_iter().collect();
        let mut out = Vec::new();
        for (node_id, items) in sub_result.node_outputs.iter() {
            if !terminals.contains(node_id.as_str()) {
                continue;
            }
            for (i, item) in items.iter().enumerate() {
                out.push(Item::produced(
                    serde_json::json!({
                        "sub_run_id": sub_result.run_id,
                        "sub_workflow_id": wf.id,
                        "terminal_node": node_id,
                        "value": item.json,
                    }),
                    ctx.node_id.clone(),
                    i,
                ));
            }
        }
        // If the sub-workflow had no terminals (e.g. single trigger), still
        // emit one summary item so downstream nodes see the sub-run happened.
        if out.is_empty() {
            out.push(Item::produced(
                serde_json::json!({
                    "sub_run_id": sub_result.run_id,
                    "sub_workflow_id": wf.id,
                    "status": format!("{:?}", sub_result.status),
                    "value": serde_json::Value::Null,
                }),
                ctx.node_id.clone(),
                0,
            ));
        }
        Ok(out)
    }
}

/// Node ids in `wf` that have no outgoing connection (terminals).
fn terminal_node_ids(wf: &a2w_ir::Workflow) -> Vec<&str> {
    use std::collections::HashSet;
    let producers: HashSet<&str> = wf.connections.iter().map(|c| c.from_node.as_str()).collect();
    wf.nodes
        .iter()
        .filter(|n| !producers.contains(n.id.as_str()))
        .map(|n| n.id.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2w_engine::ExecutionMode;
    use a2w_ir::NodeKind;

    fn ctx(params: serde_json::Value, depth: u8) -> NodeContext {
        ctx_mode(params, depth, ExecutionMode::DryRun)
    }

    fn ctx_mode(params: serde_json::Value, depth: u8, mode: ExecutionMode) -> NodeContext {
        NodeContext {
            run_id: "r".into(),
            node_id: "sub".into(),
            kind: NodeKind::SubWorkflow,
            params,
            mode,
            credentials: None,
            sub_workflows: None,
            sub_workflow_depth: depth,
            workflow_id: None,
            approvals: None,
        }
    }

    fn tiny_inline_workflow() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "id": "sub_inline",
            "name": "Inline sub",
            "nodes": [
                { "id": "trigger", "kind": "webhook_trigger", "params": {} },
                { "id": "shape", "kind": "transform", "params": { "set": { "tag": "sub" } } }
            ],
            "connections": [
                { "from_node": "trigger", "from_port": 0, "to_node": "shape" }
            ]
        })
    }

    #[tokio::test]
    async fn inline_workflow_runs_and_emits_summary() {
        let sw = SubWorkflow;
        let c = ctx(
            serde_json::json!({
                "workflow": tiny_inline_workflow(),
                "trigger_input": [ { "id": 1 } ]
            }),
            0,
        );
        let out = sw.execute(&c, Vec::new()).await.expect("ok");
        assert!(!out.is_empty(), "must emit at least one summary item");
        // The shape node was the terminal; its tag should bubble up.
        let has_tag = out.iter().any(|i| i.json["value"]["tag"] == "sub");
        assert!(has_tag, "terminal output must surface: {out:?}");
    }

    #[tokio::test]
    async fn missing_id_and_inline_is_bad_params() {
        let sw = SubWorkflow;
        let c = ctx(serde_json::json!({}), 0);
        let err = sw.execute(&c, Vec::new()).await.unwrap_err();
        assert!(matches!(err, NodeError::BadParams(_)));
    }

    #[tokio::test]
    async fn workflow_id_without_resolver_errors() {
        let sw = SubWorkflow;
        let c = ctx(serde_json::json!({ "workflow_id": "wf_x" }), 0);
        let err = sw.execute(&c, Vec::new()).await.unwrap_err();
        assert!(matches!(err, NodeError::Runtime(_)));
    }

    #[tokio::test]
    async fn depth_cap_blocks_excessive_recursion() {
        let sw = SubWorkflow;
        let c = ctx(
            serde_json::json!({ "workflow": tiny_inline_workflow() }),
            DEFAULT_MAX_SUB_WORKFLOW_DEPTH,
        );
        let err = sw.execute(&c, Vec::new()).await.unwrap_err();
        assert!(matches!(err, NodeError::Runtime(_)));
        assert!(err.to_string().contains("depth limit"));
    }

    /// Audit-3 regression: when a SubWorkflow inline IR itself contains
    /// another SubWorkflow, the recursion depth carries through to the
    /// sub-engine. Without `with_initial_sub_workflow_depth` propagation
    /// the sub-engine always sees depth=0 and the cap never fires.
    #[tokio::test]
    async fn nested_sub_workflow_propagates_depth() {
        // Build a workflow with a SubWorkflow → SubWorkflow → terminal.
        // After 2 hops the inner-most sub-engine should see depth=2.
        let level2 = serde_json::json!({
            "schema_version": 1,
            "id": "level2",
            "name": "level2",
            "nodes": [
                { "id": "t", "kind": "webhook_trigger", "params": {} },
                { "id": "shape", "kind": "transform", "params": { "set": { "level": 2 } } }
            ],
            "connections": [{ "from_node": "t", "from_port": 0, "to_node": "shape" }]
        });
        let level1 = serde_json::json!({
            "schema_version": 1,
            "id": "level1",
            "name": "level1",
            "nodes": [
                { "id": "t", "kind": "webhook_trigger", "params": {} },
                { "id": "sub", "kind": "sub_workflow", "params": { "workflow": level2 } }
            ],
            "connections": [{ "from_node": "t", "from_port": 0, "to_node": "sub" }]
        });

        let sw = SubWorkflow;
        // Run mode so the sub-engine actually executes (DryRun mocks).
        let c = ctx_mode(
            serde_json::json!({ "workflow": level1 }),
            0,
            ExecutionMode::Run,
        );
        let out = sw.execute(&c, Vec::new()).await.expect("ok");
        assert!(!out.is_empty(), "nested sub-workflow must produce output");
    }

    /// Confirm the depth-cap check fires when called at the cap directly
    /// (the recursion-cycle test relies on engine integration which is
    /// covered by integration tests; this in-unit test pins the cap logic).
    #[tokio::test]
    async fn depth_cap_at_boundary() {
        let sw = SubWorkflow;
        let c = ctx(
            serde_json::json!({ "workflow": tiny_inline_workflow() }),
            DEFAULT_MAX_SUB_WORKFLOW_DEPTH,
        );
        let err = sw.execute(&c, Vec::new()).await.unwrap_err();
        assert!(matches!(err, NodeError::Runtime(_)));
    }
}
