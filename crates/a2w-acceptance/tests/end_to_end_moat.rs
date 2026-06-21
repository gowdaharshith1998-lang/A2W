//! Whole-program acceptance: the M0→M5 spine composes end-to-end.
//!
//! A non-trivial **branching + transform** workflow is:
//! 1. statically validated (M1),
//! 2. run deterministically and zero-token (engine),
//! 3. scored into a calibrated confidence report (M3),
//! 4. promoted into the skill library iff above threshold (M4), then
//! 5. a search pass improves a deliberately-broken seed's score (M5).

use a2w_ir::{Connection, Node, NodeKind, Workflow, SCHEMA_VERSION};
use a2w_search::{evolve, InsertPassthrough, Mutation, SearchConfig, SetTransformField};
use a2w_skills::SkillLibrary;
use a2w_verify::{
    verify, CountOp, GoldenFixture, MatchMode, MetamorphicSuite, SemanticRelation, SemanticSuite,
    SpecAssertion, VerificationHarness, VerificationPlan, WorkflowSpec,
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

/// A non-trivial branching + transform workflow:
/// `trigger -> classify(Branch on /priority=="high") -> escalate(Transform) `,
/// with the low-priority arm going to `note(Transform)`. We observe the
/// `escalate` node (high-priority items, tagged escalated).
fn alert_router() -> Workflow {
    let trigger = Node::new("trigger", NodeKind::WebhookTrigger);
    let mut classify = Node::new("classify", NodeKind::Branch);
    classify.params = json!({
        "condition": { "path": "/priority", "op": "eq", "value": "high" }
    });
    let mut escalate = Node::new("escalate", NodeKind::Transform);
    escalate.params = json!({ "set": { "escalated": true } });
    let mut note = Node::new("note", NodeKind::Transform);
    note.params = json!({ "set": { "noted": true } });
    wf(
        "wf_alert_router",
        vec![trigger, classify, escalate, note],
        vec![
            Connection::new("trigger", 0, "classify"),
            Connection::new("classify", 0, "escalate"), // true -> escalate
            Connection::new("classify", 1, "note"),     // false -> note
        ],
    )
}

fn alerts(n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| {
            json!({
                "id": i,
                "priority": if i % 3 == 0 { "high" } else { "low" }
            })
        })
        .collect()
}

#[tokio::test]
async fn whole_program_validate_run_verify_promote_search() {
    // -- M1: static validity ------------------------------------------------
    let router = alert_router();
    let report = a2w_validator::validate(&router);
    assert!(
        report.is_valid,
        "router should be valid: {:?}",
        report.findings
    );

    // -- engine: deterministic, zero-token run ------------------------------
    let harness = VerificationHarness::new(); // DryRun, no network, no LLM
    let escalated = harness
        .observe(&router, "escalate", alerts(9))
        .await
        .expect("run");
    // ids 0,3,6 are high → 3 escalated items, all tagged.
    assert_eq!(escalated.len(), 3);
    assert!(escalated.iter().all(|i| i["escalated"] == json!(true)));

    // -- M3: calibrated confidence report -----------------------------------
    let plan = VerificationPlan::new("escalate")
        .with_spec(WorkflowSpec {
            input: alerts(9),
            assertions: vec![
                SpecAssertion::OutputCount {
                    op: CountOp::Eq,
                    count: 3,
                },
                SpecAssertion::EveryItemFieldEquals {
                    path: "/escalated".to_string(),
                    value: json!(true),
                },
                SpecAssertion::NoItemFieldEquals {
                    path: "/priority".to_string(),
                    value: json!("low"),
                },
            ],
        })
        // Outcome evidence: appending one more high-priority alert escalates
        // exactly one more item (the router's intent, authored independently).
        .with_semantic(SemanticSuite::new(vec![
            SemanticRelation::AppendAddsOutputs {
                base_input: alerts(9),
                passing_extra: vec![json!({ "id": 1000, "priority": "high" })],
                per_item: 1,
            },
        ]))
        .with_metamorphic(MetamorphicSuite::standard(alerts(9)));
    let confidence = verify(&harness, &router, &plan).await.expect("verify");
    assert_eq!(
        confidence.score(),
        1.0,
        "router is correct:\n{}",
        confidence.summary()
    );
    // The report cites real OUTCOME evidence (spec + semantic) AND separately
    // holds engine-invariants — which are NOT outcome evidence.
    assert!(confidence.passed_in(a2w_verify::CheckCategory::Spec) >= 3);
    assert!(confidence.passed_in(a2w_verify::CheckCategory::SemanticRelation) >= 1);
    assert!(confidence.passed_in(a2w_verify::CheckCategory::EngineInvariant) >= 3);

    // -- M4: promotion gated on the M3 signal -------------------------------
    let mut lib = SkillLibrary::with_default_threshold();
    let skill_id = lib
        .promote(
            "route high-priority alerts to escalation",
            router.clone(),
            "escalate",
            &confidence,
        )
        .expect("verified router is promoted");
    assert_eq!(lib.len(), 1);

    // Retrieve it for a similar query.
    let (skill, sim) = lib
        .best_match("escalate the high priority incoming alerts")
        .expect("retrieval");
    assert_eq!(skill.id, skill_id);
    assert!(sim > 0.0);

    // -- M5: search improves a deliberately-broken seed ---------------------
    // Seed = the router with the escalate Transform NOT tagging escalated, so
    // the outcome evidence fails.
    let mut broken = alert_router();
    broken.id = "wf_alert_router_broken".into();
    for node in &mut broken.nodes {
        if node.id == "escalate" {
            node.params = json!({ "set": {} }); // passthrough: no `escalated` tag
        }
    }
    // Fitness and holdout are EVIDENCE-DISJOINT (F3): different inputs, AND
    // different evidence kinds. Fitness = spec assertion + semantic relation on
    // alerts(9); holdout = an exact golden fixture on a separate input. The
    // disjointness guard inside evolve() enforces this.
    let fitness_plan = VerificationPlan::new("escalate")
        .with_spec(WorkflowSpec {
            input: alerts(9),
            assertions: vec![SpecAssertion::EveryItemFieldEquals {
                path: "/escalated".to_string(),
                value: json!(true),
            }],
        })
        .with_semantic(SemanticSuite::new(vec![
            SemanticRelation::AppendAddsOutputs {
                base_input: alerts(9),
                passing_extra: vec![json!({ "id": 9001, "priority": "high" })],
                per_item: 1,
            },
        ]));
    let holdout_input = vec![
        json!({ "id": 200, "priority": "high" }),
        json!({ "id": 201, "priority": "low" }),
    ];
    let holdout_plan = VerificationPlan::new("escalate")
        .with_golden(vec![GoldenFixture {
            name: "escalation".to_string(),
            input: holdout_input.clone(),
            // Only the high-priority alert reaches `escalate`, tagged escalated.
            expected: vec![json!({ "id": 200, "priority": "high", "escalated": true })],
            match_mode: MatchMode::Exact,
        }])
        // The holdout carries its OWN semantic relation (different input than
        // the fitness plan's) so it both certifies and clears the threshold.
        .with_semantic(SemanticSuite::new(vec![
            SemanticRelation::AppendAddsOutputs {
                base_input: holdout_input,
                passing_extra: vec![json!({ "id": 202, "priority": "high" })],
                per_item: 1,
            },
        ]));

    let seed_holdout = verify(&harness, &broken, &holdout_plan)
        .await
        .unwrap()
        .score();
    assert!(
        seed_holdout < 1.0,
        "broken seed must be imperfect on the holdout"
    );

    let ops: Vec<Box<dyn Mutation>> = vec![
        Box::new(SetTransformField {
            vocabulary: vec![
                ("escalated".to_string(), json!(true)),
                ("escalated".to_string(), json!(false)),
                ("decoy".to_string(), json!("z")),
            ],
            frozen: vec![],
        }),
        Box::new(InsertPassthrough),
    ];
    let outcome = evolve(
        &harness,
        &broken,
        &fitness_plan,
        &holdout_plan,
        &ops,
        SearchConfig::default(),
    )
    .await
    .expect("search");
    // Certified (holdout) improvement — not just fitness.
    assert!(
        outcome.improved_on_holdout() && outcome.best_holdout_score >= 1.0,
        "search must improve the CERTIFIED score {} -> {}",
        outcome.initial_holdout_score,
        outcome.best_holdout_score
    );
    assert!(
        !outcome.overfit(),
        "legit improvement: no overfit gap ({})",
        outcome.overfit_gap
    );

    // The repaired workflow is valid and itself promotable — gated on the
    // HOLDOUT report (never the fitness report).
    assert!(a2w_validator::validate(&outcome.best_workflow).is_valid);
    let certified = outcome.best_holdout_report.clone();
    assert!(certified.meets(lib.threshold()));
    lib.promote(
        "route high-priority alerts to escalation (evolved)",
        outcome.best_workflow,
        "escalate",
        &certified,
    )
    .expect("evolved workflow promotes on its holdout report");
    assert_eq!(
        lib.len(),
        2,
        "both the original and evolved skills are stored"
    );
}
