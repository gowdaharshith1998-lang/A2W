//! [`NodeKind::Loop`] executor — fan an input collection into one item per
//! element.
//!
//! Params:
//! ```json
//! { "over": "<json.pointer>" }
//! ```
//!
//! For each input item, resolves `over` (a JSON pointer that must point at an
//! array) and emits one output item per element of that array on port `0`.
//! When `over` is missing or doesn't point at an array, the original item is
//! passed through unchanged on port `0`.
//!
//! Each emitted element is wrapped as `{ "index": <usize>, "value": <element>,
//! "parent": <original item json> }` so downstream nodes can correlate the
//! iteration index back to the source item.
//!
//! Port `1` ("done") emits a summary item per parent
//! `{ "parent": ..., "count": <usize> }` so a downstream "after loop" branch
//! can reason about iteration counts.
//!
//! Pure (no side effects).

use async_trait::async_trait;
use serde::Deserialize;

use a2w_engine::{Item, NodeContext, NodeError, NodeExecutor};

/// Executor for [`a2w_ir::NodeKind::Loop`](a2w_ir::NodeKind::Loop).
#[derive(Debug, Default, Clone)]
pub struct Loop;

#[derive(Debug, Deserialize)]
struct LoopSpec {
    over: String,
}

impl Loop {
    fn parse(params: &serde_json::Value) -> Result<LoopSpec, NodeError> {
        serde_json::from_value(params.clone())
            .map_err(|e| NodeError::BadParams(format!("Loop params invalid: {e}")))
    }
}

#[async_trait]
impl NodeExecutor for Loop {
    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(&self, ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        let spec = Self::parse(&ctx.params)?;
        let mut out = Vec::new();
        for item in input {
            let pointer = if spec.over.is_empty() { "/" } else { &spec.over };
            let resolved = if pointer == "/" {
                Some(&item.json)
            } else {
                item.json.pointer(pointer)
            };
            match resolved {
                Some(serde_json::Value::Array(arr)) => {
                    for (i, elt) in arr.iter().enumerate() {
                        out.push(
                            Item::produced(
                                serde_json::json!({
                                    "index": i,
                                    "value": elt,
                                    "parent": item.json,
                                }),
                                ctx.node_id.clone(),
                                0,
                            )
                            .on_port(0),
                        );
                    }
                    out.push(
                        Item::produced(
                            serde_json::json!({
                                "parent": item.json,
                                "count": arr.len(),
                            }),
                            ctx.node_id.clone(),
                            0,
                        )
                        .on_port(1),
                    );
                }
                _ => {
                    // Audit-2 fix: pointer missing / not an array — pass the
                    // item through on port 0, and do NOT emit a `done`
                    // summary on port 1. Emitting a summary for the
                    // non-iterating path makes port 1 contracts inconsistent
                    // (sometimes a per-parent summary, sometimes inherited
                    // from upstream noise).
                    out.push(item.on_port(0));
                }
            }
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
            node_id: "lp".into(),
            kind: NodeKind::Loop,
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
    async fn emits_one_item_per_element_on_port_0() {
        let l = Loop;
        let c = ctx(serde_json::json!({ "over": "/items" }));
        let input = vec![Item::root(serde_json::json!({
            "items": [{ "k": 1 }, { "k": 2 }, { "k": 3 }]
        }))];
        let out = l.execute(&c, input).await.unwrap();
        let port0: Vec<_> = out.iter().filter(|i| i.output_port == 0).collect();
        let port1: Vec<_> = out.iter().filter(|i| i.output_port == 1).collect();
        assert_eq!(port0.len(), 3);
        assert_eq!(port0[0].json["index"], 0);
        assert_eq!(port0[0].json["value"]["k"], 1);
        assert_eq!(port1.len(), 1);
        assert_eq!(port1[0].json["count"], 3);
    }

    #[tokio::test]
    async fn non_array_passes_through() {
        let l = Loop;
        let c = ctx(serde_json::json!({ "over": "/missing" }));
        let input = vec![Item::root(serde_json::json!({ "a": 1 }))];
        let out = l.execute(&c, input).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].output_port, 0);
        assert_eq!(out[0].json["a"], 1);
    }
}
