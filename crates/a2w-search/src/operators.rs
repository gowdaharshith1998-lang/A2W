//! Correctness-preserving mutation operators.
//!
//! Following the MermaidFlow lesson, operators transform the IR while keeping
//! it **valid by construction** — every candidate they emit passes M1 (the
//! search re-validates as a belt-and-suspenders guard, and discards any that
//! somehow don't). The search space is therefore safe to explore: there are no
//! "wasted" candidates that would only fail at runtime.

use a2w_ir::{Connection, Node, NodeKind, Workflow};
use serde_json::{Map, Value};

/// A validity-preserving mutation operator.
pub trait Mutation {
    /// A stable name for diagnostics.
    fn name(&self) -> &str;
    /// Produce zero or more candidate workflows derived from `wf`. Each should
    /// be M1-valid; the search re-validates regardless.
    fn apply(&self, wf: &Workflow) -> Vec<Workflow>;
}

/// Set a field on each `Transform` node, trying every `(key, value)` in a
/// vocabulary. This is the primary *semantic* lever: it can add the field a
/// spec assertion or golden fixture requires.
pub struct SetTransformField {
    /// Candidate `(key, value)` entries to merge into a Transform's `set`.
    pub vocabulary: Vec<(String, Value)>,
    /// Node ids the operator must not modify (e.g. the observed node, if you
    /// want to keep its output shape fixed). Usually empty.
    pub frozen: Vec<String>,
}

impl Mutation for SetTransformField {
    fn name(&self) -> &str {
        "set_transform_field"
    }

    fn apply(&self, wf: &Workflow) -> Vec<Workflow> {
        let mut out = Vec::new();
        for (idx, node) in wf.nodes.iter().enumerate() {
            if node.kind != NodeKind::Transform || self.frozen.contains(&node.id) {
                continue;
            }
            for (key, value) in &self.vocabulary {
                let mut candidate = wf.clone();
                let target = &mut candidate.nodes[idx];
                let set = ensure_set_object(&mut target.params);
                // Skip if it already has exactly this entry (no-op mutation).
                if set.get(key) == Some(value) {
                    continue;
                }
                set.insert(key.clone(), value.clone());
                out.push(candidate);
            }
        }
        out
    }
}

/// Insert a passthrough `Transform` node on each connection (structural
/// diversity). `a --port--> b` becomes `a --port--> ins --0--> b`. Functionally
/// a no-op, but it gives later semantic operators a fresh node to specialize.
pub struct InsertPassthrough;

impl Mutation for InsertPassthrough {
    fn name(&self) -> &str {
        "insert_passthrough"
    }

    fn apply(&self, wf: &Workflow) -> Vec<Workflow> {
        let mut out = Vec::new();
        for (ci, conn) in wf.connections.iter().enumerate() {
            let new_id = unique_id(wf, &format!("ins_{}_{}", conn.from_node, conn.to_node));
            let mut candidate = wf.clone();
            let mut passthrough = Node::new(&new_id, NodeKind::Transform);
            passthrough.params = Value::Object(Map::new());
            candidate.nodes.push(passthrough);
            // Rewire: original edge -> two edges through the new node.
            candidate.connections[ci] =
                Connection::new(conn.from_node.clone(), conn.from_port, new_id.clone());
            candidate
                .connections
                .push(Connection::new(new_id, 0, conn.to_node.clone()));
            out.push(candidate);
        }
        out
    }
}

/// Remove a passthrough `Transform` node (empty `set`), reconnecting its
/// predecessors directly to its successors. The inverse of [`InsertPassthrough`];
/// keeps the search from bloating and can simplify a seed.
pub struct RemovePassthrough {
    /// Node ids that must never be removed (e.g. the observed node).
    pub frozen: Vec<String>,
}

impl Mutation for RemovePassthrough {
    fn name(&self) -> &str {
        "remove_passthrough"
    }

    fn apply(&self, wf: &Workflow) -> Vec<Workflow> {
        let mut out = Vec::new();
        for node in &wf.nodes {
            if node.kind != NodeKind::Transform || self.frozen.contains(&node.id) {
                continue;
            }
            if !is_empty_passthrough(node) {
                continue;
            }
            let id = node.id.as_str();
            let preds: Vec<&Connection> =
                wf.connections.iter().filter(|c| c.to_node == id).collect();
            let succs: Vec<&Connection> = wf
                .connections
                .iter()
                .filter(|c| c.from_node == id)
                .collect();
            // Only remove a "simple" passthrough (has both predecessors and
            // successors) so the graph stays connected.
            if preds.is_empty() || succs.is_empty() {
                continue;
            }

            let mut candidate = wf.clone();
            candidate.nodes.retain(|n| n.id != id);
            candidate
                .connections
                .retain(|c| c.from_node != id && c.to_node != id);
            // Reconnect: each predecessor's port -> each successor's target.
            for p in &preds {
                for s in &succs {
                    candidate.connections.push(Connection::new(
                        p.from_node.clone(),
                        p.from_port,
                        s.to_node.clone(),
                    ));
                }
            }
            out.push(candidate);
        }
        out
    }
}

/// Ensure `params` is an object with a `set` object, returning a mutable handle
/// to the `set` map.
fn ensure_set_object(params: &mut Value) -> &mut Map<String, Value> {
    if !params.is_object() {
        *params = Value::Object(Map::new());
    }
    let obj = params.as_object_mut().expect("params is now an object");
    if !obj.get("set").map(Value::is_object).unwrap_or(false) {
        obj.insert("set".to_string(), Value::Object(Map::new()));
    }
    obj.get_mut("set")
        .and_then(Value::as_object_mut)
        .expect("set is now an object")
}

/// True if a Transform has an empty (or absent) `set` — a pure passthrough.
fn is_empty_passthrough(node: &Node) -> bool {
    match node.params.get("set") {
        None => node.params.as_object().map(Map::is_empty).unwrap_or(true),
        Some(Value::Object(m)) => m.is_empty(),
        Some(_) => false,
    }
}

/// Generate an id derived from `base` that does not collide with any existing
/// node id in `wf`.
fn unique_id(wf: &Workflow, base: &str) -> String {
    if !wf.nodes.iter().any(|n| n.id == base) {
        return base.to_string();
    }
    let mut i = 1;
    loop {
        let candidate = format!("{base}_{i}");
        if !wf.nodes.iter().any(|n| n.id == candidate) {
            return candidate;
        }
        i += 1;
    }
}
