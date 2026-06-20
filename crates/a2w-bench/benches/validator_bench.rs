//! Validator throughput at varying workflow sizes.

use a2w_ir::{Connection, Node, NodeKind, Workflow, SCHEMA_VERSION};
use a2w_validator::validate;
use criterion::{criterion_group, criterion_main, Criterion};

fn build_wf(n: usize) -> Workflow {
    let mut nodes = vec![Node::new("trigger", NodeKind::WebhookTrigger)];
    let mut conns = Vec::new();
    let mut prev = "trigger".to_string();
    for i in 0..n {
        let id = format!("n{i}");
        nodes.push(Node::new(&id, NodeKind::Transform));
        conns.push(Connection::new(&prev, 0, &id));
        prev = id;
    }
    Workflow {
        schema_version: SCHEMA_VERSION,
        id: "wf_bench".into(),
        name: "Bench".into(),
        nodes,
        connections: conns,
    }
}

fn bench(c: &mut Criterion) {
    for n in [10, 100, 1000] {
        let wf = build_wf(n);
        c.bench_function(&format!("validator/chain/{n}"), |b| {
            b.iter(|| validate(&wf))
        });
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
