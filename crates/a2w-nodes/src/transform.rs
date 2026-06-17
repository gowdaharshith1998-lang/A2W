//! [`NodeKind::Transform`] executor.
//!
//! Pure (no side effects). Params: `{ "set": { ...fields... } }`. For each input
//! item, the output is the input object merged with `set` (set wins on key
//! collisions). With no `set`, items pass through unchanged.
//!
//! NOTE: this is the minimal merge form. Full jq/jaq mapping over the item
//! context is deferred to the expression milestone.

use async_trait::async_trait;

use a2w_engine::{Item, NodeContext, NodeError, NodeExecutor};

/// Executor for [`a2w_ir::NodeKind::Transform`].
#[derive(Debug, Default)]
pub struct Transform;

#[async_trait]
impl NodeExecutor for Transform {
    fn has_side_effects(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        ctx: &NodeContext,
        input: Vec<Item>,
    ) -> Result<Vec<Item>, NodeError> {
        // Pull the optional `set` object out of params.
        let set = match ctx.params.get("set") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::Object(map)) => Some(map.clone()),
            Some(_) => {
                return Err(NodeError::BadParams(
                    "Transform `set` must be a JSON object".to_string(),
                ));
            }
        };

        let mut out = Vec::with_capacity(input.len());
        for item in input {
            let json = match (&set, item.json) {
                // No `set`: pass the item json through unchanged.
                (None, json) => json,
                // Merge `set` over an object item.
                (Some(set), serde_json::Value::Object(mut obj)) => {
                    for (k, v) in set {
                        obj.insert(k.clone(), v.clone());
                    }
                    serde_json::Value::Object(obj)
                }
                // `set` against a non-object item is a params/data mismatch.
                (Some(_), other) => {
                    return Err(NodeError::BadParams(format!(
                        "Transform with `set` requires object items, got {}",
                        kind_name(&other)
                    )));
                }
            };
            // Lineage is re-stamped by the engine; the index here is ignored.
            out.push(Item::produced(json, ctx.node_id.clone(), 0));
        }
        Ok(out)
    }
}

/// Human-readable JSON kind name for error messages.
fn kind_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}
