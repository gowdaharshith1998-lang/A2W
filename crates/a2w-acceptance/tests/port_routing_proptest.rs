//! M0 — Property tests for port-indexed routing.
//!
//! These tests target the *routing contract*: an executor stamps each item's
//! `output_port`, and the engine's `gather_input` filters incoming items by the
//! `(from_node, from_port)` of the connection. The property under test is the
//! fan-out invariant:
//!
//! > For any valid Branch/Switch/Loop/Wait workflow, the items observed at
//! > downstream nodes are exactly the items the upstream executor stamped for
//! > the matching `(from_node, from_port)` — no leakage across ports, no
//! > duplication, no silent loss.
//!
//! We exercise this through the engine's public `run` API so the routing
//! invariant is checked end-to-end (executor → re-stamping → fan-in dedup →
//! port-matched gather).

use a2w_engine::{Engine, ExecutionMode, MemoryEventLog, RunStatus};
use a2w_ir::{Connection, Node, NodeKind, Workflow, SCHEMA_VERSION};
use proptest::prelude::*;
use serde_json::json;

fn engine() -> Engine {
    Engine::new(a2w_nodes::default_registry()).with_max_concurrency(4)
}

fn wf(id: &str, nodes: Vec<Node>, connections: Vec<Connection>) -> Workflow {
    Workflow {
        schema_version: SCHEMA_VERSION,
        id: id.to_string(),
        name: id.to_string(),
        nodes,
        connections,
    }
}

fn trigger() -> Node {
    Node::new("trigger", NodeKind::WebhookTrigger)
}

fn transform_sink(id: &str) -> Node {
    let mut n = Node::new(id, NodeKind::Transform);
    n.params = json!({ "set": { "_sunk_by": id } });
    n
}

/// Property: a Branch routes every item to exactly one of its two ports, and
/// the items observed by the downstream "true" sink are the items whose
/// `/active` field is truthy.
fn branch_strategy() -> impl Strategy<Value = Vec<bool>> {
    proptest::collection::vec(any::<bool>(), 0..10)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

    /// Branch fan-out: each input lands on exactly one of `true_sink` /
    /// `false_sink`. The unions reconstruct the original input set, with no
    /// duplication.
    #[test]
    fn branch_partition_is_total_and_disjoint(actives in branch_strategy()) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let mut branch_node = Node::new("br", NodeKind::Branch);
            branch_node.params = json!({
                "condition": { "path": "/active", "op": "truthy" }
            });

            let workflow = wf(
                "wf_branch_prop",
                vec![
                    trigger(),
                    branch_node,
                    transform_sink("t_true"),
                    transform_sink("t_false"),
                ],
                vec![
                    Connection::new("trigger", 0, "br"),
                    Connection::new("br", 0, "t_true"),
                    Connection::new("br", 1, "t_false"),
                ],
            );

            let inputs: Vec<serde_json::Value> = actives
                .iter()
                .enumerate()
                .map(|(i, a)| json!({ "id": i, "active": a }))
                .collect();

            let log = MemoryEventLog::new();
            let run = engine()
                .run(&workflow, inputs, ExecutionMode::Run, &log)
                .await
                .expect("branch run");
            assert_eq!(run.status, RunStatus::Completed);

            let trues: Vec<_> = run
                .node_outputs
                .get("t_true")
                .cloned()
                .unwrap_or_default();
            let falses: Vec<_> = run
                .node_outputs
                .get("t_false")
                .cloned()
                .unwrap_or_default();

            let want_true = actives.iter().filter(|a| **a).count();
            let want_false = actives.iter().filter(|a| !**a).count();
            prop_assert_eq!(trues.len(), want_true);
            prop_assert_eq!(falses.len(), want_false);

            // Totality + disjointness: every input id appears in exactly one sink.
            let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
            for it in trues.iter().chain(falses.iter()) {
                let id = it.json["id"].as_u64().unwrap();
                prop_assert!(seen.insert(id), "id {id} appeared more than once");
            }
            prop_assert_eq!(seen.len(), actives.len());
            Ok(())
        }).unwrap();
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, .. ProptestConfig::default() })]

    /// Switch fan-out: for cases = {a:0, b:1, c:2} + default port 3, every
    /// input is routed to exactly the case-derived port, and the per-port
    /// counts match the input distribution.
    #[test]
    fn switch_routes_to_case_or_default(
        labels in proptest::collection::vec(
            prop_oneof![Just("a"), Just("b"), Just("c"), Just("z")],
            0..12,
        )
    ) {
        let labels: Vec<String> = labels.into_iter().map(str::to_string).collect();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let mut sw = Node::new("sw", NodeKind::Switch);
            sw.params = json!({
                "key": "/label",
                "cases": [
                    { "value": "a", "port": 0 },
                    { "value": "b", "port": 1 },
                    { "value": "c", "port": 2 }
                ],
                "default_port": 3
            });
            let workflow = wf(
                "wf_switch_prop",
                vec![
                    trigger(),
                    sw,
                    transform_sink("p0"),
                    transform_sink("p1"),
                    transform_sink("p2"),
                    transform_sink("p3"),
                ],
                vec![
                    Connection::new("trigger", 0, "sw"),
                    Connection::new("sw", 0, "p0"),
                    Connection::new("sw", 1, "p1"),
                    Connection::new("sw", 2, "p2"),
                    Connection::new("sw", 3, "p3"),
                ],
            );

            let inputs: Vec<serde_json::Value> = labels
                .iter()
                .enumerate()
                .map(|(i, l)| json!({ "id": i, "label": l }))
                .collect();

            let log = MemoryEventLog::new();
            let run = engine()
                .run(&workflow, inputs, ExecutionMode::Run, &log)
                .await
                .expect("switch run");
            assert_eq!(run.status, RunStatus::Completed);

            let want = |target: &str| labels.iter().filter(|l| l == &target).count();
            let want_dflt = labels.iter().filter(|l| !["a","b","c"].contains(&l.as_str())).count();

            for (sink, want_count) in [
                ("p0", want("a")),
                ("p1", want("b")),
                ("p2", want("c")),
                ("p3", want_dflt),
            ] {
                let got = run.node_outputs.get(sink).map(Vec::len).unwrap_or(0);
                prop_assert_eq!(got, want_count, "sink {} expected {} items", sink, want_count);
            }
            Ok(())
        }).unwrap();
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, .. ProptestConfig::default() })]

    /// Loop fan-out: for each input item, port 0 receives one per array
    /// element, and port 1 receives exactly one "done" summary per *iterating*
    /// parent. A non-array parent passes through on port 0 and contributes
    /// nothing to port 1.
    #[test]
    fn loop_fanout_matches_array_length(
        sizes in proptest::collection::vec(0u32..6, 0..6)
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let mut lp = Node::new("lp", NodeKind::Loop);
            lp.params = json!({ "over": "/items" });
            let workflow = wf(
                "wf_loop_prop",
                vec![
                    trigger(),
                    lp,
                    transform_sink("body"),
                    transform_sink("done"),
                ],
                vec![
                    Connection::new("trigger", 0, "lp"),
                    Connection::new("lp", 0, "body"),
                    Connection::new("lp", 1, "done"),
                ],
            );

            // One trigger item per size: { id, items: [..] }
            let inputs: Vec<serde_json::Value> = sizes
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    let arr: Vec<serde_json::Value> = (0..*n)
                        .map(|k| json!({ "k": k }))
                        .collect();
                    json!({ "id": i, "items": arr })
                })
                .collect();

            let log = MemoryEventLog::new();
            let run = engine()
                .run(&workflow, inputs, ExecutionMode::Run, &log)
                .await
                .expect("loop run");
            assert_eq!(run.status, RunStatus::Completed);

            let want_body: usize = sizes.iter().map(|s| *s as usize).sum();
            let want_done: usize = sizes.len(); // every parent iterates (even 0-length emits a summary)

            let got_body = run.node_outputs.get("body").map(Vec::len).unwrap_or(0);
            let got_done = run.node_outputs.get("done").map(Vec::len).unwrap_or(0);
            prop_assert_eq!(got_body, want_body);
            prop_assert_eq!(got_done, want_done);
            Ok(())
        }).unwrap();
    }
}

/// Non-property regression: deterministic re-run identity at the routing level.
/// Two identical runs must produce identical, port-correct outputs (we exploit
/// this in M3 metamorphic-relation 'rerun identity').
#[tokio::test]
async fn branch_run_is_deterministic_across_two_runs() {
    let mut branch_node = Node::new("br", NodeKind::Branch);
    branch_node.params = json!({ "condition": { "path": "/active", "op": "truthy" } });
    let workflow = wf(
        "wf_branch_det",
        vec![
            trigger(),
            branch_node,
            transform_sink("t_true"),
            transform_sink("t_false"),
        ],
        vec![
            Connection::new("trigger", 0, "br"),
            Connection::new("br", 0, "t_true"),
            Connection::new("br", 1, "t_false"),
        ],
    );

    let seed: Vec<serde_json::Value> = (0..8)
        .map(|i| json!({ "id": i, "active": i % 2 == 0 }))
        .collect();

    let l1 = MemoryEventLog::new();
    let r1 = engine()
        .run(&workflow, seed.clone(), ExecutionMode::Run, &l1)
        .await
        .unwrap();
    let l2 = MemoryEventLog::new();
    let r2 = engine()
        .run(&workflow, seed, ExecutionMode::Run, &l2)
        .await
        .unwrap();

    let payload = |r: &a2w_engine::RunResult, k: &str| {
        r.node_outputs
            .get(k)
            .map(|v| v.iter().map(|i| i.json.clone()).collect::<Vec<_>>())
            .unwrap_or_default()
    };
    assert_eq!(payload(&r1, "t_true"), payload(&r2, "t_true"));
    assert_eq!(payload(&r1, "t_false"), payload(&r2, "t_false"));
}
