//! M5 integration: search measurably improves a seed's confidence score, and
//! every candidate it generates is M1-valid by construction.

use a2w_ir::{Connection, Node, NodeKind, Workflow, SCHEMA_VERSION};
use a2w_search::{
    evolve, InsertPassthrough, Mutation, RemovePassthrough, SearchConfig, SetTransformField,
};
use a2w_verify::{
    CountOp, MetamorphicSuite, SpecAssertion, VerificationHarness, VerificationPlan, WorkflowSpec,
};
use serde_json::{json, Value};

fn wf(id: &str, nodes: Vec<Node>, connections: Vec<Connection>) -> Workflow {
    Workflow {
        schema_version: SCHEMA_VERSION,
        id: id.to_string(),
        name: id.to_string(),
        nodes,
        connections,
    }
}

/// A seed that does NOT satisfy the spec: it tags items with the wrong field.
/// The held-out spec requires every output item to have `/tagged == true`.
fn seed_workflow() -> Workflow {
    let trigger = Node::new("trigger", NodeKind::WebhookTrigger);
    let mut tag = Node::new("tag", NodeKind::Transform);
    tag.params = json!({ "set": { "irrelevant": 1 } }); // missing `tagged`
    wf(
        "wf_seed",
        vec![trigger, tag],
        vec![Connection::new("trigger", 0, "tag")],
    )
}

fn held_out_plan() -> VerificationPlan {
    let input: Vec<Value> = (0..4).map(|i| json!({ "id": i })).collect();
    VerificationPlan::new("tag")
        .with_spec(WorkflowSpec {
            input: input.clone(),
            assertions: vec![
                SpecAssertion::OutputCount {
                    op: CountOp::Eq,
                    count: 4,
                },
                SpecAssertion::EveryItemFieldEquals {
                    path: "/tagged".to_string(),
                    value: json!(true),
                },
            ],
        })
        .with_metamorphic(MetamorphicSuite::standard(input))
}

fn operators() -> Vec<Box<dyn Mutation>> {
    vec![
        Box::new(SetTransformField {
            // The vocabulary includes the field the spec wants — plus noise the
            // search must reject in favour of the one that improves fitness.
            vocabulary: vec![
                ("tagged".to_string(), json!(true)),
                ("tagged".to_string(), json!(false)),
                ("noise".to_string(), json!("x")),
            ],
            frozen: vec![],
        }),
        Box::new(InsertPassthrough),
        Box::new(RemovePassthrough { frozen: vec!["tag".to_string()] }),
    ]
}

#[tokio::test]
async fn search_improves_seed_confidence_on_held_out_task() {
    let harness = VerificationHarness::new();
    let seed = seed_workflow();
    let plan = held_out_plan();
    let ops = operators();

    // Sanity: the seed genuinely fails the spec (so there's room to improve).
    let seed_report = a2w_verify::verify(&harness, &seed, &plan).await.unwrap();
    assert!(
        seed_report.score() < 1.0,
        "seed should be imperfect to start:\n{}",
        seed_report.summary()
    );

    let outcome = evolve(&harness, &seed, &plan, &ops, SearchConfig::default())
        .await
        .expect("search runs");

    assert!(
        outcome.improved(),
        "search must measurably improve the score: {} -> {}",
        outcome.initial_score,
        outcome.best_score
    );
    assert!(
        outcome.best_score >= 1.0,
        "search should reach a perfect score by adding the required field; got {} (\n{}\n)",
        outcome.best_score,
        outcome.best_report.summary()
    );

    // The improved workflow actually sets `tagged: true`.
    let best_out = harness
        .observe(&outcome.best_workflow, "tag", vec![json!({ "id": 99 })])
        .await
        .unwrap();
    assert_eq!(best_out.len(), 1);
    assert_eq!(best_out[0]["tagged"], json!(true));

    // The best workflow is M1-valid.
    assert!(a2w_validator::validate(&outcome.best_workflow).is_valid);
}

#[tokio::test]
async fn every_generated_candidate_is_valid_by_construction() {
    // Drive the operators directly and assert every emitted candidate passes
    // M1 — the structural guarantee the search relies on.
    let seed = seed_workflow();
    let ops = operators();
    let mut total = 0usize;
    for op in &ops {
        for cand in op.apply(&seed) {
            total += 1;
            assert!(
                a2w_validator::validate(&cand).is_valid,
                "operator {} produced an invalid candidate: {:?}",
                op.name(),
                a2w_validator::validate(&cand).findings
            );
        }
    }
    assert!(total > 0, "operators should produce candidates");
}

#[tokio::test]
async fn search_is_deterministic() {
    let harness = VerificationHarness::new();
    let seed = seed_workflow();
    let plan = held_out_plan();

    let a = evolve(&harness, &seed, &plan, &operators(), SearchConfig::default())
        .await
        .unwrap();
    let b = evolve(&harness, &seed, &plan, &operators(), SearchConfig::default())
        .await
        .unwrap();

    assert_eq!(a.best_score, b.best_score);
    assert_eq!(a.candidates_evaluated, b.candidates_evaluated);
    // Byte-identical best workflow across runs.
    assert_eq!(
        serde_json::to_string(&a.best_workflow).unwrap(),
        serde_json::to_string(&b.best_workflow).unwrap()
    );
}

#[tokio::test]
async fn search_on_already_perfect_seed_does_not_regress() {
    let harness = VerificationHarness::new();
    // A seed that already satisfies the spec.
    let trigger = Node::new("trigger", NodeKind::WebhookTrigger);
    let mut tag = Node::new("tag", NodeKind::Transform);
    tag.params = json!({ "set": { "tagged": true } });
    let good = wf(
        "wf_good",
        vec![trigger, tag],
        vec![Connection::new("trigger", 0, "tag")],
    );
    let plan = held_out_plan();
    let outcome = evolve(&harness, &good, &plan, &operators(), SearchConfig::default())
        .await
        .unwrap();
    assert_eq!(outcome.initial_score, 1.0);
    assert_eq!(outcome.best_score, 1.0);
    assert!(!outcome.improved(), "nothing to improve on a perfect seed");
}
