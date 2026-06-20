//! Engine throughput benchmarks: sequential chains, parallel chains, and
//! wide-fanout (one trigger → N transforms).

use a2w_engine::{Engine, ExecutionMode, MemoryEventLog};
use a2w_ir::{Connection, Node, NodeKind, Workflow, SCHEMA_VERSION};
use a2w_nodes::default_registry;
use criterion::{criterion_group, criterion_main, Criterion};

fn build_chain(n: usize) -> Workflow {
    let mut nodes = vec![Node::new("trigger", NodeKind::WebhookTrigger)];
    let mut conns = Vec::new();
    let mut prev = "trigger".to_string();
    for i in 0..n {
        let id = format!("t{i}");
        let mut node = Node::new(&id, NodeKind::Transform);
        node.params = serde_json::json!({ "set": { "i": i } });
        nodes.push(node);
        conns.push(Connection::new(&prev, 0, &id));
        prev = id;
    }
    Workflow {
        schema_version: SCHEMA_VERSION,
        id: "wf_chain".into(),
        name: "Chain".into(),
        nodes,
        connections: conns,
    }
}

fn build_wide(n: usize) -> Workflow {
    let mut nodes = vec![Node::new("trigger", NodeKind::WebhookTrigger)];
    let mut conns = Vec::new();
    for i in 0..n {
        let id = format!("t{i}");
        let mut node = Node::new(&id, NodeKind::Transform);
        node.params = serde_json::json!({ "set": { "i": i } });
        nodes.push(node);
        conns.push(Connection::new("trigger", 0, &id));
    }
    Workflow {
        schema_version: SCHEMA_VERSION,
        id: "wf_wide".into(),
        name: "Wide".into(),
        nodes,
        connections: conns,
    }
}

fn run_workflow_sync(wf: &Workflow) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let engine = Engine::new(default_registry());
        let log = MemoryEventLog::new();
        let _ = engine
            .run(
                wf,
                vec![serde_json::json!({ "id": 1 })],
                ExecutionMode::DryRun,
                &log,
            )
            .await
            .expect("ok");
    });
}

fn bench(c: &mut Criterion) {
    let chain_10 = build_chain(10);
    let chain_100 = build_chain(100);
    let wide_10 = build_wide(10);
    let wide_100 = build_wide(100);
    c.bench_function("engine/chain/10", |b| b.iter(|| run_workflow_sync(&chain_10)));
    c.bench_function("engine/chain/100", |b| b.iter(|| run_workflow_sync(&chain_100)));
    c.bench_function("engine/wide/10", |b| b.iter(|| run_workflow_sync(&wide_10)));
    c.bench_function("engine/wide/100", |b| b.iter(|| run_workflow_sync(&wide_100)));
}

criterion_group!(benches, bench);
criterion_main!(benches);
