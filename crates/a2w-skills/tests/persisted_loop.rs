//! F4 integration: the full loop through the PERSISTED surface.
//!
//! author → verify (holdout) → promote → retrieve a *persisted* skill — using
//! `PersistentSkillLibrary` over a real `a2w_store::Store`, not the in-memory
//! library. Also asserts the v6 migration round-trips and promotion stays gated
//! on the holdout report.

use a2w_ir::{Connection, Node, NodeKind, Workflow, SCHEMA_VERSION};
use a2w_skills::{PersistError, PersistentSkillLibrary};
use a2w_store::Store;
use a2w_verify::{
    verify, SemanticRelation, SemanticSuite, SpecAssertion, VerificationHarness, VerificationPlan,
    WorkflowSpec,
};
use serde_json::{json, Value};

fn tagging_workflow(id: &str) -> Workflow {
    let trigger = Node::new("trigger", NodeKind::WebhookTrigger);
    let mut tag = Node::new("tag", NodeKind::Transform);
    tag.params = json!({ "set": { "tagged": true } });
    Workflow {
        schema_version: SCHEMA_VERSION,
        id: id.to_string(),
        name: id.to_string(),
        nodes: vec![trigger, tag],
        connections: vec![Connection::new("trigger", 0, "tag")],
    }
}

fn seed(n: usize) -> Vec<Value> {
    (0..n).map(|i| json!({ "id": i })).collect()
}

/// A holdout plan with real outcome evidence (spec + semantic).
async fn holdout_report(wf: &Workflow, observe: &str) -> a2w_verify::ConfidenceReport {
    let harness = VerificationHarness::new();
    let plan = VerificationPlan::new(observe)
        .with_spec(WorkflowSpec {
            input: seed(4),
            assertions: vec![SpecAssertion::EveryItemFieldEquals {
                path: "/tagged".to_string(),
                value: json!(true),
            }],
        })
        .with_semantic(SemanticSuite::new(vec![SemanticRelation::AppendAddsOutputs {
            base_input: seed(4),
            passing_extra: vec![json!({ "id": 1000 })],
            per_item: 1,
        }]));
    verify(&harness, wf, &plan).await.expect("verify")
}

#[tokio::test]
async fn full_loop_through_persisted_store() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let lib = PersistentSkillLibrary::with_default_threshold(&store);
    assert!(lib.is_empty().await.unwrap());

    // author + verify (holdout) + promote -> persisted.
    let wf = tagging_workflow("wf_tag");
    let report = holdout_report(&wf, "tag").await;
    let id = lib
        .promote("tag each incoming alert", wf, "tag", &report)
        .await
        .expect("promote persists");
    assert_eq!(lib.len().await.unwrap(), 1);

    // Retrieve for a SIMILAR query — from the durable store, via a FRESH handle
    // so we know it really round-tripped through SQLite (nothing in memory).
    let lib2 = PersistentSkillLibrary::with_default_threshold(&store);
    let (skill, sim) = lib2
        .best_match("tag the incoming alerts")
        .await
        .expect("retrieve")
        .expect("a match exists");
    assert_eq!(skill.id, id);
    assert!(sim > 0.0);
    // The persisted skill carries its certified evidence and runnable IR.
    assert_eq!(skill.observe_node, "tag");
    assert_eq!(skill.evidence.score, 1.0);
    assert!(a2w_validator::validate(&skill.workflow).is_valid);

    // get() by id round-trips the full skill.
    let got = lib2.get(&id).await.unwrap().expect("present");
    assert_eq!(got.workflow, skill.workflow);
}

#[tokio::test]
async fn persisted_promotion_is_gated_on_holdout_evidence() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let lib = PersistentSkillLibrary::with_default_threshold(&store);

    // An evidence-free report (ran, no checks) must NOT be promoted/persisted.
    let wf = tagging_workflow("wf_tag");
    let empty = a2w_verify::ConfidenceReport::new("wf_tag", "tag");
    let err = lib
        .promote("tag", wf.clone(), "tag", &empty)
        .await
        .expect_err("evidence-free promotion must fail");
    assert!(matches!(
        err,
        PersistError::Skill(a2w_skills::SkillError::BelowThreshold { .. })
    ));
    assert!(lib.is_empty().await.unwrap(), "nothing persisted");

    // With real holdout evidence the same workflow IS persisted.
    let report = holdout_report(&wf, "tag").await;
    lib.promote("tag", wf, "tag", &report).await.expect("promote");
    assert_eq!(lib.len().await.unwrap(), 1);
}

#[tokio::test]
async fn re_promotion_upserts_not_duplicates() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let lib = PersistentSkillLibrary::with_default_threshold(&store);
    let wf = tagging_workflow("wf_tag");
    let report = holdout_report(&wf, "tag").await;

    let id1 = lib.promote("tag alerts", wf.clone(), "tag", &report).await.unwrap();
    let id2 = lib
        .promote("tag the alerts differently", wf, "tag", &report)
        .await
        .unwrap();
    assert_eq!(id1, id2, "same workflow → same content-derived id");
    assert_eq!(lib.len().await.unwrap(), 1, "re-promotion upserts");
}
