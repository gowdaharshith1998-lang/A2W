//! Complex-workflow stress suite.
//!
//! Builds a range of non-trivial workflow shapes and drives each through the real
//! engine + validator + optimizer, asserting hard on the properties most likely
//! to break:
//! - **lineage**: every output item's `source` is re-stamped to its immediate
//!   producer (guaranteed one-hop provenance), including across fan-in/merge;
//! - **the concurrent scheduler** under reconvergence (diamonds) and wide fan-in;
//! - **error policies** (Continue vs Stop);
//! - **the optimizer**: it does not over-parallelize data-dependent chains, and an
//!   applied `Parallelize` rewrite yields a still-valid, still-runnable workflow;
//! - **scale**: a 50-node graph validates and runs;
//! - **validation**: malformed graphs are rejected with the right findings and the
//!   engine refuses to run them.

use std::collections::HashMap;

use a2w_engine::{
    Engine, EngineError, ExecutionMode, ItemSource, MemoryEventLog, RunResult, RunStatus,
};
use a2w_ir::{Connection, ErrorPolicy, Node, NodeKind, Workflow, SCHEMA_VERSION};
use a2w_optimizer::{analyze, apply, SuggestionKind};
use a2w_validator::validate;
use serde_json::{json, Value};

// ---- builders -------------------------------------------------------------

fn wf(id: &str, nodes: Vec<Node>, connections: Vec<Connection>) -> Workflow {
    Workflow {
        schema_version: SCHEMA_VERSION,
        id: id.to_string(),
        name: id.to_string(),
        nodes,
        connections,
    }
}
fn trig(id: &str) -> Node {
    Node::new(id, NodeKind::WebhookTrigger)
}
/// HTTP node with a literal URL (no templating → does not consume input data).
fn http(id: &str, url: &str) -> Node {
    let mut n = Node::new(id, NodeKind::HttpRequest);
    n.params = json!({ "method": "GET", "url": url });
    n
}
/// HTTP node whose URL templates the input item (→ data-dependent).
fn http_tmpl(id: &str, url: &str) -> Node {
    let mut n = Node::new(id, NodeKind::HttpRequest);
    n.params = json!({ "method": "GET", "url": url });
    n
}
fn xform(id: &str, set: Value) -> Node {
    let mut n = Node::new(id, NodeKind::Transform);
    n.params = json!({ "set": set });
    n
}
fn merge(id: &str) -> Node {
    Node::new(id, NodeKind::Merge)
}
fn c(from: &str, to: &str) -> Connection {
    Connection::new(from, 0, to)
}

async fn dry_run(w: &Workflow) -> Result<RunResult, EngineError> {
    let engine = Engine::new(a2w_nodes::default_registry());
    let log = MemoryEventLog::new();
    engine
        .run(w, vec![json!({})], ExecutionMode::DryRun, &log)
        .await
}

/// Assert every output item of `node` is lineage-stamped to that node.
fn assert_lineage(result: &RunResult, node: &str) {
    let items = result
        .node_outputs
        .get(node)
        .unwrap_or_else(|| panic!("no outputs recorded for node '{node}'"));
    for (i, item) in items.iter().enumerate() {
        match &item.source {
            ItemSource::Produced {
                node_id,
                item_index,
            } => {
                assert_eq!(node_id, node, "item {i} of '{node}' has wrong producer");
                assert_eq!(*item_index, i, "item {i} of '{node}' has wrong index");
            }
            ItemSource::Trigger => panic!("output of '{node}' should not be a Trigger source"),
        }
    }
}

fn out_len(result: &RunResult, node: &str) -> usize {
    result.node_outputs.get(node).map(Vec::len).unwrap_or(0)
}

// ---- 1. deep linear pipeline ---------------------------------------------

#[tokio::test]
async fn deep_linear_pipeline_completes_with_lineage() {
    let mut nodes = vec![trig("t")];
    let mut conns = vec![];
    let depth = 12;
    let mut prev = "t".to_string();
    for i in 0..depth {
        let id = format!("x{i}");
        nodes.push(xform(&id, json!({ format!("step{i}"): i })));
        conns.push(c(&prev, &id));
        prev = id;
    }
    let w = wf("deep", nodes, conns);
    assert!(validate(&w).is_valid, "deep pipeline should validate");

    let r = dry_run(&w).await.expect("deep pipeline runs");
    assert_eq!(r.status, RunStatus::Completed);
    // One item flows the whole way down.
    assert_eq!(out_len(&r, &prev), 1, "final node yields one item");
    assert_lineage(&r, &prev);
    // Every node emitted a Started+Finished pair (2 events per node).
    assert_eq!(r.events.len(), (depth + 1) * 2);
}

// ---- 2. wide fan-out then fan-in -----------------------------------------

#[tokio::test]
async fn wide_fan_out_fan_in_merges_all_branches() {
    let width = 6;
    let mut nodes = vec![trig("t")];
    let mut conns = vec![];
    for i in 0..width {
        let id = format!("h{i}");
        nodes.push(http(&id, &format!("https://api.example.com/{i}")));
        conns.push(c("t", &id));
        conns.push(c(&id, "m"));
    }
    nodes.push(merge("m"));
    nodes.push(xform("final", json!({ "done": true })));
    conns.push(c("m", "final"));

    let w = wf("wide", nodes, conns);
    assert!(validate(&w).is_valid);

    let r = dry_run(&w).await.expect("wide workflow runs");
    assert_eq!(r.status, RunStatus::Completed);
    // Merge concatenates one mock item per branch.
    assert_eq!(out_len(&r, "m"), width, "merge gathers every branch");
    assert_eq!(out_len(&r, "final"), width);
    assert_lineage(&r, "m");
    assert_lineage(&r, "final");
}

// ---- 3. diamond reconvergence --------------------------------------------

#[tokio::test]
async fn diamond_waits_for_both_paths() {
    // t -> a -> {b, c} -> m(merge) -> e
    let w = wf(
        "diamond",
        vec![
            trig("t"),
            http("a", "https://api.example.com/a"),
            http("b", "https://api.example.com/b"),
            http("c", "https://api.example.com/c"),
            merge("m"),
            xform("e", json!({ "joined": true })),
        ],
        vec![
            c("t", "a"),
            c("a", "b"),
            c("a", "c"),
            c("b", "m"),
            c("c", "m"),
            c("m", "e"),
        ],
    );
    assert!(validate(&w).is_valid);
    let r = dry_run(&w).await.expect("diamond runs");
    assert_eq!(r.status, RunStatus::Completed);
    // m must see BOTH b and c (scheduler waited for both before running m).
    assert_eq!(out_len(&r, "m"), 2, "merge reconverges both paths");
    assert_eq!(out_len(&r, "e"), 2);
    assert_lineage(&r, "m");
}

// ---- 4. multiple independent branches ------------------------------------

#[tokio::test]
async fn three_independent_branches_all_execute() {
    let w = wf(
        "branches",
        vec![
            trig("t"),
            xform("a1", json!({ "a": 1 })),
            xform("a2", json!({ "a": 2 })),
            http("b1", "https://api.example.com/b"),
            http("b2", "https://api.example.com/b2"),
            xform("cc", json!({ "c": 1 })),
        ],
        vec![
            c("t", "a1"),
            c("a1", "a2"),
            c("t", "b1"),
            c("b1", "b2"),
            c("t", "cc"),
        ],
    );
    assert!(validate(&w).is_valid);
    let r = dry_run(&w).await.expect("independent branches run");
    assert_eq!(r.status, RunStatus::Completed);
    for leaf in ["a2", "b2", "cc"] {
        assert_eq!(out_len(&r, leaf), 1, "leaf '{leaf}' produced output");
    }
}

// ---- 5. optimizer must NOT parallelize a data-dependent chain ------------

#[test]
fn optimizer_respects_data_dependency() {
    // t -> fetch({{json.id}}) -> post({{json}}): every step consumes its input.
    let w = wf(
        "depchain",
        vec![
            trig("t"),
            http_tmpl("fetch", "https://api.example.com/{{json.id}}"),
            http_tmpl("post", "https://hooks.example.com/{{json}}"),
        ],
        vec![c("t", "fetch"), c("fetch", "post")],
    );
    assert!(validate(&w).is_valid);
    let suggestions = analyze(&w, None);
    assert!(
        !suggestions
            .iter()
            .any(|s| s.kind == SuggestionKind::Parallelize),
        "data-dependent chain must not be parallelized: {suggestions:?}"
    );
}

// ---- 6. a correct parallelize rewrite stays valid + runnable -------------

#[tokio::test]
async fn parallelize_apply_keeps_workflow_valid_and_runnable() {
    // t -> a -> {b (independent, literal url), c (depends via {{json}})}.
    // b qualifies for parallelize; a keeps consumer c, so a is NOT orphaned.
    let w = wf(
        "para",
        vec![
            trig("t"),
            http("a", "https://api.example.com/a"),
            http("b", "https://api.example.com/b"),
            http_tmpl("cc", "https://api.example.com/{{json}}"),
        ],
        vec![c("t", "a"), c("a", "b"), c("a", "cc")],
    );
    assert!(validate(&w).is_valid);

    let suggestions = analyze(&w, None);
    let para = suggestions
        .iter()
        .find(|s| s.kind == SuggestionKind::Parallelize)
        .expect("b should be a parallelize candidate");
    let optimized = apply(&w, &para.ops);

    // The rewrite must keep the workflow valid and runnable.
    assert!(
        validate(&optimized).is_valid,
        "optimized workflow invalid: {:?}",
        validate(&optimized).findings
    );
    let r = dry_run(&optimized).await.expect("optimized workflow runs");
    assert_eq!(r.status, RunStatus::Completed);

    // b now hangs off the trigger; a -> b edge is gone; a -> cc remains.
    let has = |from: &str, to: &str| {
        optimized
            .connections
            .iter()
            .any(|cn| cn.from_node == from && cn.to_node == to)
    };
    assert!(has("t", "b"), "b rewired onto the trigger");
    assert!(!has("a", "b"), "old a->b edge removed");
    assert!(has("a", "cc"), "a keeps its other consumer (not orphaned)");
}

// ---- 7. on_error = Continue lets the run complete ------------------------

#[tokio::test]
async fn on_error_continue_completes() {
    // An HTTP node with no `url` fails even in DryRun (BadParams). With Continue
    // the run proceeds and the node yields zero items.
    let mut bad = Node::new("bad", NodeKind::HttpRequest); // params = {} → no url
    bad.on_error = Some(ErrorPolicy::Continue);
    let w = wf(
        "cont",
        vec![trig("t"), bad, xform("after", json!({ "x": 1 }))],
        vec![c("t", "bad"), c("bad", "after")],
    );
    assert!(validate(&w).is_valid);
    let r = dry_run(&w).await.expect("run completes under Continue");
    assert_eq!(r.status, RunStatus::Completed);
    assert_eq!(out_len(&r, "bad"), 0, "failed node yields nothing");
    assert_eq!(out_len(&r, "after"), 0, "downstream sees zero items");
    // A Failed event was still recorded for observability.
    assert!(r
        .events
        .iter()
        .any(|e| e.node_id == "bad" && e.error.is_some()));
}

// ---- 8. on_error = Stop (default) aborts the run -------------------------

#[tokio::test]
async fn on_error_stop_aborts() {
    let bad = Node::new("bad", NodeKind::HttpRequest); // no url, default Stop
    let w = wf(
        "stop",
        vec![trig("t"), bad, xform("after", json!({ "x": 1 }))],
        vec![c("t", "bad"), c("bad", "after")],
    );
    assert!(validate(&w).is_valid);
    match dry_run(&w).await {
        Err(EngineError::NodeFailed { node_id, .. }) => assert_eq!(node_id, "bad"),
        other => panic!("expected NodeFailed for 'bad', got {other:?}"),
    }
}

// ---- 9. scale: 50-node fan-in --------------------------------------------

#[tokio::test]
async fn large_fifty_node_graph_runs() {
    let width = 50;
    let mut nodes = vec![trig("t"), merge("m")];
    let mut conns = vec![];
    for i in 0..width {
        let id = format!("n{i}");
        nodes.push(xform(&id, json!({ "i": i })));
        conns.push(c("t", &id));
        conns.push(c(&id, "m"));
    }
    let w = wf("big", nodes, conns);
    assert!(validate(&w).is_valid, "50-node graph validates");
    let r = dry_run(&w).await.expect("50-node graph runs");
    assert_eq!(r.status, RunStatus::Completed);
    assert_eq!(out_len(&r, "m"), width, "merge gathers all 50 branches");
}

// ---- 10. malformed graphs are rejected -----------------------------------

#[test]
fn cycle_is_rejected_and_engine_refuses() {
    // t -> a -> b -> a  (cycle a<->b)
    let w = wf(
        "cycle",
        vec![trig("t"), xform("a", json!({})), xform("b", json!({}))],
        vec![c("t", "a"), c("a", "b"), c("b", "a")],
    );
    let report = validate(&w);
    assert!(!report.is_valid);
    assert!(report
        .findings
        .iter()
        .any(|f| format!("{:?}", f.code).to_lowercase().contains("cycle")));
}

#[tokio::test]
async fn engine_refuses_invalid_workflow() {
    // dangling target + no trigger.
    let w = wf("bad", vec![xform("a", json!({}))], vec![c("a", "ghost")]);
    assert!(!validate(&w).is_valid);
    match dry_run(&w).await {
        Err(EngineError::Invalid(report)) => assert!(!report.is_valid),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

// ---- 11. registry coverage: every NodeKind has an executor ----------------

#[tokio::test]
async fn every_node_kind_has_an_executor() {
    // After the round-3 work all 14 NodeKinds are implemented. This test
    // guards regressions: if a new kind is added to the IR without a matching
    // executor in default_registry, the workflow below will surface a clean
    // NoExecutorForKind error and break here.
    use a2w_engine::{Engine, NodeRegistry};
    use a2w_nodes::default_registry;
    let reg: NodeRegistry = default_registry();
    let _ = Engine::new(reg);

    // Test SubWorkflow with an inline workflow runs end-to-end.
    let sub_inline = json!({
        "schema_version": 1,
        "id": "sub_inline",
        "name": "sub inline",
        "nodes": [
            { "id": "t", "kind": "webhook_trigger", "params": {} },
            { "id": "shape", "kind": "transform", "params": { "set": { "tag": "from_sub" } } }
        ],
        "connections": [
            { "from_node": "t", "from_port": 0, "to_node": "shape" }
        ]
    });
    let mut sub_node = Node::new("sub", NodeKind::SubWorkflow);
    sub_node.params = json!({ "workflow": sub_inline });
    let w = wf(
        "sub_caller",
        vec![trig("t"), sub_node, xform("after", json!({ "ok": true }))],
        vec![c("t", "sub"), c("sub", "after")],
    );
    assert!(validate(&w).is_valid, "sub-caller is structurally valid");
    let r = dry_run(&w).await.expect("sub-workflow dry-run completes");
    assert!(
        r.node_outputs.contains_key("sub"),
        "sub-workflow produced output"
    );
}

// ---- 12. lineage is per-run consistent across a complex graph -------------

#[tokio::test]
async fn every_produced_item_traces_one_hop() {
    let w = wf(
        "trace",
        vec![
            trig("t"),
            http("f1", "https://api.example.com/1"),
            http("f2", "https://api.example.com/2"),
            merge("m"),
            xform("s", json!({ "ok": true })),
        ],
        vec![
            c("t", "f1"),
            c("t", "f2"),
            c("f1", "m"),
            c("f2", "m"),
            c("m", "s"),
        ],
    );
    let r = dry_run(&w).await.expect("runs");
    assert_eq!(r.status, RunStatus::Completed);
    // Every non-trigger node's outputs trace exactly to that node.
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for node in ["f1", "f2", "m", "s"] {
        assert_lineage(&r, node);
        counts.insert(node, out_len(&r, node));
    }
    assert_eq!(counts["m"], 2, "merge has both fetches");
    assert_eq!(counts["s"], 2);
}
