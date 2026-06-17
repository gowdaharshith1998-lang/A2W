//! Structural optimization: analyze a workflow (optionally with a run profile)
//! and emit [`Suggestion`]s expressed as IR diff ops ([`IrOp`]) that a later
//! `wf_update_partial` step can apply, plus an [`apply`] helper that applies them
//! to produce a new workflow.

use std::collections::HashMap;

use a2w_ir::{Connection, Workflow};
use serde::{Deserialize, Serialize};

use crate::profile::{serial_independent_edges, RunProfile};

/// A single suggested improvement to a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    /// The category of suggestion.
    pub kind: SuggestionKind,
    /// A human-readable explanation naming the nodes involved.
    pub description: String,
    /// Estimated wall-clock saving in milliseconds, when a profile is available.
    pub estimated_gain_ms: Option<u64>,
    /// The IR diff ops that realize the suggestion (may be empty for purely
    /// informational suggestions).
    pub ops: Vec<IrOp>,
}

/// The category of an optimizer [`Suggestion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionKind {
    /// Rewire a serially-dependent-but-data-independent node to run in parallel.
    Parallelize,
    /// A node produced nothing and feeds nothing; it can likely be removed.
    RemoveDeadNode,
}

/// A primitive diff op over a workflow's connections. These are the ops a later
/// `wf_update_partial` applies; [`apply`] applies them here for the test loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IrOp {
    /// Add a connection `from_node[from_port] -> to_node`.
    AddConnection {
        /// Source node id.
        from_node: String,
        /// Source output port index.
        from_port: usize,
        /// Target node id.
        to_node: String,
    },
    /// Remove the connection `from_node[from_port] -> to_node` if present.
    RemoveConnection {
        /// Source node id.
        from_node: String,
        /// Source output port index.
        from_port: usize,
        /// Target node id.
        to_node: String,
    },
}

/// Analyze a workflow and return suggestions.
///
/// Without a `profile`, only structural suggestions that need no timing data are
/// emitted (currently [`SuggestionKind::Parallelize`]). With a `profile`,
/// `estimated_gain_ms` is filled in for parallelize suggestions and
/// [`SuggestionKind::RemoveDeadNode`] suggestions are added for dead nodes.
#[must_use]
pub fn analyze(wf: &Workflow, profile: Option<&RunProfile>) -> Vec<Suggestion> {
    let mut suggestions = Vec::new();

    // Per-node latency from the profile, for estimated_gain_ms.
    let latency_of: HashMap<&str, u64> = profile
        .map(|p| {
            p.per_step
                .iter()
                .map(|s| (s.node_id.as_str(), s.latency_ms))
                .collect()
        })
        .unwrap_or_default();

    // Map each node id to its incoming edges, for the rewire.
    let incoming = incoming_edges(wf);

    // --- Parallelize ------------------------------------------------------
    for serial in serial_independent_edges(wf) {
        let a = serial.predecessor.as_str();
        let b = serial.dependent.as_str();

        // Rewire: drop A->B, and connect each of A's producers straight to B at
        // the same port, so B runs as soon as A's inputs are ready (in parallel
        // with A) instead of waiting for A to finish.
        let mut ops = vec![IrOp::RemoveConnection {
            from_node: a.to_string(),
            from_port: edge_port(wf, a, b),
            to_node: b.to_string(),
        }];
        for edge in incoming.get(a).map(Vec::as_slice).unwrap_or(&[]) {
            ops.push(IrOp::AddConnection {
                from_node: edge.from_node.clone(),
                from_port: edge.from_port,
                to_node: b.to_string(),
            });
        }

        let estimated_gain_ms = latency_of.get(a).copied();
        let gain_note = match estimated_gain_ms {
            Some(ms) => format!(" (estimated saving ~{ms}ms — B no longer waits on A)"),
            None => String::new(),
        };

        suggestions.push(Suggestion {
            kind: SuggestionKind::Parallelize,
            description: format!(
                "Run '{b}' in parallel with '{a}': '{b}' has a single producer ('{a}') and \
                 does not consume its output, so rewire '{b}' to the producers of '{a}' and \
                 drop the '{a}'->'{b}' edge{gain_note}"
            ),
            estimated_gain_ms,
            ops,
        });
    }

    // --- RemoveDeadNode (profile-gated, informational) --------------------
    if let Some(profile) = profile {
        // Nodes with at least one outgoing connection are not dead-ended.
        let mut has_outgoing: HashMap<&str, bool> = HashMap::new();
        for conn in &wf.connections {
            has_outgoing.insert(conn.from_node.as_str(), true);
        }
        let is_trigger: HashMap<&str, bool> = wf
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n.kind.is_trigger()))
            .collect();

        for step in &profile.per_step {
            let id = step.node_id.as_str();
            let trigger = is_trigger.get(id).copied().unwrap_or(false);
            let outgoing = has_outgoing.get(id).copied().unwrap_or(false);
            if !trigger && step.output_items == 0 && !outgoing {
                suggestions.push(Suggestion {
                    kind: SuggestionKind::RemoveDeadNode,
                    description: format!(
                        "node '{id}' produced zero output and has no downstream consumers; \
                         consider removing it (informational — no ops applied automatically in M3)"
                    ),
                    // Node deletion is deferred; keep this informational.
                    estimated_gain_ms: Some(step.latency_ms),
                    ops: Vec::new(),
                });
            }
        }
    }

    suggestions
}

/// Apply `ops` to `wf`, returning a NEW workflow. Removing a non-existent
/// connection is a no-op; adding a duplicate connection is suppressed so the
/// operation is idempotent-ish.
#[must_use]
pub fn apply(wf: &Workflow, ops: &[IrOp]) -> Workflow {
    let mut out = wf.clone();
    for op in ops {
        match op {
            IrOp::RemoveConnection {
                from_node,
                from_port,
                to_node,
            } => {
                out.connections.retain(|c| {
                    !(c.from_node == *from_node
                        && c.from_port == *from_port
                        && c.to_node == *to_node)
                });
            }
            IrOp::AddConnection {
                from_node,
                from_port,
                to_node,
            } => {
                let exists = out.connections.iter().any(|c| {
                    c.from_node == *from_node
                        && c.from_port == *from_port
                        && c.to_node == *to_node
                });
                if !exists {
                    out.connections
                        .push(Connection::new(from_node.clone(), *from_port, to_node.clone()));
                }
            }
        }
    }
    out
}

/// Whether a node's params reference its input data via the `{{json` token
/// convention (`{{json}}` or `{{json.FIELD}}`). A node whose params contain no
/// such token does not consume its predecessor's output, so it can be
/// parallelized with that predecessor.
#[must_use]
pub fn params_consume_input(params: &serde_json::Value) -> bool {
    // Serialize the whole params blob and look for the token marker. This
    // matches the interim templating convention used by a2w-nodes.
    serde_json::to_string(params)
        .map(|s| s.contains("{{json"))
        .unwrap_or(false)
}

/// An incoming edge (producer) into a node.
#[derive(Clone)]
struct InEdge {
    from_node: String,
    from_port: usize,
}

/// Build, per node id, the list of its incoming edges.
fn incoming_edges(wf: &Workflow) -> HashMap<String, Vec<InEdge>> {
    let mut map: HashMap<String, Vec<InEdge>> = HashMap::new();
    for conn in &wf.connections {
        map.entry(conn.to_node.clone()).or_default().push(InEdge {
            from_node: conn.from_node.clone(),
            from_port: conn.from_port,
        });
    }
    map
}

/// The `from_port` of the (assumed unique) edge A->B. Defaults to 0 if not
/// found, which cannot happen for an edge supplied by `serial_independent_edges`.
fn edge_port(wf: &Workflow, from: &str, to: &str) -> usize {
    wf.connections
        .iter()
        .find(|c| c.from_node == from && c.to_node == to)
        .map(|c| c.from_port)
        .unwrap_or(0)
}
