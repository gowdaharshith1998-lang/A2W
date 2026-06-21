//! M5 / F2 integration: search selects by fitness, certifies on a disjoint
//! holdout, and surfaces (never hides) any overfit gap. Every candidate it
//! generates is M1-valid by construction, and the search is deterministic.

use a2w_ir::{Connection, Node, NodeKind, Workflow, SCHEMA_VERSION};
use a2w_search::{evolve, InsertPassthrough, Mutation, SearchConfig, SetTransformField};
use a2w_verify::{
    GoldenFixture, MatchMode, SpecAssertion, VerificationHarness, VerificationPlan, WorkflowSpec,
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

/// `trigger -> total(Transform)`. The total node's `set` is supplied by caller;
/// an empty set means "no total field yet" (the broken seed).
fn total_workflow(id: &str, set: Value) -> Workflow {
    let trigger = Node::new("trigger", NodeKind::WebhookTrigger);
    let mut total = Node::new("total", NodeKind::Transform);
    total.params = json!({ "set": set });
    wf(
        id,
        vec![trigger, total],
        vec![Connection::new("trigger", 0, "total")],
    )
}

/// The seed: a workflow that does NOT compute `total` at all.
fn broken_seed() -> Workflow {
    total_workflow("wf_seed", json!({}))
}

/// Golden fixture: `{price, qty}` → `{price, qty, total: price*qty}`.
///
/// `total` is expected as a float because the expression engine evaluates
/// arithmetic in f64 (so `5 * 3` renders as `15.0`, a JSON float).
fn golden(name: &str, price: i64, qty: i64) -> GoldenFixture {
    GoldenFixture {
        name: name.to_string(),
        input: vec![json!({ "price": price, "qty": qty })],
        expected: vec![json!({ "price": price, "qty": qty, "total": (price * qty) as f64 })],
        match_mode: MatchMode::Exact,
    }
}

/// Operators whose vocabulary contains the CORRECT expression plus decoys.
fn rich_operators() -> Vec<Box<dyn Mutation>> {
    vec![
        Box::new(SetTransformField {
            vocabulary: vec![
                ("total".to_string(), json!("${{ $.price * $.qty }}")), // correct
                ("total".to_string(), json!(0)),                        // decoy constant
                ("noise".to_string(), json!("x")),                      // decoy field
            ],
            frozen: vec![],
        }),
        Box::new(InsertPassthrough),
    ]
}

#[tokio::test]
async fn search_improves_certified_holdout_score() {
    let harness = VerificationHarness::new();
    let seed = broken_seed();

    // Fitness and holdout are DISJOINT inputs, both encoding the real intent
    // (total = price*qty). The correct expression satisfies both.
    let fitness = VerificationPlan::new("total").with_golden(vec![golden("fit", 10, 2)]);
    let holdout = VerificationPlan::new("total").with_golden(vec![golden("hold", 5, 3)]);

    let outcome = evolve(
        &harness,
        &seed,
        &fitness,
        &holdout,
        &rich_operators(),
        SearchConfig::default(),
    )
    .await
    .expect("search");

    // The certified (holdout) score improved and is perfect.
    assert!(
        outcome.improved_on_holdout(),
        "certified score must improve: {} -> {}",
        outcome.initial_holdout_score,
        outcome.best_holdout_score
    );
    assert!(
        outcome.best_holdout_score >= 1.0,
        "certified holdout score perfect"
    );
    assert!(outcome.best_fitness_score >= 1.0);
    // Legit improvement: fitness gains are reflected in the holdout → no overfit.
    assert!(
        !outcome.overfit(),
        "overfit_gap should be ~0, got {}",
        outcome.overfit_gap
    );

    // The winner really computes price*qty.
    let out = harness
        .observe(
            &outcome.best_workflow,
            "total",
            vec![json!({ "price": 7, "qty": 6 })],
        )
        .await
        .unwrap();
    assert_eq!(out[0]["total"], json!(42.0)); // expression arithmetic is f64
    assert!(a2w_validator::validate(&outcome.best_workflow).is_valid);
}

#[tokio::test]
async fn search_overfit_is_surfaced_not_hidden() {
    // GOODHART setup: the fitness metric has a GAP — it only checks that a
    // `/total` field is PRESENT, not its value. The operator vocabulary can
    // only set a constant. The search can hit a perfect FITNESS score by
    // setting total=0, which is wrong. The holdout (exact golden) catches it.
    let harness = VerificationHarness::new();
    let seed = broken_seed();

    let fitness = VerificationPlan::new("total").with_spec(WorkflowSpec {
        input: vec![json!({ "price": 10, "qty": 2 })],
        assertions: vec![SpecAssertion::EveryItemHasField {
            path: "/total".to_string(), // GAP: presence only, not the value
        }],
    });
    let holdout = VerificationPlan::new("total").with_golden(vec![golden("hold", 5, 3)]);

    // Impoverished operator: can ONLY set total to a constant (cannot express
    // price*qty), so it can satisfy the gappy fitness but not the holdout.
    let ops: Vec<Box<dyn Mutation>> = vec![Box::new(SetTransformField {
        vocabulary: vec![("total".to_string(), json!(0))],
        frozen: vec![],
    })];

    let outcome = evolve(
        &harness,
        &seed,
        &fitness,
        &holdout,
        &ops,
        SearchConfig::default(),
    )
    .await
    .expect("search");

    // The search "won" on the metric it optimized...
    assert!(
        outcome.best_fitness_score >= 1.0,
        "fitness hit its (gappy) ceiling"
    );
    assert!(
        outcome.improved_on_fitness(),
        "fitness improved from the seed"
    );
    // ...but the CERTIFIED (holdout) score reveals it is not actually correct.
    assert!(
        outcome.best_holdout_score < 1.0,
        "holdout must reveal the gap; got {}",
        outcome.best_holdout_score
    );
    assert!(
        outcome.overfit(),
        "overfit_gap must be surfaced: {}",
        outcome.overfit_gap
    );
    // The honest report we'd hand to promotion is the holdout one.
    assert!(outcome.best_holdout_report.score() < 1.0);
    // And the certified metric did NOT actually improve — the gain was illusory.
    assert!(!outcome.improved_on_holdout());
}

#[tokio::test]
async fn every_generated_candidate_is_valid_by_construction() {
    let seed = broken_seed();
    let ops = rich_operators();
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
    let seed = broken_seed();
    let fitness = VerificationPlan::new("total").with_golden(vec![golden("fit", 10, 2)]);
    let holdout = VerificationPlan::new("total").with_golden(vec![golden("hold", 5, 3)]);

    let a = evolve(
        &harness,
        &seed,
        &fitness,
        &holdout,
        &rich_operators(),
        SearchConfig::default(),
    )
    .await
    .unwrap();
    let b = evolve(
        &harness,
        &seed,
        &fitness,
        &holdout,
        &rich_operators(),
        SearchConfig::default(),
    )
    .await
    .unwrap();

    assert_eq!(a.best_fitness_score, b.best_fitness_score);
    assert_eq!(a.best_holdout_score, b.best_holdout_score);
    assert_eq!(a.candidates_evaluated, b.candidates_evaluated);
    assert_eq!(
        serde_json::to_string(&a.best_workflow).unwrap(),
        serde_json::to_string(&b.best_workflow).unwrap()
    );
}

#[tokio::test]
async fn guard_rejects_fitness_holdout_sharing_inputs() {
    use a2w_search::{shared_evidence, SearchError};
    let harness = VerificationHarness::new();
    let seed = broken_seed();

    // Both plans use the SAME golden fixture (same input + expected) — a
    // correlated blind spot. The guard must reject before any search runs.
    let shared = golden("same", 10, 2);
    let fitness = VerificationPlan::new("total").with_golden(vec![shared.clone()]);
    let holdout = VerificationPlan::new("total").with_golden(vec![shared]);

    assert!(shared_evidence(&fitness, &holdout).is_some());
    let err = evolve(
        &harness,
        &seed,
        &fitness,
        &holdout,
        &rich_operators(),
        SearchConfig::default(),
    )
    .await
    .expect_err("shared evidence must be rejected");
    assert!(
        matches!(err, SearchError::CorrelatedEvidence(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn guard_rejects_identical_assertions_even_on_different_inputs() {
    use a2w_search::shared_evidence;
    // Different inputs, but the SAME spec assertion — still a correlated blind
    // spot (both plans are blind to whatever that assertion misses).
    let assertion = SpecAssertion::EveryItemHasField {
        path: "/total".to_string(),
    };
    let fitness = VerificationPlan::new("total").with_spec(WorkflowSpec {
        input: vec![json!({ "price": 1, "qty": 1 })],
        assertions: vec![assertion.clone()],
    });
    let holdout = VerificationPlan::new("total").with_spec(WorkflowSpec {
        input: vec![json!({ "price": 9, "qty": 9 })],
        assertions: vec![assertion],
    });
    assert!(
        shared_evidence(&fitness, &holdout).is_some(),
        "identical assertions must be flagged even with different inputs"
    );
}

#[tokio::test]
async fn disjoint_holdout_catches_a_fitness_gap() {
    // The correlated-blind-spot test: the fitness set has a GAP (presence only)
    // that a constant satisfies; the DISJOINT holdout (exact golden, different
    // input, different evidence kind) exercises the case the gap lets pass, so
    // the certified score reflects the real defect.
    let harness = VerificationHarness::new();
    let seed = broken_seed();

    let fitness = VerificationPlan::new("total").with_spec(WorkflowSpec {
        input: vec![json!({ "price": 10, "qty": 2 })],
        assertions: vec![SpecAssertion::EveryItemHasField {
            path: "/total".to_string(),
        }],
    });
    let holdout = VerificationPlan::new("total").with_golden(vec![golden("hold", 5, 3)]);
    // Genuinely disjoint: different inputs, different evidence kinds.
    assert!(a2w_search::shared_evidence(&fitness, &holdout).is_none());

    let ops: Vec<Box<dyn Mutation>> = vec![Box::new(SetTransformField {
        vocabulary: vec![("total".to_string(), json!(0))],
        frozen: vec![],
    })];
    let outcome = evolve(
        &harness,
        &seed,
        &fitness,
        &holdout,
        &ops,
        SearchConfig::default(),
    )
    .await
    .expect("search");
    // Fitness was satisfied; the holdout reveals the defect the gap hid.
    assert!(outcome.best_fitness_score >= 1.0);
    assert!(
        outcome.best_holdout_score < 1.0,
        "the disjoint holdout must catch the fitness gap"
    );
}

#[tokio::test]
async fn search_on_already_perfect_seed_does_not_regress() {
    let harness = VerificationHarness::new();
    // A seed that already computes total correctly.
    let good = total_workflow("wf_good", json!({ "total": "${{ $.price * $.qty }}" }));
    let fitness = VerificationPlan::new("total").with_golden(vec![golden("fit", 10, 2)]);
    let holdout = VerificationPlan::new("total").with_golden(vec![golden("hold", 5, 3)]);

    let outcome = evolve(
        &harness,
        &good,
        &fitness,
        &holdout,
        &rich_operators(),
        SearchConfig::default(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.initial_fitness_score, 1.0);
    assert_eq!(outcome.best_holdout_score, 1.0);
    assert!(
        !outcome.improved_on_holdout(),
        "nothing to improve on a perfect seed"
    );
    assert!(!outcome.overfit());
}
