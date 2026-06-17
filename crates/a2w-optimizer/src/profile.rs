//! Run profiling: turn a [`RunResult`]'s step events into a [`RunProfile`] with
//! per-step metrics, the latency-critical path through the DAG, and flagged
//! inefficiencies.

use std::collections::HashMap;

use a2w_engine::{RunResult, StepKind};
use a2w_ir::Workflow;
use petgraph::graph::{DiGraph, NodeIndex};
use serde::Serialize;

use crate::graph::longest_latency_path;
use crate::optimize::params_consume_input;

/// A profile of one workflow run.
#[derive(Debug, Clone, Serialize)]
pub struct RunProfile {
    /// The wall-clock lower bound implied by the dependency graph: the sum of
    /// node latencies along the latency-critical path (NOT the sum of all
    /// nodes, since independent branches run concurrently).
    pub total_latency_ms: u64,
    /// One entry per node that produced a `Finished` event, derived from that
    /// node's last `Finished` event.
    pub per_step: Vec<StepProfile>,
    /// The node ids on the longest-latency root→sink path, in order.
    pub critical_path: Vec<String>,
    /// Inefficiencies detected in this run.
    pub flagged: Vec<Inefficiency>,
}

/// Per-node timing/throughput, from the node's last `Finished` event.
#[derive(Debug, Clone, Serialize)]
pub struct StepProfile {
    /// The node id.
    pub node_id: String,
    /// Measured step latency in milliseconds.
    pub latency_ms: u64,
    /// Items handed to the node.
    pub input_items: usize,
    /// Items the node produced.
    pub output_items: usize,
}

/// A detected inefficiency, optionally located on a node.
#[derive(Debug, Clone, Serialize)]
pub struct Inefficiency {
    /// The node this concerns, if node-specific.
    pub node_id: Option<String>,
    /// The category of inefficiency.
    pub kind: InefficiencyKind,
    /// A human-readable explanation.
    pub description: String,
}

/// Categories of inefficiency the profiler flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InefficiencyKind {
    /// A node finished but produced zero output items.
    ZeroOutput,
    /// A node runs serially after a predecessor it does not depend on, so it
    /// could run in parallel instead (see the optimizer's parallelize rule).
    SerialIndependentStep,
}

/// Profile a run: derive per-step metrics, the critical path, and flagged
/// inefficiencies.
///
/// `per_step` uses the **last** `Finished` event per node (a node executes once
/// per run today, but last-wins is robust to future re-execution). Latency for
/// the critical path comes from those `Finished` events; a node without a
/// `Finished` event contributes zero latency.
#[must_use]
pub fn profile(wf: &Workflow, result: &RunResult) -> RunProfile {
    // --- Per-step from the last Finished event per node. ------------------
    // Insertion order of events is preserved, so overwriting on each Finished
    // leaves the last one. We also remember first-seen order for stable output.
    let mut last_finished: HashMap<&str, StepProfile> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for ev in &result.events {
        if ev.kind == StepKind::Finished {
            let id = ev.node_id.as_str();
            if !last_finished.contains_key(id) {
                order.push(id);
            }
            last_finished.insert(
                id,
                StepProfile {
                    node_id: ev.node_id.clone(),
                    latency_ms: ev.latency_ms,
                    input_items: ev.input_items,
                    output_items: ev.output_items,
                },
            );
        }
    }

    let per_step: Vec<StepProfile> = order
        .iter()
        .filter_map(|id| last_finished.get(id).cloned())
        .collect();

    // --- Latency lookup for the critical-path computation. ----------------
    let latency_of: HashMap<&str, u64> = last_finished
        .iter()
        .map(|(&id, sp)| (id, sp.latency_ms))
        .collect();

    let (critical_path, total_latency_ms) = critical_path(wf, &latency_of);

    // --- Flagged inefficiencies. ------------------------------------------
    let mut flagged: Vec<Inefficiency> = Vec::new();

    // ZeroOutput: any node whose Finished output_items == 0. Emit in the same
    // stable per-step order.
    for sp in &per_step {
        if sp.output_items == 0 {
            flagged.push(Inefficiency {
                node_id: Some(sp.node_id.clone()),
                kind: InefficiencyKind::ZeroOutput,
                description: format!(
                    "node '{}' finished but produced zero output items (received {})",
                    sp.node_id, sp.input_items
                ),
            });
        }
    }

    // SerialIndependentStep: edge A->B where B has exactly one producer (A), A
    // is not a trigger, and B does not consume A's data ({{json absent). This
    // mirrors the optimizer's parallelize rule so the profiler and optimizer
    // agree on what is parallelizable.
    for serial in serial_independent_edges(wf) {
        flagged.push(Inefficiency {
            node_id: Some(serial.dependent.clone()),
            kind: InefficiencyKind::SerialIndependentStep,
            description: format!(
                "node '{}' runs serially after '{}' but does not consume its output; \
                 it could run in parallel",
                serial.dependent, serial.predecessor
            ),
        });
    }

    RunProfile {
        total_latency_ms,
        per_step,
        critical_path,
        flagged,
    }
}

/// Compute the latency-critical path and its total latency over the workflow
/// DAG. Node latency defaults to 0 when the node has no `Finished` event.
fn critical_path(wf: &Workflow, latency_of: &HashMap<&str, u64>) -> (Vec<String>, u64) {
    // Build a DiGraph over node ids (deterministic, declaration order).
    let mut graph: DiGraph<&str, ()> = DiGraph::new();
    let mut idx_of: HashMap<&str, NodeIndex> = HashMap::new();
    for node in &wf.nodes {
        idx_of
            .entry(node.id.as_str())
            .or_insert_with(|| graph.add_node(node.id.as_str()));
    }
    for conn in &wf.connections {
        if let (Some(&fi), Some(&ti)) = (
            idx_of.get(conn.from_node.as_str()),
            idx_of.get(conn.to_node.as_str()),
        ) {
            graph.add_edge(fi, ti, ());
        }
    }

    let weight = |id: &str| latency_of.get(id).copied().unwrap_or(0);
    let path = longest_latency_path(&graph, &weight);
    let total = path.iter().map(|id| weight(id)).sum();
    (path, total)
}

/// An A->B edge where B runs serially after A but does not depend on A's data.
pub(crate) struct SerialEdge {
    pub predecessor: String,
    pub dependent: String,
}

/// Find every edge A->B that is serial-but-independent per the parallelize rule:
/// B has exactly one incoming connection (from A), A is not a trigger, and B's
/// params contain no `{{json` token (so B does not consume A's output).
pub(crate) fn serial_independent_edges(wf: &Workflow) -> Vec<SerialEdge> {
    // Count incoming connections per target node.
    let mut in_count: HashMap<&str, usize> = HashMap::new();
    for conn in &wf.connections {
        *in_count.entry(conn.to_node.as_str()).or_insert(0) += 1;
    }

    let node_by_id: HashMap<&str, &a2w_ir::Node> =
        wf.nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    let mut edges = Vec::new();
    for conn in &wf.connections {
        let b = conn.to_node.as_str();
        let a = conn.from_node.as_str();

        // (1) B has exactly one incoming connection (which is necessarily A->B).
        if in_count.get(b).copied().unwrap_or(0) != 1 {
            continue;
        }
        // (2) A is not a trigger.
        let Some(a_node) = node_by_id.get(a) else {
            continue;
        };
        if a_node.kind.is_trigger() {
            continue;
        }
        // (3) B does not consume A's output data.
        let Some(b_node) = node_by_id.get(b) else {
            continue;
        };
        if params_consume_input(&b_node.params) {
            continue;
        }

        edges.push(SerialEdge {
            predecessor: a.to_string(),
            dependent: b.to_string(),
        });
    }
    edges
}
