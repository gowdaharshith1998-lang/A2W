//! # a2w-optimizer
//!
//! Run **profiling** and structural **optimization** for A2W workflows. This
//! backs the `wf_profile` / `wf_optimize` agent tools.
//!
//! ## Profiler ([`profile`])
//! Turns a [`RunResult`](a2w_engine::RunResult)'s step events into a
//! [`RunProfile`]: per-step latency/throughput, the latency-**critical path**
//! through the DAG (the wall-clock lower bound, since independent branches run
//! concurrently), and a list of flagged [`Inefficiency`]s.
//!
//! ## Optimizer ([`analyze`] / [`apply`])
//! Inspects a workflow (optionally with a profile) and emits [`Suggestion`]s as
//! IR diff ops ([`IrOp`]). The flagship is **Parallelize**: an edge `A -> B`
//! where `B` depends on `A` only for ordering, not data, is rewired so `B` runs
//! in parallel with `A`. [`apply`] applies a suggestion's ops to produce a new
//! workflow, so an agent can apply-then-retest in a loop.
//!
//! ### Parallelize rule
//! For an edge `A -> B` where (1) `B` has exactly one incoming connection (from
//! `A`), (2) `A` is not a trigger, and (3) `B`'s params contain no `{{json`
//! token (so `B` does not consume `A`'s output), emit:
//! `ops = [RemoveConnection(A->B)]` followed by, for each incoming edge `X->A`,
//! `AddConnection(X[port] -> B)`. `B` then becomes runnable as soon as `A`'s own
//! inputs are ready, executing concurrently with `A`.

#![forbid(unsafe_code)]

mod graph;
mod optimize;
mod profile;

pub use optimize::{analyze, apply, IrOp, Suggestion, SuggestionKind};
pub use profile::{profile, Inefficiency, InefficiencyKind, RunProfile, StepProfile};

#[cfg(test)]
mod tests {
    use super::*;
    use a2w_engine::{Engine, ExecutionMode, MemoryEventLog};
    use a2w_ir::{Connection, Node, NodeKind, Workflow, SCHEMA_VERSION};
    use serde_json::json;

    /// Build a workflow with boilerplate filled in.
    fn wf(nodes: Vec<Node>, connections: Vec<Connection>) -> Workflow {
        Workflow {
            schema_version: SCHEMA_VERSION,
            id: "wf_opt".to_string(),
            name: "optimizer test".to_string(),
            nodes,
            connections,
        }
    }

    /// An HttpRequest node with a literal url (no `{{json}}` → independent).
    fn http_literal(id: &str, url: &str) -> Node {
        let mut n = Node::new(id, NodeKind::HttpRequest);
        n.params = json!({ "url": url });
        n
    }

    /// `trigger -> A(http literal) -> B(http literal)`. B is independent of A.
    fn linear_independent() -> Workflow {
        wf(
            vec![
                Node::new("trigger", NodeKind::WebhookTrigger),
                http_literal("a", "https://example.com/a"),
                http_literal("b", "https://example.com/b"),
            ],
            vec![
                Connection::new("trigger", 0, "a"),
                Connection::new("a", 0, "b"),
            ],
        )
    }

    #[test]
    fn parallelize_independent_chain() {
        let wf = linear_independent();
        let suggestions = analyze(&wf, None);

        let par: Vec<&Suggestion> = suggestions
            .iter()
            .filter(|s| s.kind == SuggestionKind::Parallelize)
            .collect();
        assert_eq!(par.len(), 1, "expected exactly one parallelize suggestion");
        let s = par[0];
        assert!(s.description.contains("'a'") && s.description.contains("'b'"));
        // No profile -> no estimated gain.
        assert!(s.estimated_gain_ms.is_none());

        // ops = remove a->b, add trigger[0]->b.
        assert!(s.ops.contains(&IrOp::RemoveConnection {
            from_node: "a".to_string(),
            from_port: 0,
            to_node: "b".to_string(),
        }));
        assert!(s.ops.contains(&IrOp::AddConnection {
            from_node: "trigger".to_string(),
            from_port: 0,
            to_node: "b".to_string(),
        }));

        // Apply and assert the new connection set: trigger->a and trigger->b,
        // and a->b is gone (b no longer has a as a producer; shares trigger).
        let applied = apply(&wf, &s.ops);
        let edges: Vec<(&str, usize, &str)> = applied
            .connections
            .iter()
            .map(|c| (c.from_node.as_str(), c.from_port, c.to_node.as_str()))
            .collect();
        assert!(edges.contains(&("trigger", 0, "a")));
        assert!(edges.contains(&("trigger", 0, "b")));
        assert!(!edges.contains(&("a", 0, "b")), "a->b must be removed");

        // b's producers: only the trigger now.
        let b_producers: Vec<&str> = applied
            .connections
            .iter()
            .filter(|c| c.to_node == "b")
            .map(|c| c.from_node.as_str())
            .collect();
        assert_eq!(b_producers, vec!["trigger"]);
        assert!(!b_producers.contains(&"a"));
    }

    #[test]
    fn no_parallelize_when_dependent() {
        // B's url references {{json.id}} → B consumes A's output → not independent.
        let mut b = Node::new("b", NodeKind::HttpRequest);
        b.params = json!({ "url": "https://example.com/{{json.id}}" });
        let wf = wf(
            vec![
                Node::new("trigger", NodeKind::WebhookTrigger),
                http_literal("a", "https://example.com/a"),
                b,
            ],
            vec![
                Connection::new("trigger", 0, "a"),
                Connection::new("a", 0, "b"),
            ],
        );
        let suggestions = analyze(&wf, None);
        assert!(
            !suggestions
                .iter()
                .any(|s| s.kind == SuggestionKind::Parallelize),
            "B depends on A's data; no parallelize expected: {suggestions:?}"
        );
    }

    #[tokio::test]
    async fn profiler_computes_critical_path() {
        // Run the independent chain in DryRun so HttpRequest does no network I/O
        // but still emits Finished events with item counts.
        let wf = linear_independent();
        let engine = Engine::new(a2w_nodes::default_registry());
        let log = MemoryEventLog::new();
        let result = engine
            .run(&wf, vec![json!({ "id": 1 })], ExecutionMode::DryRun, &log)
            .await
            .expect("run should succeed");

        let prof = profile(&wf, &result);

        // per_step covers all three nodes.
        let ids: Vec<&str> = prof.per_step.iter().map(|s| s.node_id.as_str()).collect();
        assert!(ids.contains(&"trigger"), "per_step: {ids:?}");
        assert!(ids.contains(&"a"), "per_step: {ids:?}");
        assert!(ids.contains(&"b"), "per_step: {ids:?}");

        // The critical path runs trigger -> a -> b (the only root->sink path).
        assert_eq!(
            prof.critical_path,
            vec!["trigger".to_string(), "a".to_string(), "b".to_string()],
            "critical path should be the single chain"
        );

        // total_latency_ms == sum of the critical-path node latencies.
        let sum: u64 = prof
            .critical_path
            .iter()
            .map(|id| {
                prof.per_step
                    .iter()
                    .find(|s| &s.node_id == id)
                    .map(|s| s.latency_ms)
                    .unwrap_or(0)
            })
            .sum();
        assert_eq!(prof.total_latency_ms, sum);
    }

    #[tokio::test]
    async fn profiler_critical_path_picks_slower_branch() {
        // Diamond: trigger -> {a, b} -> merge. Hand-build a RunResult with known
        // latencies so the critical path is deterministic. We use the engine to
        // get a real RunResult shape, then assert critical-path selection via a
        // synthetic profile path: here we just check the longest branch wins.
        //
        // trigger(0) -> a(5) -> sink ; trigger(0) -> b(20) -> sink
        // critical path must go through b (the slower branch).
        let wf = wf(
            vec![
                Node::new("trigger", NodeKind::WebhookTrigger),
                http_literal("a", "https://example.com/a"),
                http_literal("b", "https://example.com/b"),
                {
                    let mut s = Node::new("sink", NodeKind::Transform);
                    // No {{json}} → independent, but it has two producers so it
                    // is not a parallelize candidate.
                    s.params = json!({});
                    s
                },
            ],
            vec![
                Connection::new("trigger", 0, "a"),
                Connection::new("trigger", 0, "b"),
                Connection::new("a", 0, "sink"),
                Connection::new("b", 0, "sink"),
            ],
        );

        // Build a synthetic RunResult with controlled latencies via the engine's
        // public StepEvent through a MemoryEventLog is not directly settable;
        // instead construct events by hand.
        use a2w_engine::{RunResult, RunStatus, StepEvent, StepKind};
        let ev = |node: &str, lat: u64, out: usize| StepEvent {
            run_id: "r".to_string(),
            node_id: node.to_string(),
            kind: StepKind::Finished,
            latency_ms: lat,
            input_items: 1,
            output_items: out,
            external_calls: 0,
            tokens: 0,
            error: None,
        };
        let result = RunResult {
            run_id: "r".to_string(),
            status: RunStatus::Completed,
            node_outputs: std::collections::HashMap::new(),
            events: vec![
                ev("trigger", 0, 1),
                ev("a", 5, 1),
                ev("b", 20, 1),
                ev("sink", 1, 1),
            ],
        };

        let prof = profile(&wf, &result);
        assert_eq!(
            prof.critical_path,
            vec!["trigger".to_string(), "b".to_string(), "sink".to_string()],
            "critical path must traverse the slower branch b"
        );
        // trigger(0) + b(20) + sink(1) = 21.
        assert_eq!(prof.total_latency_ms, 21);
    }

    #[test]
    fn zero_output_is_flagged() {
        use a2w_engine::{RunResult, RunStatus, StepEvent, StepKind};
        let wf = wf(
            vec![
                Node::new("trigger", NodeKind::WebhookTrigger),
                Node::new("dead", NodeKind::Transform),
            ],
            vec![Connection::new("trigger", 0, "dead")],
        );
        let result = RunResult {
            run_id: "r".to_string(),
            status: RunStatus::Completed,
            node_outputs: std::collections::HashMap::new(),
            events: vec![
                StepEvent {
                    run_id: "r".to_string(),
                    node_id: "trigger".to_string(),
                    kind: StepKind::Finished,
                    latency_ms: 0,
                    input_items: 1,
                    output_items: 1,
                    external_calls: 0,
                    tokens: 0,
                    error: None,
                },
                StepEvent {
                    run_id: "r".to_string(),
                    node_id: "dead".to_string(),
                    kind: StepKind::Finished,
                    latency_ms: 2,
                    input_items: 1,
                    output_items: 0,
                    external_calls: 0,
                    tokens: 0,
                    error: None,
                },
            ],
        };
        let prof = profile(&wf, &result);
        assert!(prof
            .flagged
            .iter()
            .any(|f| f.kind == InefficiencyKind::ZeroOutput
                && f.node_id.as_deref() == Some("dead")));

        // 'dead' has zero output and no outgoing edges → RemoveDeadNode.
        let suggestions = analyze(&wf, Some(&prof));
        assert!(suggestions
            .iter()
            .any(|s| s.kind == SuggestionKind::RemoveDeadNode));
    }

    #[test]
    fn apply_is_idempotent_and_remove_missing_is_noop() {
        let wf = linear_independent();
        // Remove a non-existent edge → no-op.
        let after = apply(
            &wf,
            &[IrOp::RemoveConnection {
                from_node: "ghost".to_string(),
                from_port: 0,
                to_node: "nope".to_string(),
            }],
        );
        assert_eq!(after.connections, wf.connections);

        // Adding the same edge twice yields one edge.
        let add = IrOp::AddConnection {
            from_node: "trigger".to_string(),
            from_port: 0,
            to_node: "b".to_string(),
        };
        let twice = apply(&wf, &[add.clone(), add]);
        let count = twice
            .connections
            .iter()
            .filter(|c| c.from_node == "trigger" && c.to_node == "b")
            .count();
        assert_eq!(count, 1, "duplicate add should be suppressed");
    }
}
