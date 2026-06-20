//! [`NodeKind::Transform`] executor.
//!
//! Pure (no side effects). Params: `{ "set": { ...fields... } }`. For each input
//! item, the output is the input object merged with `set` (set wins on key
//! collisions). With no `set`, items pass through unchanged.
//!
//! ## Expression engine
//! String values in `set` are passed through [`a2w_expr::render`] so they can
//! reference fields of the current item via `${{ ... }}` expressions:
//! ```json
//! { "set": { "greeting": "${{ \"Hello, \" + $.name }}",
//!            "age_plus_one": "${{ $.age + 1 }}" } }
//! ```
//! Whole-value expressions where the rendered output is a JSON literal
//! (number, boolean, null, object/array) are parsed and substituted as the
//! native JSON value rather than a stringified copy.

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

    async fn execute(&self, ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
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
                // Merge `set` over an object item, evaluating expression
                // strings against the input item.
                (Some(set), serde_json::Value::Object(mut obj)) => {
                    // Evaluate against a snapshot of the input PRE-merge so
                    // an expression like `$.name` sees the original value
                    // even when `set` would overwrite that key.
                    let snapshot = serde_json::Value::Object(obj.clone());
                    for (k, v) in set {
                        let evaluated = render_value(v, &snapshot);
                        obj.insert(k.clone(), evaluated);
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

/// Recursively walk `v`, evaluating any string that contains `${{ ... }}`
/// expressions against `item`.
///
/// **R3 audit-fix**: the JSON-literal substitution only fires when the whole
/// string is exactly one `${{ ... }}` (no surrounding text), so a workflow
/// author's literal string output like `"[1,2,3]"` isn't silently mutated
/// into a 3-element array. Mixed strings (`"prefix${{ expr }}suffix"`) always
/// produce a string result.
fn render_value(v: &serde_json::Value, item: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) if s.contains("${{") => {
            let trimmed = s.trim();
            let is_whole_expr = trimmed.starts_with("${{")
                && trimmed.ends_with("}}")
                // Reject mixed strings: count occurrences of "${{" — must be
                // exactly one — and the leading/trailing whitespace must be
                // the only content outside the markers.
                && trimmed.matches("${{").count() == 1
                && trimmed.matches("}}").count() == 1;
            let rendered = a2w_expr::render(s, item);
            if is_whole_expr {
                // Try parsing the rendered output as JSON. Only substitute
                // the native value when parsing succeeds AND yields a non-
                // string value (so a string-returning expression still
                // produces a string, never accidentally re-interpreted).
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(rendered.trim()) {
                    if !matches!(parsed, serde_json::Value::String(_)) {
                        return parsed;
                    }
                }
            }
            serde_json::Value::String(rendered)
        }
        serde_json::Value::String(_) => v.clone(),
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(|x| render_value(x, item)).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut out = serde_json::Map::with_capacity(obj.len());
            for (k, val) in obj {
                out.insert(k.clone(), render_value(val, item));
            }
            serde_json::Value::Object(out)
        }
        _ => v.clone(),
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
