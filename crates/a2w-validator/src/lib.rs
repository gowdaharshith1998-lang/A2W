//! # a2w-validator
//!
//! Deterministic structural and semantic validation for the [`a2w_ir`]
//! workflow IR.
//!
//! [`validate`] returns a [`ValidationReport`] of **located, fix-suggesting**
//! findings in a **stable order**, so the same workflow always produces the
//! same report — a property the validate→repair loop relies on.
//!
//! ## Checks (M1)
//! 1. Empty workflow (no nodes) → `Error` `EmptyWorkflow`.
//! 2. Duplicate node ids → one `Error` `DuplicateNodeId` per offending id.
//! 3. Trigger count: zero → `Error` `NoTrigger`; more than one → `Error`
//!    `MultipleTriggers`.
//! 4. Dangling connection endpoints → `Error`
//!    `DanglingConnectionSource` / `DanglingConnectionTarget`.
//! 5. Out-of-range `from_port` → `Error` `InvalidOutputPort` (skipped for
//!    dynamic-port kinds, i.e. `Switch`).
//! 6. Directed cycle → `Error` `Cycle`.
//! 7. Unreachable nodes → `Warning` `UnreachableNode`.
//! 8. Sub-workflow self-reference → `Error` `SubWorkflowSelfReference`.
//! 9. Per-kind required-field / role checks → `Error` `MissingRequiredParam` /
//!    `InvalidParamType` / `TriggerHasIncomingConnection` (M1 reject-before-
//!    execute layer: invalid IR fails at authoring time so the executor may
//!    henceforth assume validity as a precondition).

#![forbid(unsafe_code)]

mod report;

pub use report::{Finding, FindingCode, Location, Severity, ValidationReport};

/// All workflow ids reachable from `wf` via `SubWorkflow.workflow_id` params
/// (one hop only — for cross-workflow cycle detection the caller traverses
/// the resulting set transitively). Returns the set in declaration order.
#[must_use]
pub fn sub_workflow_references(wf: &Workflow) -> Vec<String> {
    let mut refs: Vec<String> = Vec::new();
    for n in &wf.nodes {
        if n.kind != a2w_ir::NodeKind::SubWorkflow {
            continue;
        }
        if let Some(id) = n.params.get("workflow_id").and_then(|v| v.as_str()) {
            if !refs.iter().any(|r| r == id) {
                refs.push(id.to_string());
            }
        }
        // Inline `workflow` form: also report any sub_workflow references
        // it contains (so the inline workflow's cycle risks are visible to
        // the cross-workflow walker).
        if let Some(inline) = n.params.get("workflow") {
            if let Ok(sub_wf) = serde_json::from_value::<Workflow>(inline.clone()) {
                for child in sub_workflow_references(&sub_wf) {
                    if !refs.iter().any(|r| r == &child) {
                        refs.push(child);
                    }
                }
            }
        }
    }
    refs
}

use std::collections::{HashMap, HashSet, VecDeque};

use a2w_ir::{Node, Workflow};
use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};

/// Validate a workflow, returning a deterministic report.
///
/// The report's `findings` are sorted by `(severity, code, location, message)`,
/// and `is_valid` is `true` iff no finding has `Severity::Error`.
#[must_use]
pub fn validate(wf: &Workflow) -> ValidationReport {
    let mut findings: Vec<Finding> = Vec::new();

    // --- Check 1: empty workflow ------------------------------------------
    if wf.nodes.is_empty() {
        findings.push(Finding {
            severity: Severity::Error,
            code: FindingCode::EmptyWorkflow,
            message: "workflow has no nodes".to_string(),
            location: Location::Workflow,
            suggestion: Some(
                "add at least one trigger node (e.g. webhook_trigger) to start the workflow"
                    .to_string(),
            ),
        });
        // Nothing else is meaningful without nodes.
        return ValidationReport::from_findings(findings);
    }

    // Index nodes by id. The first occurrence of each id wins for lookups; the
    // duplicate-id check below reports the collisions separately.
    let mut by_id: HashMap<&str, &Node> = HashMap::new();
    let mut id_counts: HashMap<&str, usize> = HashMap::new();
    for node in &wf.nodes {
        *id_counts.entry(node.id.as_str()).or_insert(0) += 1;
        by_id.entry(node.id.as_str()).or_insert(node);
    }

    // --- Check 2: duplicate node ids --------------------------------------
    // One finding per offending id, located on the node. Sort the ids so the
    // (pre-sort) emission is itself stable; the report re-sorts regardless.
    let mut dup_ids: Vec<&str> = id_counts
        .iter()
        .filter(|(_, &count)| count > 1)
        .map(|(&id, _)| id)
        .collect();
    dup_ids.sort_unstable();
    for id in dup_ids {
        let count = id_counts[id];
        findings.push(Finding {
            severity: Severity::Error,
            code: FindingCode::DuplicateNodeId,
            message: format!("node id '{id}' is used {count} times; ids must be unique"),
            location: Location::Node(id.to_string()),
            suggestion: Some(format!(
                "rename the duplicate node(s) so that '{id}' identifies exactly one node"
            )),
        });
    }

    // --- Check 3: trigger count -------------------------------------------
    // Decision: more than one trigger is an ERROR (not a warning). A workflow
    // has a single entry point in M1; multiple triggers make "reachable from
    // the trigger" ambiguous, so we treat it as blocking.
    let triggers: Vec<&Node> = wf.nodes.iter().filter(|n| n.kind.is_trigger()).collect();
    match triggers.len() {
        0 => findings.push(Finding {
            severity: Severity::Error,
            code: FindingCode::NoTrigger,
            message: "workflow has no trigger node (exactly one is required)".to_string(),
            location: Location::Workflow,
            suggestion: Some(
                "add a webhook_trigger or schedule_trigger node as the entry point".to_string(),
            ),
        }),
        1 => {}
        n => {
            let mut ids: Vec<&str> = triggers.iter().map(|t| t.id.as_str()).collect();
            ids.sort_unstable();
            findings.push(Finding {
                severity: Severity::Error,
                code: FindingCode::MultipleTriggers,
                message: format!(
                    "workflow has {n} trigger nodes ({}); exactly one is required",
                    ids.join(", ")
                ),
                location: Location::Workflow,
                suggestion: Some(
                    "keep a single trigger; convert the others to non-trigger nodes or remove them"
                        .to_string(),
                ),
            });
        }
    }

    // --- Checks 4 & 5: connection endpoints and ports ---------------------
    // Track which connections are structurally sound so cycle/reachability only
    // consider real edges.
    let mut valid_edges: Vec<(&str, &str)> = Vec::new();
    for conn in &wf.connections {
        let loc = Location::Connection {
            from_node: conn.from_node.clone(),
            from_port: conn.from_port,
            to_node: conn.to_node.clone(),
        };

        let source = by_id.get(conn.from_node.as_str()).copied();
        let target_exists = by_id.contains_key(conn.to_node.as_str());

        if source.is_none() {
            findings.push(Finding {
                severity: Severity::Error,
                code: FindingCode::DanglingConnectionSource,
                message: format!(
                    "connection's from_node '{}' does not match any node id",
                    conn.from_node
                ),
                location: loc.clone(),
                suggestion: Some(format!(
                    "point from_node at an existing node, or remove this connection ('{}' -> '{}')",
                    conn.from_node, conn.to_node
                )),
            });
        }

        if !target_exists {
            findings.push(Finding {
                severity: Severity::Error,
                code: FindingCode::DanglingConnectionTarget,
                message: format!(
                    "connection's to_node '{}' does not match any node id",
                    conn.to_node
                ),
                location: loc.clone(),
                suggestion: Some(format!(
                    "point to_node at an existing node, or remove this connection ('{}' -> '{}')",
                    conn.from_node, conn.to_node
                )),
            });
        }

        // Port range check only makes sense when the source node exists.
        let mut port_ok = true;
        if let Some(src) = source {
            if !src.kind.has_dynamic_ports() {
                let ports = src.kind.output_port_count();
                if conn.from_port >= ports {
                    port_ok = false;
                    findings.push(Finding {
                        severity: Severity::Error,
                        code: FindingCode::InvalidOutputPort,
                        message: format!(
                            "node '{}' (kind exposes {ports} output port(s)) has no port \
                             index {}; valid indices are 0..{}",
                            conn.from_node, conn.from_port, ports
                        ),
                        location: loc.clone(),
                        suggestion: Some(format!(
                            "use a from_port in 0..{ports} for node '{}'",
                            conn.from_node
                        )),
                    });
                }
            }
        }

        // An edge participates in graph analysis only if both endpoints exist
        // and the port is acceptable.
        if source.is_some() && target_exists && port_ok {
            valid_edges.push((conn.from_node.as_str(), conn.to_node.as_str()));
        }
    }

    // --- Check 6: cycle detection (petgraph) ------------------------------
    // Build a DiGraph over node ids using only structurally-valid edges.
    let mut graph: DiGraph<&str, ()> = DiGraph::new();
    let mut idx_of: HashMap<&str, NodeIndex> = HashMap::new();
    // Insert nodes in declaration order for deterministic indices.
    for node in &wf.nodes {
        // If ids are duplicated, only the first gets a graph node; that is fine
        // because duplicate ids are already a reported Error.
        idx_of
            .entry(node.id.as_str())
            .or_insert_with(|| graph.add_node(node.id.as_str()));
    }
    for (from, to) in &valid_edges {
        if let (Some(&fi), Some(&ti)) = (idx_of.get(*from), idx_of.get(*to)) {
            graph.add_edge(fi, ti, ());
        }
    }

    let has_cycle = match toposort(&graph, None) {
        Ok(_) => false,
        Err(cycle) => {
            // `cycle` reports one node participating in a cycle. Locate it.
            let id = graph[cycle.node_id()];
            findings.push(Finding {
                severity: Severity::Error,
                code: FindingCode::Cycle,
                message: format!(
                    "workflow contains a cycle through node '{id}'; the graph must be acyclic"
                ),
                location: Location::Node(id.to_string()),
                suggestion: Some(format!(
                    "break the loop involving node '{id}' (use a loop node for intended \
                     iteration rather than a back-edge)"
                )),
            });
            true
        }
    };

    // --- Check 7: unreachable nodes (Warning) -----------------------------
    // Reachability is only meaningful when the graph is well-formed enough to
    // trust: skip if there is no usable trigger or a cycle was found. Dangling
    // connections are already excluded from `valid_edges`, so they don't
    // corrupt the traversal.
    let single_trigger = triggers.len() == 1;
    if single_trigger && !has_cycle {
        // Adjacency over valid edges.
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        for (from, to) in &valid_edges {
            adj.entry(*from).or_default().push(*to);
        }

        let start = triggers[0].id.as_str();
        let mut reachable: HashSet<&str> = HashSet::new();
        let mut queue: VecDeque<&str> = VecDeque::new();
        reachable.insert(start);
        queue.push_back(start);
        while let Some(cur) = queue.pop_front() {
            if let Some(neighbours) = adj.get(cur) {
                for &next in neighbours {
                    if reachable.insert(next) {
                        queue.push_back(next);
                    }
                }
            }
        }

        // Report unreachable, non-duplicate nodes. Iterate declaration order
        // and dedupe ids so each id yields at most one finding.
        let mut seen: HashSet<&str> = HashSet::new();
        for node in &wf.nodes {
            let id = node.id.as_str();
            if !seen.insert(id) {
                continue; // duplicate id already reported elsewhere
            }
            if !reachable.contains(id) {
                findings.push(Finding {
                    severity: Severity::Warning,
                    code: FindingCode::UnreachableNode,
                    message: format!("node '{id}' is not reachable from the trigger '{start}'"),
                    location: Location::Node(id.to_string()),
                    suggestion: Some(format!(
                        "connect '{id}' to the flow originating at trigger '{start}', \
                         or remove it"
                    )),
                });
            }
        }
    }

    // --- Check 8 (R3 audit-fix): SubWorkflow self-reference -----------------
    // A SubWorkflow node that names its enclosing workflow by id would loop
    // until the runtime depth cap fires. Reject at save time so the operator
    // sees a clear error.
    for node in &wf.nodes {
        if node.kind != a2w_ir::NodeKind::SubWorkflow {
            continue;
        }
        let workflow_id = node.params.get("workflow_id").and_then(|v| v.as_str());
        if workflow_id == Some(wf.id.as_str()) {
            findings.push(Finding {
                severity: Severity::Error,
                code: FindingCode::SubWorkflowSelfReference,
                message: format!(
                    "sub_workflow node '{}' references the enclosing workflow '{}' \
                     by id — would loop until the runtime depth cap",
                    node.id, wf.id
                ),
                location: Location::Node(node.id.clone()),
                suggestion: Some(
                    "remove the self-reference, or use a distinct workflow_id".to_string(),
                ),
            });
        }
        // Also detect inline workflow that re-uses the same id.
        if let Some(inline) = node.params.get("workflow") {
            if let Some(inline_id) = inline.get("id").and_then(|v| v.as_str()) {
                if inline_id == wf.id.as_str() {
                    findings.push(Finding {
                        severity: Severity::Error,
                        code: FindingCode::SubWorkflowSelfReference,
                        message: format!(
                            "sub_workflow node '{}' inlines a workflow whose id \
                             matches the enclosing workflow '{}' — would loop",
                            node.id, wf.id
                        ),
                        location: Location::Node(node.id.clone()),
                        suggestion: Some(
                            "use a distinct id in the inline workflow's `id` field".to_string(),
                        ),
                    });
                }
            }
        }
    }

    // --- Check 9 (M1): per-kind required-field / role checks --------------
    // The executor would reject these at runtime with `BadParams`. The
    // validator surfaces them at authoring time so the agent's search space
    // is safe by construction and the executor may treat validity as a
    // precondition.
    //
    // We err on the side of being USEFUL rather than COMPLETE: only
    // unambiguous violations (a missing url on http_request, etc.) become
    // errors. Anything that the executor accepts (defaults, optional fields)
    // is left alone.
    let mut targets: HashSet<&str> = HashSet::new();
    for conn in &wf.connections {
        targets.insert(conn.to_node.as_str());
    }
    for node in &wf.nodes {
        check_node_params(node, &targets, &mut findings);
    }

    ValidationReport::from_findings(findings)
}

/// Per-kind required-field / role check. Pushes into `findings`.
///
/// Decoupled from the rest of validate() so each kind's rule is auditable in
/// one place — and so M5's mutation operators can call it on a synthesized
/// node without re-walking the whole graph.
fn check_node_params(node: &Node, targets: &HashSet<&str>, findings: &mut Vec<Finding>) {
    use a2w_ir::NodeKind as K;
    let id = node.id.as_str();
    let p = &node.params;

    // Triggers must not have incoming connections — they are entry points.
    if node.kind.is_trigger() && targets.contains(id) {
        findings.push(Finding {
            severity: Severity::Error,
            code: FindingCode::TriggerHasIncomingConnection,
            message: format!(
                "trigger '{id}' has incoming connection(s); triggers are entry \
                 points and only emit"
            ),
            location: Location::Node(id.to_string()),
            suggestion: Some(format!(
                "remove the incoming connection(s) to '{id}', or replace the \
                 trigger with a non-trigger kind if intermediate routing was intended"
            )),
        });
    }

    match node.kind {
        K::HttpRequest => {
            match p.get("url") {
                None => missing(
                    findings,
                    id,
                    "url",
                    "http_request requires a `url` string param",
                ),
                Some(serde_json::Value::String(s)) if s.is_empty() => bad_type(
                    findings,
                    id,
                    "url",
                    "http_request `url` must be a non-empty string",
                ),
                Some(serde_json::Value::String(_)) => {}
                Some(_) => bad_type(findings, id, "url", "http_request `url` must be a string"),
            }
            if let Some(method) = p.get("method") {
                if !method.is_string() {
                    bad_type(
                        findings,
                        id,
                        "method",
                        "http_request `method` must be a string",
                    );
                }
            }
            if let Some(headers) = p.get("headers") {
                if !headers.is_object() {
                    bad_type(
                        findings,
                        id,
                        "headers",
                        "http_request `headers` must be an object",
                    );
                }
            }
        }
        K::Transform => {
            // `set` is the canonical shaping field; if present it must be an
            // object. Empty params is allowed (passes items through).
            if let Some(set) = p.get("set") {
                if !set.is_object() {
                    bad_type(findings, id, "set", "transform `set` must be a JSON object");
                }
            }
        }
        K::Branch => match p.get("condition") {
            None => missing(findings, id, "condition", "branch requires a `condition`"),
            Some(serde_json::Value::String(_)) => {} // shorthand path
            Some(serde_json::Value::Object(o)) => {
                if !o.contains_key("path") {
                    missing(
                        findings,
                        id,
                        "condition.path",
                        "branch `condition` object must contain a `path`",
                    );
                } else if !o["path"].is_string() {
                    bad_type(
                        findings,
                        id,
                        "condition.path",
                        "branch `condition.path` must be a string",
                    );
                }
                if let Some(op) = o.get("op") {
                    if !op.is_string() {
                        bad_type(
                            findings,
                            id,
                            "condition.op",
                            "branch `condition.op` must be a string (truthy|eq|ne|contains)",
                        );
                    }
                }
            }
            Some(_) => bad_type(
                findings,
                id,
                "condition",
                "branch `condition` must be a JSON pointer string or { path, op?, value? }",
            ),
        },
        K::Switch => {
            match p.get("key") {
                None => missing(findings, id, "key", "switch requires a `key` JSON pointer"),
                Some(serde_json::Value::String(_)) => {}
                Some(_) => bad_type(findings, id, "key", "switch `key` must be a string"),
            }
            match p.get("cases") {
                None => missing(findings, id, "cases", "switch requires a `cases` array"),
                Some(serde_json::Value::Array(arr)) => {
                    for (i, c) in arr.iter().enumerate() {
                        let Some(obj) = c.as_object() else {
                            bad_type(
                                findings,
                                id,
                                &format!("cases[{i}]"),
                                "each switch case must be an object { value, port }",
                            );
                            continue;
                        };
                        if !obj.contains_key("value") {
                            missing(
                                findings,
                                id,
                                &format!("cases[{i}].value"),
                                "each switch case must include a `value`",
                            );
                        }
                        match obj.get("port") {
                            None => missing(
                                findings,
                                id,
                                &format!("cases[{i}].port"),
                                "each switch case must include a `port` (non-negative integer)",
                            ),
                            Some(v) if v.as_u64().is_none() => bad_type(
                                findings,
                                id,
                                &format!("cases[{i}].port"),
                                "switch case `port` must be a non-negative integer",
                            ),
                            _ => {}
                        }
                    }
                }
                Some(_) => bad_type(findings, id, "cases", "switch `cases` must be an array"),
            }
        }
        K::Loop => match p.get("over") {
            None => missing(findings, id, "over", "loop requires an `over` JSON pointer"),
            Some(serde_json::Value::String(_)) => {}
            Some(_) => bad_type(findings, id, "over", "loop `over` must be a string"),
        },
        K::Wait => match p.get("duration_ms") {
            None => missing(
                findings,
                id,
                "duration_ms",
                "wait requires a `duration_ms` non-negative integer",
            ),
            Some(v) if v.as_u64().is_none() => bad_type(
                findings,
                id,
                "duration_ms",
                "wait `duration_ms` must be a non-negative integer",
            ),
            _ => {}
        },
        K::SubWorkflow => {
            let has_id = p.get("workflow_id").is_some();
            let has_inline = p.get("workflow").is_some();
            if !has_id && !has_inline {
                missing(
                    findings,
                    id,
                    "workflow_id|workflow",
                    "sub_workflow requires either `workflow_id` or an inline `workflow`",
                );
            }
            if has_id && !p["workflow_id"].is_string() {
                bad_type(
                    findings,
                    id,
                    "workflow_id",
                    "sub_workflow `workflow_id` must be a string",
                );
            }
            if has_inline && !p["workflow"].is_object() {
                bad_type(
                    findings,
                    id,
                    "workflow",
                    "sub_workflow inline `workflow` must be an object",
                );
            }
        }
        K::LlmCall => match p.get("prompt") {
            None => missing(
                findings,
                id,
                "prompt",
                "llm_call requires a `prompt` string",
            ),
            Some(serde_json::Value::String(_)) => {}
            Some(_) => bad_type(findings, id, "prompt", "llm_call `prompt` must be a string"),
        },
        K::McpToolCall => {
            if p.get("server").is_none() && p.get("command").is_none() {
                missing(
                    findings,
                    id,
                    "server|command",
                    "mcp_tool_call requires a `server` spec or a `command`",
                );
            }
            if p.get("tool").is_none() {
                missing(findings, id, "tool", "mcp_tool_call requires a `tool` name");
            }
        }
        K::CodeStep => {
            // CodeStep runs a sandboxed WASM module: it needs a `wasm` source
            // ({ base64 } or { path }) plus a `function` export name.
            match p.get("wasm") {
                None => missing(
                    findings,
                    id,
                    "wasm",
                    "code_step requires a `wasm` source ({ base64} or { path })",
                ),
                Some(serde_json::Value::Object(w)) => {
                    if !w.contains_key("base64") && !w.contains_key("path") {
                        missing(
                            findings,
                            id,
                            "wasm.base64|wasm.path",
                            "code_step `wasm` must contain `base64` or `path`",
                        );
                    }
                }
                Some(_) => bad_type(
                    findings,
                    id,
                    "wasm",
                    "code_step `wasm` must be an object ({ base64 } or { path })",
                ),
            }
            match p.get("function") {
                None => missing(
                    findings,
                    id,
                    "function",
                    "code_step requires a `function` export name",
                ),
                Some(v) if !v.is_string() => bad_type(
                    findings,
                    id,
                    "function",
                    "code_step `function` must be a string",
                ),
                _ => {}
            }
        }
        K::Approval => {
            // Approval has only optional params; the runtime fills defaults.
        }
        K::Merge | K::WebhookTrigger | K::ScheduleTrigger => {
            // No required params at the validator layer.
        }
    }
}

/// Helper: emit a `MissingRequiredParam` finding.
fn missing(findings: &mut Vec<Finding>, node_id: &str, field: &str, msg: &str) {
    findings.push(Finding {
        severity: Severity::Error,
        code: FindingCode::MissingRequiredParam,
        message: format!("node '{node_id}': {msg} (missing `{field}`)"),
        location: Location::Node(node_id.to_string()),
        suggestion: Some(format!("set `params.{field}` on node '{node_id}'")),
    });
}

/// Helper: emit an `InvalidParamType` finding.
fn bad_type(findings: &mut Vec<Finding>, node_id: &str, field: &str, msg: &str) {
    findings.push(Finding {
        severity: Severity::Error,
        code: FindingCode::InvalidParamType,
        message: format!("node '{node_id}': {msg}"),
        location: Location::Node(node_id.to_string()),
        suggestion: Some(format!(
            "give `params.{field}` of '{node_id}' the expected JSON type"
        )),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2w_ir::{Connection, Node, NodeKind, Workflow, SCHEMA_VERSION};

    /// Build a workflow from nodes + connections with boilerplate filled in.
    fn wf(nodes: Vec<Node>, connections: Vec<Connection>) -> Workflow {
        Workflow {
            schema_version: SCHEMA_VERSION,
            id: "wf_test".to_string(),
            name: "test".to_string(),
            nodes,
            connections,
        }
    }

    fn codes(report: &ValidationReport) -> Vec<FindingCode> {
        report.findings.iter().map(|f| f.code).collect()
    }

    fn finding(report: &ValidationReport, code: FindingCode) -> &Finding {
        report
            .findings
            .iter()
            .find(|f| f.code == code)
            .unwrap_or_else(|| panic!("expected a finding with code {code:?}"))
    }

    #[test]
    fn sample_workflow_is_valid() {
        let report = validate(&a2w_ir::sample_workflow());
        let errors: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .collect();
        assert!(
            errors.is_empty(),
            "sample workflow should have no errors, got: {errors:?}"
        );
        assert!(report.is_valid);
    }

    #[test]
    fn empty_workflow() {
        let report = validate(&wf(vec![], vec![]));
        assert_eq!(codes(&report), vec![FindingCode::EmptyWorkflow]);
        assert!(!report.is_valid);
        assert_eq!(
            finding(&report, FindingCode::EmptyWorkflow).location,
            Location::Workflow
        );
    }

    #[test]
    fn duplicate_node_id() {
        let report = validate(&wf(
            vec![
                Node::new("trigger", NodeKind::WebhookTrigger),
                Node::new("dup", NodeKind::HttpRequest),
                Node::new("dup", NodeKind::Transform),
            ],
            vec![Connection::new("trigger", 0, "dup")],
        ));
        let f = finding(&report, FindingCode::DuplicateNodeId);
        assert_eq!(f.severity, Severity::Error);
        assert!(
            f.message.contains("dup"),
            "message should mention 'dup': {}",
            f.message
        );
        assert_eq!(f.location, Location::Node("dup".to_string()));
        assert!(!report.is_valid);
    }

    #[test]
    fn no_trigger() {
        let report = validate(&wf(
            vec![
                Node::new("a", NodeKind::HttpRequest),
                Node::new("b", NodeKind::Transform),
            ],
            vec![Connection::new("a", 0, "b")],
        ));
        let f = finding(&report, FindingCode::NoTrigger);
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(f.location, Location::Workflow);
        assert!(!report.is_valid);
    }

    #[test]
    fn multiple_triggers() {
        let report = validate(&wf(
            vec![
                Node::new("t1", NodeKind::WebhookTrigger),
                Node::new("t2", NodeKind::ScheduleTrigger),
                Node::new("work", NodeKind::Transform),
            ],
            vec![
                Connection::new("t1", 0, "work"),
                Connection::new("t2", 0, "work"),
            ],
        ));
        let f = finding(&report, FindingCode::MultipleTriggers);
        assert_eq!(f.severity, Severity::Error);
        assert!(f.message.contains("t1") && f.message.contains("t2"));
        assert!(!report.is_valid);
    }

    #[test]
    fn dangling_connection_source() {
        let report = validate(&wf(
            vec![
                Node::new("trigger", NodeKind::WebhookTrigger),
                Node::new("dst", NodeKind::Transform),
            ],
            vec![Connection::new("ghost", 0, "dst")],
        ));
        let f = finding(&report, FindingCode::DanglingConnectionSource);
        assert_eq!(f.severity, Severity::Error);
        assert!(
            f.message.contains("ghost"),
            "message should mention 'ghost': {}",
            f.message
        );
        assert!(!report.is_valid);
    }

    #[test]
    fn dangling_connection_target() {
        let report = validate(&wf(
            vec![Node::new("trigger", NodeKind::WebhookTrigger)],
            vec![Connection::new("trigger", 0, "ghost")],
        ));
        let f = finding(&report, FindingCode::DanglingConnectionTarget);
        assert_eq!(f.severity, Severity::Error);
        assert!(
            f.message.contains("ghost"),
            "message should mention 'ghost': {}",
            f.message
        );
        assert!(!report.is_valid);
    }

    #[test]
    fn invalid_output_port() {
        // Branch exposes ports 0 and 1; port 2 is invalid.
        let report = validate(&wf(
            vec![
                Node::new("trigger", NodeKind::WebhookTrigger),
                Node::new("br", NodeKind::Branch),
                Node::new("sink", NodeKind::Transform),
            ],
            vec![
                Connection::new("trigger", 0, "br"),
                Connection::new("br", 2, "sink"),
            ],
        ));
        let f = finding(&report, FindingCode::InvalidOutputPort);
        assert_eq!(f.severity, Severity::Error);
        assert!(
            f.message.contains("br"),
            "message should mention 'br': {}",
            f.message
        );
        assert!(!report.is_valid);
    }

    #[test]
    fn switch_dynamic_ports_are_not_flagged() {
        // Switch has dynamic ports, so a large port index must NOT be flagged.
        let mut sw = Node::new("sw", NodeKind::Switch);
        sw.params = serde_json::json!({
            "key": "/k",
            "cases": [{ "value": "x", "port": 7 }]
        });
        let report = validate(&wf(
            vec![
                Node::new("trigger", NodeKind::WebhookTrigger),
                sw,
                Node::new("sink", NodeKind::Transform),
            ],
            vec![
                Connection::new("trigger", 0, "sw"),
                Connection::new("sw", 7, "sink"),
            ],
        ));
        assert!(
            !codes(&report).contains(&FindingCode::InvalidOutputPort),
            "switch ports should not be flagged: {:?}",
            report.findings
        );
        assert!(report.is_valid, "report: {:?}", report.findings);
    }

    #[test]
    fn cycle() {
        // trigger -> a -> b -> a   (a<->b cycle)
        let report = validate(&wf(
            vec![
                Node::new("trigger", NodeKind::WebhookTrigger),
                Node::new("a", NodeKind::Transform),
                Node::new("b", NodeKind::Transform),
            ],
            vec![
                Connection::new("trigger", 0, "a"),
                Connection::new("a", 0, "b"),
                Connection::new("b", 0, "a"),
            ],
        ));
        let f = finding(&report, FindingCode::Cycle);
        assert_eq!(f.severity, Severity::Error);
        assert!(
            matches!(&f.location, Location::Node(id) if id == "a" || id == "b"),
            "cycle should be located on a node in the cycle: {:?}",
            f.location
        );
        assert!(!report.is_valid);
    }

    #[test]
    fn unreachable_node() {
        // 'island' is valid but not connected to the trigger flow.
        let mut island = Node::new("island", NodeKind::HttpRequest);
        island.params = serde_json::json!({ "url": "https://example.com" });
        let report = validate(&wf(
            vec![
                Node::new("trigger", NodeKind::WebhookTrigger),
                Node::new("step", NodeKind::Transform),
                island,
            ],
            vec![Connection::new("trigger", 0, "step")],
        ));
        let f = finding(&report, FindingCode::UnreachableNode);
        assert_eq!(f.severity, Severity::Warning);
        assert!(
            f.message.contains("island"),
            "message should mention 'island': {}",
            f.message
        );
        assert_eq!(f.location, Location::Node("island".to_string()));
        // Warnings do not invalidate the workflow.
        assert!(report.is_valid);
    }

    #[test]
    fn findings_are_sorted_deterministically() {
        // A messy workflow that triggers several findings; running twice must
        // produce identical reports.
        let build = || {
            wf(
                vec![
                    Node::new("dup", NodeKind::HttpRequest),
                    Node::new("dup", NodeKind::Transform),
                    Node::new("br", NodeKind::Branch),
                ],
                vec![
                    Connection::new("br", 5, "dup"),
                    Connection::new("missing", 0, "dup"),
                ],
            )
        };
        let r1 = validate(&build());
        let r2 = validate(&build());
        assert_eq!(r1.findings, r2.findings);
        // Errors must all come before any warnings.
        let first_warning = r1
            .findings
            .iter()
            .position(|f| f.severity == Severity::Warning);
        let last_error = r1
            .findings
            .iter()
            .rposition(|f| f.severity == Severity::Error);
        if let (Some(fw), Some(le)) = (first_warning, last_error) {
            assert!(le < fw, "all errors must precede warnings");
        }
    }

    #[test]
    fn report_serializes_to_json() {
        let report = validate(&a2w_ir::sample_workflow());
        let json = serde_json::to_string(&report).expect("serialize report");
        assert!(json.contains("findings"));
        assert!(json.contains("is_valid"));
    }

    // -------- M1: per-kind required-field / role checks --------

    #[test]
    fn http_request_missing_url_is_error() {
        let report = validate(&wf(
            vec![
                Node::new("trigger", NodeKind::WebhookTrigger),
                Node::new("fetch", NodeKind::HttpRequest),
            ],
            vec![Connection::new("trigger", 0, "fetch")],
        ));
        let f = finding(&report, FindingCode::MissingRequiredParam);
        assert_eq!(f.severity, Severity::Error);
        assert!(f.message.contains("url"), "{}", f.message);
        assert!(!report.is_valid);
    }

    #[test]
    fn http_request_url_must_be_string() {
        let mut fetch = Node::new("fetch", NodeKind::HttpRequest);
        fetch.params = serde_json::json!({ "url": ["not", "a", "string"] });
        let report = validate(&wf(
            vec![Node::new("trigger", NodeKind::WebhookTrigger), fetch],
            vec![Connection::new("trigger", 0, "fetch")],
        ));
        let f = finding(&report, FindingCode::InvalidParamType);
        assert_eq!(f.severity, Severity::Error);
        assert!(f.message.contains("url"));
    }

    #[test]
    fn branch_missing_condition_is_error() {
        let report = validate(&wf(
            vec![
                Node::new("trigger", NodeKind::WebhookTrigger),
                Node::new("br", NodeKind::Branch),
            ],
            vec![Connection::new("trigger", 0, "br")],
        ));
        assert!(codes(&report).contains(&FindingCode::MissingRequiredParam));
        assert!(!report.is_valid);
    }

    #[test]
    fn switch_must_have_key_and_cases() {
        let mut sw = Node::new("sw", NodeKind::Switch);
        sw.params = serde_json::json!({}); // missing both
        let report = validate(&wf(
            vec![Node::new("trigger", NodeKind::WebhookTrigger), sw],
            vec![Connection::new("trigger", 0, "sw")],
        ));
        let missing: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.code == FindingCode::MissingRequiredParam)
            .collect();
        assert!(
            missing.len() >= 2,
            "should flag both key and cases: {missing:?}"
        );
        assert!(!report.is_valid);
    }

    #[test]
    fn loop_missing_over_is_error() {
        let report = validate(&wf(
            vec![
                Node::new("trigger", NodeKind::WebhookTrigger),
                Node::new("lp", NodeKind::Loop),
            ],
            vec![Connection::new("trigger", 0, "lp")],
        ));
        assert!(codes(&report).contains(&FindingCode::MissingRequiredParam));
    }

    #[test]
    fn wait_requires_duration_ms_integer() {
        let mut w = Node::new("w", NodeKind::Wait);
        w.params = serde_json::json!({ "duration_ms": "soon" });
        let report = validate(&wf(
            vec![Node::new("trigger", NodeKind::WebhookTrigger), w],
            vec![Connection::new("trigger", 0, "w")],
        ));
        assert!(codes(&report).contains(&FindingCode::InvalidParamType));
    }

    #[test]
    fn sub_workflow_needs_id_or_inline() {
        let report = validate(&wf(
            vec![
                Node::new("trigger", NodeKind::WebhookTrigger),
                Node::new("sub", NodeKind::SubWorkflow),
            ],
            vec![Connection::new("trigger", 0, "sub")],
        ));
        assert!(codes(&report).contains(&FindingCode::MissingRequiredParam));
    }

    #[test]
    fn trigger_with_incoming_connection_is_error() {
        // Two triggers OR a self-loop into the trigger — both would be
        // rejected. We test the second case explicitly so the role check is
        // the surfacing finding.
        let mut shape = Node::new("shape", NodeKind::Transform);
        shape.params = serde_json::json!({ "set": { "x": 1 } });
        let report = validate(&wf(
            vec![Node::new("trigger", NodeKind::WebhookTrigger), shape],
            vec![
                Connection::new("trigger", 0, "shape"),
                Connection::new("shape", 0, "trigger"),
            ],
        ));
        assert!(
            codes(&report).contains(&FindingCode::TriggerHasIncomingConnection),
            "codes: {:?}",
            codes(&report)
        );
        assert!(!report.is_valid);
    }

    #[test]
    fn report_with_node_location_serializes() {
        // Regression: Location::Node is a newtype variant wrapping a String.
        // Internally-tagged serde would error at runtime serializing it; the
        // adjacently-tagged representation must serialize cleanly, because
        // reports are handed back to the agent as JSON.
        let report = validate(&wf(
            vec![
                Node::new("dup", NodeKind::WebhookTrigger),
                Node::new("dup", NodeKind::Transform),
            ],
            vec![],
        ));
        assert!(
            report
                .findings
                .iter()
                .any(|f| matches!(&f.location, Location::Node(_))),
            "expected a node-located finding to exercise serialization"
        );
        let json = serde_json::to_string(&report).expect("serialize report with node location");
        assert!(json.contains("\"kind\":\"node\""), "json: {json}");
        assert!(json.contains("\"value\":\"dup\""), "json: {json}");
    }
}
