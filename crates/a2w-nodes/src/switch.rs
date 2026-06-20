//! [`NodeKind::Switch`] executor — multi-way conditional split keyed on a
//! value.
//!
//! Params:
//! ```json
//! { "key": "<json.pointer>", "cases": [ { "value": <any>, "port": <usize> }, ... ],
//!   "default_port": <usize>? }
//! ```
//! - For each input item, evaluates `key` (a JSON pointer into the item) and
//!   routes the item to the FIRST case whose `value` is equal to the resolved
//!   value.
//! - If no case matches, routes to `default_port` (the highest port index of
//!   any case + 1 when omitted).
//!
//! Pure (no side effects).

use async_trait::async_trait;
use serde::Deserialize;

use a2w_engine::{Item, NodeContext, NodeError, NodeExecutor};

/// Executor for [`a2w_ir::NodeKind::Switch`](a2w_ir::NodeKind::Switch).
#[derive(Debug, Default, Clone)]
pub struct Switch;

#[derive(Debug, Deserialize)]
struct SwitchSpec {
    key: String,
    cases: Vec<SwitchCase>,
    #[serde(default)]
    default_port: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct SwitchCase {
    value: serde_json::Value,
    port: usize,
}

impl Switch {
    fn parse(params: &serde_json::Value) -> Result<SwitchSpec, NodeError> {
        serde_json::from_value(params.clone())
            .map_err(|e| NodeError::BadParams(format!("Switch params invalid: {e}")))
    }

    fn route(spec: &SwitchSpec, item: &serde_json::Value) -> usize {
        // Audit-2 fix: parity with Branch — `/` (or empty) shorthand resolves
        // to the WHOLE item; otherwise treated as a JSON pointer. This makes
        // `Switch` usable for cases like `{ key: "/", cases: [...] }` over
        // scalar trigger items.
        let resolved = if spec.key.is_empty() || spec.key == "/" {
            Some(item)
        } else {
            item.pointer(&spec.key)
        };
        if let Some(v) = resolved {
            for case in &spec.cases {
                if &case.value == v {
                    return case.port;
                }
            }
        }
        spec.default_port.unwrap_or_else(|| {
            // No explicit default: synthesize one past the highest explicit
            // case port. Empty cases array → port 0.
            spec.cases.iter().map(|c| c.port).max().map(|m| m + 1).unwrap_or(0)
        })
    }
}

#[async_trait]
impl NodeExecutor for Switch {
    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(&self, ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        let spec = Self::parse(&ctx.params)?;
        let mut out = Vec::with_capacity(input.len());
        for item in input {
            let port = Self::route(&spec, &item.json);
            out.push(item.on_port(port));
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
            node_id: "sw".into(),
            kind: NodeKind::Switch,
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
    async fn routes_by_first_matching_case() {
        let s = Switch;
        let c = ctx(serde_json::json!({
            "key": "/severity",
            "cases": [
                { "value": "critical", "port": 0 },
                { "value": "warning",  "port": 1 },
                { "value": "info",     "port": 2 }
            ],
            "default_port": 3
        }));
        let input = vec![
            Item::root(serde_json::json!({ "severity": "critical" })),
            Item::root(serde_json::json!({ "severity": "warning" })),
            Item::root(serde_json::json!({ "severity": "info" })),
            Item::root(serde_json::json!({ "severity": "unknown" })),
            Item::root(serde_json::json!({})),
        ];
        let out = s.execute(&c, input).await.unwrap();
        let ports: Vec<usize> = out.iter().map(|i| i.output_port).collect();
        assert_eq!(ports, vec![0, 1, 2, 3, 3]);
    }

    #[tokio::test]
    async fn missing_default_port_picks_max_plus_one() {
        let s = Switch;
        let c = ctx(serde_json::json!({
            "key": "/k",
            "cases": [
                { "value": "a", "port": 0 },
                { "value": "b", "port": 1 }
            ]
        }));
        let input = vec![Item::root(serde_json::json!({ "k": "z" }))];
        let out = s.execute(&c, input).await.unwrap();
        assert_eq!(out[0].output_port, 2);
    }
}
