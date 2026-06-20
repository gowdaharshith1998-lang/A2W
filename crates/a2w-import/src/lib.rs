//! # a2w-import
//!
//! Milestone **M6** — a best-effort importer translating an **n8n workflow JSON
//! export** into an [`a2w_ir::Workflow`].
//!
//! The importer never aborts on individual nodes it cannot map cleanly: it
//! keeps the graph structurally intact (unknown node types become flagged
//! `transform` passthroughs) and records every lossy decision as an
//! [`ImportWarning`]. The resulting workflow may not pass A2W validation — for
//! example when the source has zero or several triggers — and that is by
//! design: the warnings make every such gap explicit so a downstream
//! validate→repair loop (or a human) can act on it.
//!
//! ## Entry point
//! [`import_n8n`] takes the raw n8n export JSON and returns an [`ImportResult`]
//! holding the translated workflow plus the warnings.
//!
//! ## n8n shape (parsed loosely via [`serde_json::Value`])
//! ```json
//! { "name": "My WF",
//!   "nodes": [ { "name": "Webhook", "type": "n8n-nodes-base.webhook",
//!                "parameters": {} } ],
//!   "connections": { "Webhook": { "main": [ [ { "node": "HTTP Request",
//!                                               "index": 0 } ] ] } } }
//! ```
//! n8n addresses nodes by **name**; we slugify each name into a stable A2W id
//! and rewire connections accordingly.

#![forbid(unsafe_code)]

mod expr;
mod mapping;
mod slugify;

pub use expr::translate_expr;

use a2w_ir::{Connection, Node, Workflow, SCHEMA_VERSION};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use crate::mapping::map_node;
use crate::slugify::{slugify, SlugAllocator};

/// Error returned when an n8n export cannot be parsed into a workflow at all.
///
/// Note that *node-level* problems are not errors — they surface as
/// [`ImportWarning`]s on a successful [`ImportResult`]. An `ImportError` is
/// reserved for inputs that are not usable JSON or are missing the structural
/// scaffolding (`nodes` array) the importer requires.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ImportError {
    /// The input was not valid JSON.
    #[error("invalid n8n JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The input was valid JSON but not a usable n8n workflow export.
    #[error("malformed n8n export: {0}")]
    Malformed(String),
}

/// Classification of an [`ImportWarning`]. Serialized `snake_case`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningKind {
    /// An n8n node type had no A2W mapping; it was imported as a flagged
    /// `transform` passthrough.
    UnmappedNodeType,
    /// An n8n expression could not be translated to A2W template syntax and was
    /// left as-is.
    ExpressionNotTranslated,
    /// The trigger count is not exactly one (zero, or several). The workflow is
    /// still produced but will not pass A2W validation.
    TriggerIssue,
    /// A connection referenced a node name that did not resolve to any imported
    /// node; the edge was dropped.
    UnsupportedConnection,
}

/// A single non-fatal issue encountered during import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportWarning {
    /// The n8n node display name the warning concerns, when applicable.
    pub node: Option<String>,
    /// Machine-readable classification.
    pub kind: WarningKind,
    /// Human-readable description.
    pub message: String,
}

/// The result of a successful import: the translated workflow plus every
/// best-effort decision recorded as a warning.
#[derive(Debug, Clone, Serialize)]
pub struct ImportResult {
    /// The translated A2W workflow.
    pub workflow: Workflow,
    /// Warnings for everything that did not map cleanly. Empty for a clean
    /// import.
    pub warnings: Vec<ImportWarning>,
}

/// Translate an n8n workflow JSON export into an [`a2w_ir::Workflow`].
///
/// # Errors
/// Returns [`ImportError::Json`] if `json` is not valid JSON, or
/// [`ImportError::Malformed`] if it is valid JSON but lacks the `nodes` array
/// required to form a workflow.
pub fn import_n8n(json: &str) -> Result<ImportResult, ImportError> {
    let root: Value = serde_json::from_str(json)?;

    let obj = root.as_object().ok_or_else(|| {
        ImportError::Malformed("top-level value is not a JSON object".to_string())
    })?;

    let nodes_json = obj
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| ImportError::Malformed("missing or non-array 'nodes' field".to_string()))?;

    let wf_name = obj
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("imported")
        .to_string();
    let wf_id = slugify(&wf_name);

    let mut warnings: Vec<ImportWarning> = Vec::new();
    let mut alloc = SlugAllocator::new();
    let mut nodes: Vec<Node> = Vec::new();
    let mut trigger_count: usize = 0;

    for (idx, node_json) in nodes_json.iter().enumerate() {
        // n8n nodes are objects; skip anything else defensively with a warning.
        let Some(node_obj) = node_json.as_object() else {
            warnings.push(ImportWarning {
                node: None,
                kind: WarningKind::UnmappedNodeType,
                message: format!("nodes[{idx}] is not an object; skipped"),
            });
            continue;
        };

        // A node without a usable name still needs an id; fall back to a
        // positional placeholder so connections that reference real names are
        // unaffected.
        let name = node_obj
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("node_{idx}"));

        let n8n_type = node_obj
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let empty = Value::Object(serde_json::Map::new());
        let parameters = node_obj.get("parameters").unwrap_or(&empty);

        let id = alloc.allocate(&name);

        let mut mapped = map_node(&name, &n8n_type, parameters);
        if mapped.kind.is_trigger() {
            trigger_count += 1;
        }
        warnings.append(&mut mapped.warnings);

        nodes.push(Node {
            id,
            kind: mapped.kind,
            params: mapped.params,
            retry: None,
            on_error: None,
        });
    }

    // --- Trigger sanity: exactly one is required by A2W validation ----------
    if trigger_count != 1 {
        warnings.push(ImportWarning {
            node: None,
            kind: WarningKind::TriggerIssue,
            message: format!(
                "imported workflow has {trigger_count} trigger node(s); A2W requires \
                 exactly one (the workflow may not pass validation as-is)"
            ),
        });
    }

    // --- Connections: n8n addresses by name; rewire by allocated id ---------
    let connections = build_connections(obj.get("connections"), &alloc, &mut warnings);

    let workflow = Workflow {
        schema_version: SCHEMA_VERSION,
        id: wf_id,
        name: wf_name,
        nodes,
        connections,
    };

    Ok(ImportResult { workflow, warnings })
}

/// Translate the n8n `connections` map into A2W [`Connection`]s.
///
/// Shape: `{ SourceName: { "main": [ <port0: [{node,index}]>, <port1: [...]> ] } }`.
/// The outer array index of `main` is the source output port; each inner entry
/// names a target node (by n8n name). Unknown source/target names are skipped
/// with an `UnsupportedConnection` warning.
fn build_connections(
    connections: Option<&Value>,
    alloc: &SlugAllocator,
    warnings: &mut Vec<ImportWarning>,
) -> Vec<Connection> {
    let mut out: Vec<Connection> = Vec::new();

    let Some(conn_obj) = connections.and_then(Value::as_object) else {
        return out;
    };

    for (source_name, ports) in conn_obj {
        // Resolve the source id once; if the source name is unknown, every edge
        // from it is unsupported.
        let Some(from_id) = alloc.id_for(source_name) else {
            warnings.push(ImportWarning {
                node: Some(source_name.clone()),
                kind: WarningKind::UnsupportedConnection,
                message: format!(
                    "connection source '{source_name}' does not match any imported node; \
                     its outgoing connections were dropped"
                ),
            });
            continue;
        };

        // We only model the "main" output type. Other connection types (e.g.
        // ai_tool / ai_languageModel from LangChain nodes) are flagged.
        let Some(main) = ports.get("main").and_then(Value::as_array) else {
            // No main connections to translate for this source.
            if let Some(map) = ports.as_object() {
                for other_type in map.keys().filter(|k| k.as_str() != "main") {
                    warnings.push(ImportWarning {
                        node: Some(source_name.clone()),
                        kind: WarningKind::UnsupportedConnection,
                        message: format!(
                            "connection type '{other_type}' from '{source_name}' is not \
                             supported and was dropped"
                        ),
                    });
                }
            }
            continue;
        };

        for (port_index, targets) in main.iter().enumerate() {
            let Some(targets) = targets.as_array() else {
                continue;
            };
            for target in targets {
                let Some(target_name) = target.get("node").and_then(Value::as_str) else {
                    warnings.push(ImportWarning {
                        node: Some(source_name.clone()),
                        kind: WarningKind::UnsupportedConnection,
                        message: format!(
                            "a connection from '{source_name}' (port {port_index}) had no \
                             target node name and was dropped"
                        ),
                    });
                    continue;
                };
                let Some(to_id) = alloc.id_for(target_name) else {
                    warnings.push(ImportWarning {
                        node: Some(target_name.to_string()),
                        kind: WarningKind::UnsupportedConnection,
                        message: format!(
                            "connection target '{target_name}' (from '{source_name}') does \
                             not match any imported node; the edge was dropped"
                        ),
                    });
                    continue;
                };
                out.push(Connection {
                    from_node: from_id.to_string(),
                    from_port: port_index,
                    to_node: to_id.to_string(),
                });
            }
        }
    }

    out
}

/// Convenience helper: true if any warning has the given kind.
#[cfg(test)]
fn has_warning(result: &ImportResult, kind: WarningKind) -> bool {
    result.warnings.iter().any(|w| w.kind == kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2w_ir::NodeKind;

    fn node_by_id<'a>(wf: &'a Workflow, id: &str) -> &'a Node {
        wf.nodes
            .iter()
            .find(|n| n.id == id)
            .unwrap_or_else(|| panic!("expected node with id '{id}'"))
    }

    /// Test 1: a clean webhook → httpRequest(url literal) → set workflow
    /// imports to three correctly-typed nodes, two id-wired connections, and
    /// passes A2W validation with no warnings.
    #[test]
    fn clean_workflow_imports_and_validates() {
        let json = r#"
        {
          "name": "Clean WF",
          "nodes": [
            { "name": "Webhook", "type": "n8n-nodes-base.webhook", "parameters": {} },
            { "name": "HTTP Request", "type": "n8n-nodes-base.httpRequest",
              "parameters": { "url": "https://example.com/api", "method": "POST" } },
            { "name": "Set", "type": "n8n-nodes-base.set",
              "parameters": { "assignments": { "assignments": [
                { "name": "greeting", "value": "hello" }
              ] } } }
          ],
          "connections": {
            "Webhook": { "main": [ [ { "node": "HTTP Request", "type": "main", "index": 0 } ] ] },
            "HTTP Request": { "main": [ [ { "node": "Set", "type": "main", "index": 0 } ] ] }
          }
        }
        "#;

        let result = import_n8n(json).expect("clean import");
        let wf = &result.workflow;

        assert_eq!(wf.nodes.len(), 3, "expected 3 nodes");
        assert_eq!(node_by_id(wf, "webhook").kind, NodeKind::WebhookTrigger);
        assert_eq!(node_by_id(wf, "http_request").kind, NodeKind::HttpRequest);
        assert_eq!(node_by_id(wf, "set").kind, NodeKind::Transform);

        // http_request params carried url + method.
        let http = node_by_id(wf, "http_request");
        assert_eq!(
            http.params["url"],
            serde_json::json!("https://example.com/api")
        );
        assert_eq!(http.params["method"], serde_json::json!("POST"));

        // set params extracted the assignment.
        let set = node_by_id(wf, "set");
        assert_eq!(set.params["set"]["greeting"], serde_json::json!("hello"));

        // Two connections, wired by id.
        assert_eq!(wf.connections.len(), 2);
        assert!(wf
            .connections
            .contains(&Connection::new("webhook", 0, "http_request")));
        assert!(wf
            .connections
            .contains(&Connection::new("http_request", 0, "set")));

        // No warnings, and the workflow is valid.
        assert!(
            result.warnings.is_empty(),
            "expected no warnings, got: {:?}",
            result.warnings
        );
        let report = a2w_validator::validate(wf);
        assert!(
            report.is_valid,
            "expected valid workflow: {:?}",
            report.findings
        );
    }

    /// Test 2: an unknown node type becomes a flagged `transform` passthrough.
    #[test]
    fn unknown_type_becomes_flagged_transform() {
        let json = r##"
        {
          "name": "Unknown WF",
          "nodes": [
            { "name": "Trigger", "type": "n8n-nodes-base.webhook", "parameters": {} },
            { "name": "Slack", "type": "n8n-nodes-base.slack",
              "parameters": { "channel": "#general", "text": "hi" } }
          ],
          "connections": {
            "Trigger": { "main": [ [ { "node": "Slack", "type": "main", "index": 0 } ] ] }
          }
        }
        "##;

        let result = import_n8n(json).expect("import with unknown node");
        let slack = node_by_id(&result.workflow, "slack");

        assert_eq!(slack.kind, NodeKind::Transform);
        assert_eq!(slack.params["_unmapped"], serde_json::json!(true));
        assert_eq!(
            slack.params["original_type"],
            serde_json::json!("n8n-nodes-base.slack")
        );
        // Original parameters preserved verbatim.
        assert_eq!(
            slack.params["original_parameters"]["channel"],
            serde_json::json!("#general")
        );

        assert!(has_warning(&result, WarningKind::UnmappedNodeType));
        let w = result
            .warnings
            .iter()
            .find(|w| w.kind == WarningKind::UnmappedNodeType)
            .expect("unmapped warning");
        assert_eq!(w.node.as_deref(), Some("Slack"));
        assert!(w.message.contains("n8n-nodes-base.slack"));
    }

    /// Test 3: expression translation — translatable `$json` expr is rewritten
    /// with no warning; a `$node[...]` expr is left as-is with a warning.
    #[test]
    fn expression_translation_behaviour() {
        // Translatable url expression.
        let json_ok = r#"
        {
          "name": "Expr OK",
          "nodes": [
            { "name": "Webhook", "type": "n8n-nodes-base.webhook", "parameters": {} },
            { "name": "HTTP Request", "type": "n8n-nodes-base.httpRequest",
              "parameters": { "url": "={{ $json.repo }}" } }
          ],
          "connections": {
            "Webhook": { "main": [ [ { "node": "HTTP Request", "type": "main", "index": 0 } ] ] }
          }
        }
        "#;
        let result = import_n8n(json_ok).expect("import expr-ok");
        let http = node_by_id(&result.workflow, "http_request");
        assert_eq!(http.params["url"], serde_json::json!("{{json.repo}}"));
        assert!(
            !has_warning(&result, WarningKind::ExpressionNotTranslated),
            "translatable expr should not warn: {:?}",
            result.warnings
        );

        // Untranslatable $node expression.
        let json_bad = r#"
        {
          "name": "Expr Bad",
          "nodes": [
            { "name": "Webhook", "type": "n8n-nodes-base.webhook", "parameters": {} },
            { "name": "HTTP Request", "type": "n8n-nodes-base.httpRequest",
              "parameters": { "url": "={{ $node[\"X\"].json.value }}" } }
          ],
          "connections": {
            "Webhook": { "main": [ [ { "node": "HTTP Request", "type": "main", "index": 0 } ] ] }
          }
        }
        "#;
        let result = import_n8n(json_bad).expect("import expr-bad");
        let http = node_by_id(&result.workflow, "http_request");
        // Left exactly as-is.
        assert_eq!(
            http.params["url"],
            serde_json::json!("={{ $node[\"X\"].json.value }}")
        );
        assert!(has_warning(&result, WarningKind::ExpressionNotTranslated));
    }

    /// Test 4: zero-trigger and multi-trigger fixtures both produce a
    /// `TriggerIssue` warning.
    #[test]
    fn trigger_count_issues_are_flagged() {
        // Zero triggers.
        let zero = r#"
        {
          "name": "No Trigger",
          "nodes": [
            { "name": "A", "type": "n8n-nodes-base.httpRequest", "parameters": {} },
            { "name": "B", "type": "n8n-nodes-base.set", "parameters": {} }
          ],
          "connections": {
            "A": { "main": [ [ { "node": "B", "type": "main", "index": 0 } ] ] }
          }
        }
        "#;
        let result = import_n8n(zero).expect("import zero-trigger");
        assert!(has_warning(&result, WarningKind::TriggerIssue));

        // Multiple triggers.
        let multi = r#"
        {
          "name": "Two Triggers",
          "nodes": [
            { "name": "Hook", "type": "n8n-nodes-base.webhook", "parameters": {} },
            { "name": "Cron", "type": "n8n-nodes-base.scheduleTrigger", "parameters": {} },
            { "name": "Work", "type": "n8n-nodes-base.set", "parameters": {} }
          ],
          "connections": {
            "Hook": { "main": [ [ { "node": "Work", "type": "main", "index": 0 } ] ] },
            "Cron": { "main": [ [ { "node": "Work", "type": "main", "index": 0 } ] ] }
          }
        }
        "#;
        let result = import_n8n(multi).expect("import multi-trigger");
        assert!(has_warning(&result, WarningKind::TriggerIssue));
    }

    /// Extra: connections to unknown node names are dropped with a warning, and
    /// port indices map from the n8n `main` outer-array index.
    #[test]
    fn unknown_connection_target_is_dropped() {
        let json = r#"
        {
          "name": "Dangling",
          "nodes": [
            { "name": "Webhook", "type": "n8n-nodes-base.webhook", "parameters": {} }
          ],
          "connections": {
            "Webhook": { "main": [ [ { "node": "Ghost", "type": "main", "index": 0 } ] ] }
          }
        }
        "#;
        let result = import_n8n(json).expect("import dangling");
        assert!(result.workflow.connections.is_empty());
        assert!(has_warning(&result, WarningKind::UnsupportedConnection));
    }

    /// Extra: branch port mapping — n8n `if` second `main` array (index 1) maps
    /// to A2W from_port 1.
    #[test]
    fn branch_port_indices_map_from_main_array() {
        let json = r#"
        {
          "name": "Branchy",
          "nodes": [
            { "name": "Webhook", "type": "n8n-nodes-base.webhook", "parameters": {} },
            { "name": "If", "type": "n8n-nodes-base.if", "parameters": {} },
            { "name": "T", "type": "n8n-nodes-base.set", "parameters": {} },
            { "name": "F", "type": "n8n-nodes-base.set", "parameters": {} }
          ],
          "connections": {
            "Webhook": { "main": [ [ { "node": "If", "type": "main", "index": 0 } ] ] },
            "If": { "main": [
              [ { "node": "T", "type": "main", "index": 0 } ],
              [ { "node": "F", "type": "main", "index": 0 } ]
            ] }
          }
        }
        "#;
        let result = import_n8n(json).expect("import branch");
        let wf = &result.workflow;
        assert_eq!(node_by_id(wf, "if").kind, NodeKind::Branch);
        assert!(wf.connections.contains(&Connection::new("if", 0, "t")));
        assert!(wf.connections.contains(&Connection::new("if", 1, "f")));
        // The whole thing should validate (single trigger, branch ports 0/1 ok).
        assert!(a2w_validator::validate(wf).is_valid);
    }

    #[test]
    fn invalid_json_errors() {
        assert!(matches!(
            import_n8n("{ not json"),
            Err(ImportError::Json(_))
        ));
    }

    #[test]
    fn missing_nodes_is_malformed() {
        assert!(matches!(
            import_n8n(r#"{ "name": "x" }"#),
            Err(ImportError::Malformed(_))
        ));
    }
}
