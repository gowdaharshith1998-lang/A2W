//! [`NodeKind::Branch`] executor — two-way conditional split.
//!
//! Params:
//! ```json
//! { "condition": { "path": "<json.pointer>", "op": "truthy"|"eq"|"ne"|"contains", "value": <any> } }
//! ```
//!
//! Or the convenience shorthand `{ "condition": "<json.pointer>" }` which is
//! equivalent to `{ "path": "<...>", "op": "truthy" }`.
//!
//! Output ports:
//! - `0` ("true") — items for which the condition evaluated truthy
//! - `1` ("false") — items for which it did not
//!
//! Both ports receive items with their lineage preserved; downstream
//! connections from `(branch, 0)` get the "true" items, `(branch, 1)` get the
//! "false" ones. A node with no incoming items emits nothing on either port.
//!
//! Pure (no side effects): both dry-run and real-run run the same code.

use async_trait::async_trait;
use serde::Deserialize;

use a2w_engine::{Item, NodeContext, NodeError, NodeExecutor};

/// Executor for [`a2w_ir::NodeKind::Branch`](a2w_ir::NodeKind::Branch).
#[derive(Debug, Default, Clone)]
pub struct Branch;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BranchSpec {
    /// Shorthand: just a JSON pointer to test for truthiness.
    Path(String),
    /// Structured condition.
    Full(BranchCondition),
}

#[derive(Debug, Deserialize)]
struct BranchCondition {
    path: String,
    #[serde(default = "default_op")]
    op: BranchOp,
    #[serde(default)]
    value: serde_json::Value,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum BranchOp {
    Truthy,
    Eq,
    Ne,
    Contains,
}

fn default_op() -> BranchOp {
    BranchOp::Truthy
}

impl Branch {
    fn parse(params: &serde_json::Value) -> Result<BranchCondition, NodeError> {
        let raw = params
            .get("condition")
            .ok_or_else(|| NodeError::BadParams("Branch requires a `condition`".into()))?;
        let spec: BranchSpec = serde_json::from_value(raw.clone()).map_err(|e| {
            NodeError::BadParams(format!(
                "Branch `condition` must be a JSON pointer string or {{ path, op?, value? }}: {e}"
            ))
        })?;
        let cond = match spec {
            BranchSpec::Path(path) => BranchCondition {
                path,
                op: BranchOp::Truthy,
                value: serde_json::Value::Null,
            },
            BranchSpec::Full(c) => c,
        };
        // Audit-2 fix: `condition: "/"` + Truthy is effectively always true
        // for any non-empty item — useless as a routing predicate. Reject it
        // explicitly so workflow authors point at a real field.
        if cond.op == BranchOp::Truthy && (cond.path.is_empty() || cond.path == "/") {
            return Err(NodeError::BadParams(
                "Branch `condition.path` must point at a real field when op is `truthy`; \
                 the root path always evaluates true for non-empty items"
                    .into(),
            ));
        }
        Ok(cond)
    }

    fn evaluate(cond: &BranchCondition, item: &serde_json::Value) -> bool {
        // Audit-2 fix: a `/` (or empty) shorthand resolves to the WHOLE item.
        // Under Truthy this would always be true for any non-empty payload,
        // which makes the routing useless. So for Truthy with `/`, we treat
        // an absent value as false but a present empty value (`{}`, `[]`,
        // `""`, `0`, `null`, `false`) as falsey via the existing arms.
        let resolved = if cond.path.is_empty() || cond.path == "/" {
            Some(item)
        } else {
            item.pointer(&cond.path)
        };
        match cond.op {
            BranchOp::Truthy => match resolved {
                Some(serde_json::Value::Null) | None => false,
                Some(serde_json::Value::Bool(b)) => *b,
                Some(serde_json::Value::Number(n)) => n.as_f64() != Some(0.0),
                Some(serde_json::Value::String(s)) => !s.is_empty(),
                Some(serde_json::Value::Array(a)) => !a.is_empty(),
                Some(serde_json::Value::Object(o)) => !o.is_empty(),
            },
            BranchOp::Eq => resolved.is_some_and(|v| v == &cond.value),
            BranchOp::Ne => resolved.is_some_and(|v| v != &cond.value),
            BranchOp::Contains => match resolved {
                Some(serde_json::Value::String(s)) => {
                    cond.value.as_str().is_some_and(|n| s.contains(n))
                }
                Some(serde_json::Value::Array(a)) => a.iter().any(|v| v == &cond.value),
                Some(serde_json::Value::Object(o)) => {
                    cond.value.as_str().is_some_and(|k| o.contains_key(k))
                }
                _ => false,
            },
        }
    }
}

#[async_trait]
impl NodeExecutor for Branch {
    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(&self, ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        let cond = Self::parse(&ctx.params)?;
        let mut out = Vec::with_capacity(input.len());
        for item in input {
            let truthy = Self::evaluate(&cond, &item.json);
            // Preserve the item's payload; assign the routing port. The engine
            // re-stamps `source` after we return, so we leave that alone.
            out.push(item.on_port(if truthy { 0 } else { 1 }));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2w_engine::ExecutionMode;
    use a2w_ir::NodeKind;

    fn ctx(params: serde_json::Value) -> NodeContext {
        NodeContext {
            run_id: "r".into(),
            node_id: "br".into(),
            kind: NodeKind::Branch,
            params,
            mode: ExecutionMode::Run,
            credentials: None,
        sub_workflows: None,
        sub_workflow_depth: 0,
        workflow_id: None,
        approvals: None,
        }
    }

    #[tokio::test]
    async fn truthy_shorthand_routes_by_pointer() {
        let b = Branch;
        let c = ctx(serde_json::json!({ "condition": "/active" }));
        let input = vec![
            Item::root(serde_json::json!({ "active": true })),
            Item::root(serde_json::json!({ "active": false })),
            Item::root(serde_json::json!({})), // missing → false
        ];
        let out = b.execute(&c, input).await.unwrap();
        let ports: Vec<usize> = out.iter().map(|i| i.output_port).collect();
        assert_eq!(ports, vec![0, 1, 1]);
    }

    #[tokio::test]
    async fn eq_op() {
        let b = Branch;
        let c = ctx(serde_json::json!({
            "condition": { "path": "/status", "op": "eq", "value": "ok" }
        }));
        let input = vec![
            Item::root(serde_json::json!({ "status": "ok" })),
            Item::root(serde_json::json!({ "status": "bad" })),
        ];
        let out = b.execute(&c, input).await.unwrap();
        assert_eq!(out[0].output_port, 0);
        assert_eq!(out[1].output_port, 1);
    }

    #[tokio::test]
    async fn contains_op_on_array() {
        let b = Branch;
        let c = ctx(serde_json::json!({
            "condition": { "path": "/tags", "op": "contains", "value": "alert" }
        }));
        let input = vec![
            Item::root(serde_json::json!({ "tags": ["info", "alert"] })),
            Item::root(serde_json::json!({ "tags": ["info"] })),
        ];
        let out = b.execute(&c, input).await.unwrap();
        assert_eq!(out[0].output_port, 0);
        assert_eq!(out[1].output_port, 1);
    }

    #[tokio::test]
    async fn missing_condition_is_bad_params() {
        let b = Branch;
        let c = ctx(serde_json::json!({}));
        let err = b.execute(&c, vec![]).await.unwrap_err();
        assert!(matches!(err, NodeError::BadParams(_)));
    }
}
