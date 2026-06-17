//! Longest-latency path over a node-weighted DAG.
//!
//! The critical path of a run is the root→sink path that maximizes the sum of
//! per-node latencies: the wall-clock lower bound, since independent branches
//! run concurrently. We compute it with a topological order plus a single
//! dynamic-programming pass — no need for an external longest-path routine.

use std::collections::HashMap;

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use petgraph::Direction;

/// Return the node ids on a maximum-weight (longest-latency) path through the
/// DAG, in topological order, where each node's weight is `weight(id)`.
///
/// The graph is assumed acyclic (the engine only runs validated, acyclic
/// workflows). If a cycle is present we degrade gracefully to an empty path
/// rather than looping. Ties are broken deterministically by node index, which
/// follows declaration order at construction.
pub(crate) fn longest_latency_path<F>(graph: &DiGraph<&str, ()>, weight: &F) -> Vec<String>
where
    F: Fn(&str) -> u64,
{
    let order = match petgraph::algo::toposort(graph, None) {
        Ok(order) => order,
        // Cyclic (shouldn't happen for a validated workflow): no meaningful
        // longest path, so return empty rather than risk a bad traversal.
        Err(_) => return Vec::new(),
    };

    // best[n] = the best path ENDING at n (inclusive of n), scored
    // lexicographically as (total latency, node count). Scoring by node count
    // as a tiebreaker means that when latencies are equal (e.g. an all-zero
    // dry run) the path traversing MORE nodes wins, yielding a meaningful
    // root→sink critical path rather than collapsing to a single node.
    // pred[n] = the predecessor on that best path (None if n is a root).
    let mut best: HashMap<NodeIndex, (u64, usize)> = HashMap::with_capacity(order.len());
    let mut pred: HashMap<NodeIndex, Option<NodeIndex>> = HashMap::with_capacity(order.len());

    for &n in &order {
        let w = weight(graph[n]);
        // Choose the incoming neighbour that maximizes the score into n.
        let mut chosen: Option<NodeIndex> = None;
        let mut chosen_best: (u64, usize) = (0, 0);
        for edge in graph.edges_directed(n, Direction::Incoming) {
            let src = edge.source();
            let cand = best.get(&src).copied().unwrap_or((0, 0));
            // `>` (not `>=`) keeps the earliest-declared predecessor on ties.
            if chosen.is_none() || cand > chosen_best {
                chosen = Some(src);
                chosen_best = cand;
            }
        }
        best.insert(n, (chosen_best.0.saturating_add(w), chosen_best.1 + 1));
        pred.insert(n, chosen);
    }

    // The path ends at the node with the maximum score. On ties, prefer the
    // earliest in topological order for determinism.
    let mut end: Option<NodeIndex> = None;
    let mut end_best: (u64, usize) = (0, 0);
    for &n in &order {
        let b = best.get(&n).copied().unwrap_or((0, 0));
        if end.is_none() || b > end_best {
            end = Some(n);
            end_best = b;
        }
    }

    // Walk predecessors back from the end to reconstruct the path.
    let mut rev: Vec<String> = Vec::new();
    let mut cursor = end;
    while let Some(n) = cursor {
        rev.push(graph[n].to_string());
        cursor = pred.get(&n).copied().flatten();
    }
    rev.reverse();
    rev
}
