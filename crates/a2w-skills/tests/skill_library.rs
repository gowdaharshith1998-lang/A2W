//! M4 integration: promote → retrieve → compose, with promotion gated on M3.

use a2w_ir::{Connection, Node, NodeKind, Workflow, SCHEMA_VERSION};
use a2w_skills::{adapt, compose_sequential, SkillError, SkillLibrary};
use a2w_verify::{
    verify, MetamorphicSuite, VerificationHarness, VerificationPlan,
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

fn trigger() -> Node {
    Node::new("trigger", NodeKind::WebhookTrigger)
}

/// trigger -> tag(Transform). A clean per-item map.
fn tagging_workflow(id: &str) -> Workflow {
    let mut tag = Node::new("tag", NodeKind::Transform);
    tag.params = json!({ "set": { "tagged": true } });
    wf(
        id,
        vec![trigger(), tag],
        vec![Connection::new("trigger", 0, "tag")],
    )
}

/// trigger -> br(filter keep==true) -> sink. A clean per-item filter.
fn filter_workflow(id: &str) -> Workflow {
    let mut br = Node::new("br", NodeKind::Branch);
    br.params = json!({ "condition": { "path": "/keep", "op": "eq", "value": true } });
    let mut sink = Node::new("sink", NodeKind::Transform);
    sink.params = json!({ "set": { "kept": true } });
    wf(
        id,
        vec![trigger(), br, sink],
        vec![
            Connection::new("trigger", 0, "br"),
            Connection::new("br", 0, "sink"),
        ],
    )
}

fn seed(n: usize) -> Vec<Value> {
    (0..n).map(|i| json!({ "id": i, "keep": i % 2 == 0 })).collect()
}

async fn confidence_for(wf: &Workflow, observe: &str) -> a2w_verify::ConfidenceReport {
    let harness = VerificationHarness::new();
    let plan = VerificationPlan::new(observe)
        .with_metamorphic(MetamorphicSuite::standard(seed(6)));
    verify(&harness, wf, &plan).await.expect("verify")
}

#[tokio::test]
async fn promote_retrieve_and_adapt() {
    let mut lib = SkillLibrary::with_default_threshold();

    // A verified tagging workflow is promoted.
    let tagging = tagging_workflow("wf_tag");
    let report = confidence_for(&tagging, "tag").await;
    let id = lib
        .promote("tag each incoming alert event", tagging, "tag", &report)
        .expect("clean workflow clears threshold");
    assert_eq!(lib.len(), 1);

    // A similar query retrieves it.
    let (skill, sim) = lib
        .best_match("tag the incoming alerts")
        .expect("a match exists");
    assert_eq!(skill.id, id);
    assert!(sim > 0.0, "similarity should be positive: {sim}");

    // It can be adapted (re-id) for a new instance, staying valid.
    let adapted = adapt(skill, "wf_tag_v2", "Tagging v2").expect("adapt");
    assert_eq!(adapted.id, "wf_tag_v2");
    assert!(a2w_validator::validate(&adapted).is_valid);

    // And the adapted workflow still verifies (expertise transferred intact).
    let re_report = confidence_for(&adapted, "tag").await;
    assert!(re_report.meets(lib.threshold()));
}

#[tokio::test]
async fn promotion_is_gated_on_m3_not_execution() {
    let mut lib = SkillLibrary::with_default_threshold();

    // Build an EMPTY confidence report (the workflow "ran" but carries no
    // evidence). Promotion must be refused.
    let tagging = tagging_workflow("wf_tag");
    let empty_report = a2w_verify::ConfidenceReport::new("wf_tag", "tag");
    let err = lib
        .promote("tag alerts", tagging.clone(), "tag", &empty_report)
        .expect_err("an evidence-free workflow must not be promoted");
    assert!(matches!(err, SkillError::BelowThreshold { .. }));
    assert!(lib.is_empty(), "nothing should have been stored");

    // With real M3 evidence, the same workflow IS promoted.
    let report = confidence_for(&tagging, "tag").await;
    lib.promote("tag alerts", tagging, "tag", &report)
        .expect("with evidence, promotion succeeds");
    assert_eq!(lib.len(), 1);
}

#[tokio::test]
async fn invalid_workflow_cannot_be_promoted() {
    let mut lib = SkillLibrary::with_default_threshold();
    // http_request with no url → M1 invalid.
    let bad = wf(
        "wf_bad",
        vec![trigger(), Node::new("fetch", NodeKind::HttpRequest)],
        vec![Connection::new("trigger", 0, "fetch")],
    );
    // Even with a (fabricated, all-passing) report, an invalid workflow is
    // refused — skills are valid-by-construction.
    let mut report = a2w_verify::ConfidenceReport::new("wf_bad", "fetch");
    for i in 0..4 {
        report.push(a2w_verify::CheckResult::pass(
            a2w_verify::CheckCategory::Metamorphic,
            format!("mr{i}"),
            "held",
        ));
    }
    let err = lib
        .promote("fetch", bad, "fetch", &report)
        .expect_err("invalid IR cannot be a skill");
    assert!(matches!(err, SkillError::Invalid(_)));
}

#[tokio::test]
async fn report_must_match_workflow() {
    let mut lib = SkillLibrary::with_default_threshold();
    let tagging = tagging_workflow("wf_tag");
    // Report computed for a DIFFERENT workflow id.
    let report = confidence_for(&filter_workflow("wf_other"), "sink").await;
    let err = lib
        .promote("tag", tagging, "tag", &report)
        .expect_err("a mismatched report must be rejected");
    assert!(matches!(err, SkillError::ReportMismatch { .. }));
}

#[tokio::test]
async fn compose_two_skills_sequentially() {
    let mut lib = SkillLibrary::with_default_threshold();

    // Promote a filter skill and a tagging skill.
    let filter = filter_workflow("wf_filter");
    let fr = confidence_for(&filter, "sink").await;
    let filter_id = lib
        .promote("keep only items to retain", filter, "sink", &fr)
        .expect("filter promoted");

    let tagging = tagging_workflow("wf_tag");
    let tr = confidence_for(&tagging, "tag").await;
    let tag_id = lib
        .promote("tag each item", tagging, "tag", &tr)
        .expect("tagging promoted");

    let filter_skill = lib.get(&filter_id).unwrap();
    let tag_skill = lib.get(&tag_id).unwrap();

    // Compose: filter THEN tag. The result must be a single valid DAG.
    let (composed, observe) =
        compose_sequential(filter_skill, tag_skill, "wf_composed", "Filter then tag")
            .expect("compose");
    assert!(
        a2w_validator::validate(&composed).is_valid,
        "composed workflow must be valid: {:?}",
        a2w_validator::validate(&composed).findings
    );
    assert_eq!(observe, "b_tag");

    // Exactly one trigger survives (A's).
    let triggers = composed.nodes.iter().filter(|n| n.kind.is_trigger()).count();
    assert_eq!(triggers, 1);

    // The composed workflow runs and is itself verifiable: feed items, observe
    // that only kept items reach the tagger, all tagged.
    let harness = VerificationHarness::new();
    let out = harness
        .observe(&composed, &observe, seed(6))
        .await
        .expect("composed run");
    // seed(6): ids 0..6, keep == even → 3 items survive the filter.
    assert_eq!(out.len(), 3, "only kept items flow through: {out:?}");
    assert!(out.iter().all(|it| it["tagged"] == json!(true)));
}

#[tokio::test]
async fn retrieve_ranks_by_similarity_deterministically() {
    let mut lib = SkillLibrary::with_default_threshold();

    let tagging = tagging_workflow("wf_tag");
    let tr = confidence_for(&tagging, "tag").await;
    lib.promote("summarize and tag alert events", tagging, "tag", &tr)
        .unwrap();

    let filter = filter_workflow("wf_filter");
    let fr = confidence_for(&filter, "sink").await;
    lib.promote("filter currency exchange records", filter, "sink", &fr)
        .unwrap();

    // A query close to the tagging skill should rank it first.
    let ranked = lib.retrieve("tag the alert events and summarize", 2);
    assert_eq!(ranked.len(), 2);
    assert_eq!(ranked[0].0.query, "summarize and tag alert events");
    assert!(ranked[0].1 >= ranked[1].1);

    // Deterministic: same query, same order.
    let ranked2 = lib.retrieve("tag the alert events and summarize", 2);
    let ids: Vec<&str> = ranked.iter().map(|(s, _)| s.id.as_str()).collect();
    let ids2: Vec<&str> = ranked2.iter().map(|(s, _)| s.id.as_str()).collect();
    assert_eq!(ids, ids2);
}
