//! n8n node-type → A2W [`NodeKind`] mapping and per-kind parameter building.
//!
//! The mapping is intentionally *best-effort*: unknown types are preserved as
//! `transform` passthroughs (so the graph stays intact) and flagged with an
//! `UnmappedNodeType` warning rather than aborting the whole import.

use a2w_ir::NodeKind;
use serde_json::{json, Map, Value};

use crate::expr::translate_expr;
use crate::{ImportWarning, WarningKind};

/// Outcome of mapping a single n8n node's type + parameters.
pub(crate) struct MappedNode {
    pub kind: NodeKind,
    pub params: Value,
    /// Warnings produced while building this node's params. The `node` field is
    /// filled in (with the n8n display name) by the caller.
    pub warnings: Vec<ImportWarning>,
}

/// Map an n8n node `type` + `parameters` object to an A2W kind and params.
///
/// `node_name` is the n8n display name, used only to populate warning messages.
pub(crate) fn map_node(node_name: &str, n8n_type: &str, parameters: &Value) -> MappedNode {
    let mut warnings: Vec<ImportWarning> = Vec::new();

    let kind = classify(n8n_type);

    let params = match kind {
        Some(NodeKind::HttpRequest) => build_http(parameters, node_name, &mut warnings),
        Some(NodeKind::Transform) if is_set_type(n8n_type) => {
            build_set(parameters, node_name, &mut warnings)
        }
        Some(NodeKind::Transform) => {
            // noOp / passthrough transforms carry nothing.
            json!({})
        }
        Some(NodeKind::CodeStep) => build_code(parameters),
        Some(NodeKind::LlmCall) => build_llm(parameters, n8n_type),
        // Branch / Switch carry a *required* condition / key+cases under M1.
        // n8n's `if`/`switch` parameter shapes don't map cleanly, so we
        // synthesize a minimal placeholder that is valid-by-construction and
        // flag it for manual review — keeping the imported graph runnable
        // rather than producing an IR the validator (rightly) rejects.
        Some(NodeKind::Branch) => build_branch(parameters, node_name, &mut warnings),
        Some(NodeKind::Switch) => build_switch(parameters, node_name, &mut warnings),
        Some(other) => {
            // Triggers, merge: translate any string params best-effort but
            // otherwise carry the original parameters through.
            best_effort_params(parameters, node_name, &mut warnings, Some(other))
        }
        None => {
            // Unknown type: keep the graph intact as a transform passthrough,
            // preserving the original type + params for traceability, and flag
            // it explicitly.
            warnings.push(ImportWarning {
                node: Some(node_name.to_string()),
                kind: WarningKind::UnmappedNodeType,
                message: format!(
                    "n8n node type '{n8n_type}' has no A2W mapping; imported as a \
                     transform passthrough (params under '_unmapped')"
                ),
            });
            json!({
                "_unmapped": true,
                "original_type": n8n_type,
                "original_parameters": parameters.clone(),
            })
        }
    };

    MappedNode {
        // Unknown types fall back to Transform so the graph is preserved.
        kind: kind.unwrap_or(NodeKind::Transform),
        params,
        warnings,
    }
}

/// Classify an n8n node `type` string into an A2W [`NodeKind`].
///
/// Returns `None` for types with no clean mapping (the caller turns these into
/// a flagged transform passthrough).
fn classify(n8n_type: &str) -> Option<NodeKind> {
    // LangChain / agent / LLM family is matched by substring first, because its
    // type strings are varied (`@n8n/n8n-nodes-langchain.agent`,
    // `...lmChatOpenAi`, etc.).
    let lower = n8n_type.to_ascii_lowercase();
    if n8n_type.starts_with("@n8n/n8n-nodes-langchain.")
        || lower.contains("agent")
        || lower.contains("openai")
        || lower.contains("lmchat")
    {
        return Some(NodeKind::LlmCall);
    }

    match n8n_type {
        "n8n-nodes-base.webhook" => Some(NodeKind::WebhookTrigger),
        "n8n-nodes-base.scheduleTrigger" | "n8n-nodes-base.cron" | "n8n-nodes-base.interval" => {
            Some(NodeKind::ScheduleTrigger)
        }
        "n8n-nodes-base.httpRequest" => Some(NodeKind::HttpRequest),
        "n8n-nodes-base.set" | "n8n-nodes-base.editFields" => Some(NodeKind::Transform),
        "n8n-nodes-base.merge" => Some(NodeKind::Merge),
        "n8n-nodes-base.if" => Some(NodeKind::Branch),
        "n8n-nodes-base.switch" => Some(NodeKind::Switch),
        "n8n-nodes-base.code" | "n8n-nodes-base.function" | "n8n-nodes-base.functionItem" => {
            Some(NodeKind::CodeStep)
        }
        "n8n-nodes-base.noOp" => Some(NodeKind::Transform),
        _ => None,
    }
}

/// Whether the type is one of the n8n "Set"/"Edit Fields" variants (which map
/// to `transform` with extracted assignments rather than a bare passthrough).
fn is_set_type(n8n_type: &str) -> bool {
    matches!(n8n_type, "n8n-nodes-base.set" | "n8n-nodes-base.editFields")
}

/// Build A2W `http_request` params from an n8n httpRequest node.
fn build_http(parameters: &Value, node_name: &str, warnings: &mut Vec<ImportWarning>) -> Value {
    let mut out = Map::new();

    // url
    if let Some(url) = parameters.get("url") {
        out.insert("url".to_string(), translate_value(url, node_name, warnings));
    }

    // method: `method` (v4+) or `requestMethod` (older). Default GET.
    let method = parameters
        .get("method")
        .or_else(|| parameters.get("requestMethod"))
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_string();
    out.insert("method".to_string(), Value::String(method));

    // Best-effort carry of headers and json body if present in any obvious form.
    for key in ["headers", "json", "body", "jsonBody", "queryParameters"] {
        if let Some(v) = parameters.get(key) {
            let translated = translate_value(v, node_name, warnings);
            // Normalize a couple of common aliases onto stable names.
            let out_key = match key {
                "jsonBody" => "json",
                "body" => "body",
                other => other,
            };
            out.entry(out_key.to_string()).or_insert(translated);
        }
    }

    Value::Object(out)
}

/// Build A2W `transform` params from an n8n Set / Edit Fields node.
///
/// n8n stores assignments in a few shapes across versions:
/// - v3+ "assignments": `parameters.assignments.assignments = [{name,value,..}]`
/// - older "values": `parameters.values = { string:[{name,value}], number:[...] }`
///
/// We extract a flat `{ name: value }` object best-effort. If nothing can be
/// extracted, we emit `{ "set": {} }` and flag it.
fn build_set(parameters: &Value, node_name: &str, warnings: &mut Vec<ImportWarning>) -> Value {
    let mut set = Map::new();

    // Shape 1: assignments.assignments = [ { name, value, type }, ... ]
    if let Some(list) = parameters
        .get("assignments")
        .and_then(|a| a.get("assignments"))
        .and_then(Value::as_array)
    {
        for entry in list {
            if let (Some(name), Some(value)) = (
                entry.get("name").and_then(Value::as_str),
                entry.get("value"),
            ) {
                set.insert(
                    name.to_string(),
                    translate_value(value, node_name, warnings),
                );
            }
        }
    }

    // Shape 2: values = { string: [ {name,value} ], number: [...], boolean: [...] }
    if let Some(values) = parameters.get("values").and_then(Value::as_object) {
        for typed_list in values.values() {
            if let Some(arr) = typed_list.as_array() {
                for entry in arr {
                    if let (Some(name), Some(value)) = (
                        entry.get("name").and_then(Value::as_str),
                        entry.get("value"),
                    ) {
                        set.entry(name.to_string())
                            .or_insert_with(|| translate_value(value, node_name, warnings));
                    }
                }
            }
        }
    }

    if set.is_empty() {
        // Could not extract assignments cleanly; keep an empty set and flag it.
        warnings.push(ImportWarning {
            node: Some(node_name.to_string()),
            kind: WarningKind::ExpressionNotTranslated,
            message: format!(
                "could not extract Set/EditFields assignments for node '{node_name}'; \
                 emitted empty 'set'"
            ),
        });
    }

    json!({ "set": Value::Object(set) })
}

/// Build A2W `code_step` params, carrying the original code verbatim.
fn build_code(parameters: &Value) -> Value {
    // n8n stores code under `jsCode`, `functionCode`, or `code` depending on
    // the node/version. Carry whichever is present.
    let code = parameters
        .get("jsCode")
        .or_else(|| parameters.get("functionCode"))
        .or_else(|| parameters.get("code"))
        .cloned();

    let language = parameters
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("javascript")
        .to_string();

    let mut out = Map::new();
    if let Some(code) = code {
        out.insert("code".to_string(), code);
    }
    out.insert("language".to_string(), Value::String(language));
    Value::Object(out)
}

/// Build A2W `llm_call` params best-effort from a LangChain/agent node.
fn build_llm(parameters: &Value, n8n_type: &str) -> Value {
    let mut out = Map::new();
    out.insert(
        "_original_type".to_string(),
        Value::String(n8n_type.to_string()),
    );

    // Carry a couple of commonly-present fields if they exist.
    for key in ["model", "prompt", "text", "messages", "options"] {
        if let Some(v) = parameters.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    Value::Object(out)
}

/// Build params for an n8n `if` → A2W [`NodeKind::Branch`].
///
/// n8n stores `if` conditions under `parameters.conditions` in a shape that
/// does not map cleanly to A2W's `{ path, op, value }`. Rather than emit an IR
/// the M1 validator rejects (Branch requires a `condition`), we synthesize a
/// minimal valid-by-construction placeholder and flag it for review. The
/// original n8n conditions are preserved under `_original_conditions` so a
/// downstream repair step (or human) can reconstruct the real predicate.
fn build_branch(parameters: &Value, node_name: &str, warnings: &mut Vec<ImportWarning>) -> Value {
    warnings.push(ImportWarning {
        node: Some(node_name.to_string()),
        kind: WarningKind::PlaceholderParams,
        message: format!(
            "n8n `if` node '{node_name}' was mapped to a Branch with a placeholder \
             condition (routes every item to the `true` port); replace \
             `params.condition` with the intended predicate. Original n8n \
             conditions preserved under `_original_conditions`."
        ),
    });
    json!({
        // Placeholder: `eq` against a synthetic field that is never present,
        // so by default every item routes to port 1 (false) deterministically.
        // Valid under both the validator and the Branch executor.
        "condition": { "path": "/_a2w_placeholder", "op": "eq", "value": true },
        "_original_conditions": parameters.get("conditions").cloned().unwrap_or(Value::Null),
    })
}

/// Build params for an n8n `switch` → A2W [`NodeKind::Switch`].
///
/// Same rationale as [`build_branch`]: synthesize a minimal valid `key`+`cases`
/// placeholder (empty cases → everything routes to the default port) and flag
/// it. The original n8n params are preserved for repair.
fn build_switch(parameters: &Value, node_name: &str, warnings: &mut Vec<ImportWarning>) -> Value {
    warnings.push(ImportWarning {
        node: Some(node_name.to_string()),
        kind: WarningKind::PlaceholderParams,
        message: format!(
            "n8n `switch` node '{node_name}' was mapped to a Switch with a \
             placeholder key and no cases (routes every item to the default \
             port 0); replace `params.key`/`params.cases` with the intended \
             routing. Original n8n params preserved under `_original_params`."
        ),
    });
    json!({
        "key": "/_a2w_placeholder",
        "cases": [],
        "default_port": 0,
        "_original_params": parameters.clone(),
    })
}

/// For kinds we map structurally (triggers, merge) but whose parameters we do
/// not reshape: carry the original parameters through, while still translating
/// any embedded n8n string expressions best-effort.
fn best_effort_params(
    parameters: &Value,
    node_name: &str,
    warnings: &mut Vec<ImportWarning>,
    kind: Option<NodeKind>,
) -> Value {
    let mut translated = translate_value(parameters, node_name, warnings);
    if let Some(kind) = kind {
        if let Value::Object(map) = &mut translated {
            // Tag non-trivial structural mappings for traceability.
            if matches!(kind, NodeKind::Merge) {
                map.entry("_original_type".to_string())
                    .or_insert(Value::Null);
            }
        }
    }
    translated
}

/// Recursively translate n8n expressions found in string leaves of a JSON
/// value. Non-string leaves are returned unchanged. Any string that looks like
/// an n8n expression (`=`-prefixed) but cannot be fully translated is left
/// as-is and produces a single `ExpressionNotTranslated` warning.
fn translate_value(value: &Value, node_name: &str, warnings: &mut Vec<ImportWarning>) -> Value {
    match value {
        Value::String(s) => {
            // Only attempt translation for `=`-prefixed n8n expressions; plain
            // strings pass through translate_expr unchanged with ok=true.
            let (out, ok) = translate_expr(s);
            if !ok {
                warnings.push(ImportWarning {
                    node: Some(node_name.to_string()),
                    kind: WarningKind::ExpressionNotTranslated,
                    message: format!(
                        "expression '{s}' on node '{node_name}' could not be translated \
                         to A2W template syntax and was left as-is"
                    ),
                });
            }
            Value::String(out)
        }
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|v| translate_value(v, node_name, warnings))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.clone(), translate_value(v, node_name, warnings));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}
