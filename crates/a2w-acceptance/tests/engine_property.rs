//! Property tests for the validate ↔ execute contract on *random* pipelines.
//!
//! Complements `port_routing_proptest` (which targets fan-out routing) with the
//! whole-engine determinism contract:
//!
//! > For any randomly-generated, statically-valid pure-logic pipeline:
//! >   1. the engine runs it to `Completed`,
//! >   2. a re-run is byte-identical (determinism / zero hidden state), and
//! >   3. production (`Run`) and `DryRun` produce identical output (no
//! >      mode-dependent drift for pure logic).
//!
//! Pure-logic pipelines are zero-token by construction, so this also exercises
//! the "deterministic, zero-token execution" invariant across a wide input space.

use a2w_engine::{Engine, ExecutionMode, MemoryEventLog, RunStatus};
use a2w_ir::{Connection, Node, NodeKind, Workflow, SCHEMA_VERSION};
use a2w_validator::validate;
use proptest::prelude::*;
use serde_json::json;

/// One transform stage: set `field` to either a constant or a simple arithmetic
/// expression over the running accumulator `n` (which the trigger seeds).
#[derive(Debug, Clone)]
enum Stage {
    Const(String, i64),
    AddN(String, i64),
    MulN(String, i64),
}

fn stage_strategy() -> impl Strategy<Value = Stage> {
    let field = "[a-e]"; // small alphabet → fields collide sometimes (last-writer-wins)
    prop_oneof![
        (field, -50i64..50).prop_map(|(f, v)| Stage::Const(f, v)),
        (field, -20i64..20).prop_map(|(f, v)| Stage::AddN(f, v)),
        (field, 1i64..8).prop_map(|(f, v)| Stage::MulN(f, v)),
    ]
}

fn build(stages: &[Stage]) -> Workflow {
    let mut nodes = vec![Node::new("trigger", NodeKind::WebhookTrigger)];
    let mut connections = Vec::new();
    let mut prev = "trigger".to_string();
    for (i, st) in stages.iter().enumerate() {
        let id = format!("s{i}");
        let mut node = Node::new(&id, NodeKind::Transform);
        node.params = match st {
            Stage::Const(f, v) => json!({ "set": { f: v } }),
            Stage::AddN(f, v) => json!({ "set": { f: format!("${{{{ $.n + {v} }}}}") } }),
            Stage::MulN(f, v) => json!({ "set": { f: format!("${{{{ $.n * {v} }}}}") } }),
        };
        nodes.push(node);
        connections.push(Connection::new(&prev, 0, &id));
        prev = id;
    }
    Workflow {
        schema_version: SCHEMA_VERSION,
        id: "wf_prop".to_string(),
        name: "prop".to_string(),
        nodes,
        connections,
    }
}

fn run(wf: &Workflow, mode: ExecutionMode) -> (RunStatus, serde_json::Value) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let engine = Engine::new(a2w_nodes::default_registry());
        let log = MemoryEventLog::new();
        let result = engine
            .run(wf, vec![json!({ "n": 3 })], mode, &log)
            .await
            .expect("validated pure-logic pipeline runs");
        (
            result.status,
            serde_json::to_value(&result.node_outputs).unwrap(),
        )
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn random_pipeline_is_valid_deterministic_and_mode_invariant(
        stages in prop::collection::vec(stage_strategy(), 1..7)
    ) {
        let wf = build(&stages);

        // 1) A pure-logic transform chain is always statically valid.
        prop_assert!(validate(&wf).is_valid, "generated pipeline must validate");

        // 2) Production run completes, and 3) a re-run is byte-identical.
        let (s1, out1) = run(&wf, ExecutionMode::Run);
        let (s2, out2) = run(&wf, ExecutionMode::Run);
        prop_assert_eq!(s1, RunStatus::Completed);
        prop_assert_eq!(s2, RunStatus::Completed);
        prop_assert_eq!(&out1, &out2, "run must be deterministic across reruns");

        // 4) Run ≡ DryRun for pure logic (no mode-dependent drift).
        let (sd, outd) = run(&wf, ExecutionMode::DryRun);
        prop_assert_eq!(sd, RunStatus::Completed);
        prop_assert_eq!(&out1, &outd, "Run and DryRun must agree for pure logic");
    }
}
