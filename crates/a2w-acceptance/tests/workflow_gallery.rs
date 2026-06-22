//! Example-gallery acceptance suite.
//!
//! Loads every committed workflow in `examples/`, then for each one:
//!   1. **validates** it (M1 — reject-before-execute),
//!   2. **runs** it deterministically and zero-token (DryRun; side-effecting
//!      nodes are mocked), and
//!   3. **verifies** it with a calibrated confidence report (M3) whose
//!      OUTCOME evidence (spec assertions + a spec-derived semantic relation +
//!      golden fixtures) clears the default threshold, with engine-invariants
//!      reported separately.
//!
//! It then demonstrates the compounding loop: **promote** a verified workflow
//! into the skill library (M4) and **evolve** a deliberately-broken one,
//! certifying the winner on a disjoint holdout (M5).
//!
//! Run with output: `cargo test -p a2w-acceptance --test workflow_gallery -- --nocapture`

use a2w_engine::{Engine, ExecutionMode, MemoryEventLog, RunStatus};
use a2w_ir::Workflow;
use a2w_search::{evolve, InsertPassthrough, Mutation, SearchConfig, SetTransformField};
use a2w_skills::SkillLibrary;
use a2w_validator::validate;
use a2w_verify::{
    verify, CheckCategory, CountOp, GoldenFixture, MatchMode, MetamorphicSuite, SemanticRelation,
    SemanticSuite, SpecAssertion, Threshold, VerificationHarness, VerificationPlan, WorkflowSpec,
};
use serde_json::{json, Value};

// ---- the committed examples (compiled in, so the gallery is hermetic) ----
const ORDER_PRICING: &str = include_str!("../../../examples/order_pricing.json");
const ALERT_ROUTER: &str = include_str!("../../../examples/alert_router.json");
const SEVERITY_SWITCH: &str = include_str!("../../../examples/severity_switch.json");
const ORDER_ITEMS_LOOP: &str = include_str!("../../../examples/order_items_loop.json");
const ENRICH_MERGE: &str = include_str!("../../../examples/enrich_merge.json");
const HTTP_FETCH_SHAPE: &str = include_str!("../../../examples/http_fetch_shape.json");
const DEEP_PIPELINE: &str = include_str!("../../../examples/deep_pipeline.json");

/// Parse + statically validate an example, returning the workflow.
fn load(json: &str) -> Workflow {
    let wf = Workflow::from_json(json).expect("example is valid JSON IR");
    let report = validate(&wf);
    assert!(
        report.is_valid,
        "example '{}' failed validation: {:?}",
        wf.id, report.findings
    );
    wf
}

/// DryRun the workflow (zero-token) and return (status, observed item count).
async fn dry_observe(wf: &Workflow, observe: &str, input: Vec<Value>) -> (RunStatus, usize) {
    let engine = Engine::new(a2w_nodes::default_registry());
    let log = MemoryEventLog::new();
    let run = engine
        .run(wf, input, ExecutionMode::DryRun, &log)
        .await
        .expect("dry run completes");
    let n = run.node_outputs.get(observe).map(Vec::len).unwrap_or(0);
    (run.status, n)
}

/// DryRun and return (run status, the observed node's full output payloads).
async fn run_node(wf: &Workflow, observe: &str, input: Vec<Value>) -> (RunStatus, Vec<Value>) {
    let engine = Engine::new(a2w_nodes::default_registry());
    let log = MemoryEventLog::new();
    let run = engine
        .run(wf, input, ExecutionMode::DryRun, &log)
        .await
        .expect("dry run completes");
    let out = run
        .node_outputs
        .get(observe)
        .map(|items| items.iter().map(|i| i.json.clone()).collect())
        .unwrap_or_default();
    (run.status, out)
}

/// One gallery entry: a workflow + the node to observe + its verification plan
/// + a representative input used for the run-summary line.
struct Case {
    name: &'static str,
    wf: Workflow,
    observe: &'static str,
    plan: VerificationPlan,
    run_input: Vec<Value>,
}

fn gallery() -> Vec<Case> {
    let mut cases = Vec::new();

    // 1) order pricing — expression arithmetic, verified by a scaling relation.
    {
        let input = vec![
            json!({ "price": 10, "qty": 2 }),
            json!({ "price": 5, "qty": 3 }),
            json!({ "price": 8, "qty": 1 }),
        ];
        cases.push(Case {
            name: "order_pricing",
            wf: load(ORDER_PRICING),
            observe: "price",
            run_input: input.clone(),
            plan: VerificationPlan::new("price")
                .with_spec(WorkflowSpec {
                    input: input.clone(),
                    assertions: vec![
                        SpecAssertion::OutputCount {
                            op: CountOp::Eq,
                            count: 3,
                        },
                        SpecAssertion::EveryItemHasField {
                            path: "/total".into(),
                        },
                    ],
                })
                .with_semantic(SemanticSuite::new(vec![SemanticRelation::FieldScaling {
                    in_field: "/price".into(),
                    out_field: "/total".into(),
                    factor: 2.0,
                    base_input: input,
                }]))
                .with_golden(vec![GoldenFixture {
                    name: "single_line".into(),
                    input: vec![json!({ "price": 4, "qty": 5 })],
                    expected: vec![json!({ "price": 4, "qty": 5, "total": 20.0 })],
                    match_mode: MatchMode::Exact,
                }])
                .with_metamorphic(MetamorphicSuite::standard(vec![
                    json!({ "price": 2, "qty": 2 }),
                    json!({ "price": 3, "qty": 4 }),
                ])),
        });
    }

    // 2) alert router — branch routing, verified by a spec + append relation.
    {
        let input = vec![
            json!({ "id": 0, "priority": "high" }),
            json!({ "id": 1, "priority": "low" }),
            json!({ "id": 2, "priority": "high" }),
            json!({ "id": 3, "priority": "medium" }),
        ];
        cases.push(Case {
            name: "alert_router",
            wf: load(ALERT_ROUTER),
            observe: "escalate",
            run_input: input.clone(),
            plan: VerificationPlan::new("escalate")
                .with_spec(WorkflowSpec {
                    input: input.clone(),
                    assertions: vec![
                        SpecAssertion::EveryItemFieldEquals {
                            path: "/escalated".into(),
                            value: json!(true),
                        },
                        SpecAssertion::NoItemFieldEquals {
                            path: "/priority".into(),
                            value: json!("low"),
                        },
                    ],
                })
                .with_semantic(SemanticSuite::new(vec![
                    SemanticRelation::AppendAddsOutputs {
                        base_input: input,
                        passing_extra: vec![json!({ "id": 99, "priority": "high" })],
                        per_item: 1,
                    },
                ]))
                .with_metamorphic(MetamorphicSuite::standard(vec![
                    json!({ "id": 0, "priority": "high" }),
                    json!({ "id": 1, "priority": "low" }),
                ])),
        });
    }

    // 3) severity switch — multi-way routing; observe the "critical" port.
    {
        let input = vec![
            json!({ "severity": "critical" }),
            json!({ "severity": "warning" }),
            json!({ "severity": "info" }),
            json!({ "severity": "critical" }),
            json!({ "severity": "nope" }),
        ];
        cases.push(Case {
            name: "severity_switch",
            wf: load(SEVERITY_SWITCH),
            observe: "page",
            run_input: input.clone(),
            plan: VerificationPlan::new("page")
                .with_spec(WorkflowSpec {
                    input: input.clone(),
                    assertions: vec![
                        SpecAssertion::OutputCount {
                            op: CountOp::Eq,
                            count: 2,
                        },
                        SpecAssertion::EveryItemFieldEquals {
                            path: "/routed".into(),
                            value: json!("critical"),
                        },
                    ],
                })
                .with_semantic(SemanticSuite::new(vec![
                    SemanticRelation::AppendAddsOutputs {
                        base_input: input,
                        passing_extra: vec![json!({ "severity": "critical" })],
                        per_item: 1,
                    },
                ]))
                .with_metamorphic(MetamorphicSuite::standard(vec![
                    json!({ "severity": "critical" }),
                    json!({ "severity": "info" }),
                ])),
        });
    }

    // 4) order items loop — array fan-out; observe the per-element body.
    {
        let input = vec![
            json!({ "order": 1, "items": [{ "k": 1 }, { "k": 2 }] }),
            json!({ "order": 2, "items": [{ "k": 3 }] }),
        ];
        cases.push(Case {
            name: "order_items_loop",
            wf: load(ORDER_ITEMS_LOOP),
            observe: "process",
            run_input: input.clone(),
            plan: VerificationPlan::new("process")
                .with_spec(WorkflowSpec {
                    input: input.clone(),
                    assertions: vec![
                        SpecAssertion::OutputCount {
                            op: CountOp::Eq,
                            count: 3,
                        },
                        SpecAssertion::EveryItemFieldEquals {
                            path: "/processed".into(),
                            value: json!(true),
                        },
                    ],
                })
                .with_semantic(SemanticSuite::new(vec![
                    SemanticRelation::AppendAddsOutputs {
                        base_input: input,
                        // appending one parent carrying exactly one line item adds one body output.
                        passing_extra: vec![json!({ "order": 9, "items": [{ "k": 9 }] })],
                        per_item: 1,
                    },
                ]))
                .with_metamorphic(MetamorphicSuite::standard(vec![
                    json!({ "order": 1, "items": [{ "k": 1 }] }),
                    json!({ "order": 2, "items": [{ "k": 2 }] }),
                ])),
        });
    }

    // 5) enrich + merge — concurrent diamond; observe the fan-in.
    {
        let input = vec![json!({ "id": 0 }), json!({ "id": 1 })];
        cases.push(Case {
            name: "enrich_merge",
            wf: load(ENRICH_MERGE),
            observe: "merge",
            run_input: input.clone(),
            plan: VerificationPlan::new("merge")
                .with_spec(WorkflowSpec {
                    input: input.clone(),
                    assertions: vec![
                        // two branches × two items → four merged items.
                        SpecAssertion::OutputCount {
                            op: CountOp::Eq,
                            count: 4,
                        },
                        SpecAssertion::SomeItemFieldEquals {
                            path: "/region".into(),
                            value: json!("us-east"),
                        },
                        SpecAssertion::SomeItemFieldEquals {
                            path: "/tier".into(),
                            value: json!("gold"),
                        },
                    ],
                })
                .with_semantic(SemanticSuite::new(vec![
                    SemanticRelation::AppendAddsOutputs {
                        base_input: input,
                        // one trigger item fans into two merged outputs (region + tier).
                        passing_extra: vec![json!({ "id": 2 })],
                        per_item: 2,
                    },
                ]))
                .with_metamorphic(MetamorphicSuite::standard(vec![
                    json!({ "id": 0 }),
                    json!({ "id": 1 }),
                ])),
        });
    }

    // 6) http fetch + shape — a side-effecting node, mocked in dry-run.
    {
        let input = vec![json!({ "q": "a" }), json!({ "q": "b" })];
        cases.push(Case {
            name: "http_fetch_shape",
            wf: load(HTTP_FETCH_SHAPE),
            observe: "shape",
            run_input: input.clone(),
            plan: VerificationPlan::new("shape")
                .with_spec(WorkflowSpec {
                    input: input.clone(),
                    assertions: vec![
                        SpecAssertion::OutputCount {
                            op: CountOp::Eq,
                            count: 2,
                        },
                        SpecAssertion::EveryItemFieldEquals {
                            path: "/shaped".into(),
                            value: json!(true),
                        },
                    ],
                })
                .with_semantic(SemanticSuite::new(vec![
                    SemanticRelation::CountConservation { input },
                ]))
                .with_metamorphic(MetamorphicSuite::standard(vec![
                    json!({ "q": "x" }),
                    json!({ "q": "y" }),
                ])),
        });
    }

    // 7) deep pipeline — staged transforms; count-conservation.
    {
        let input = vec![json!({ "id": 0 }), json!({ "id": 1 }), json!({ "id": 2 })];
        cases.push(Case {
            name: "deep_pipeline",
            wf: load(DEEP_PIPELINE),
            observe: "finalize",
            run_input: input.clone(),
            plan: VerificationPlan::new("finalize")
                .with_spec(WorkflowSpec {
                    input: input.clone(),
                    assertions: vec![
                        SpecAssertion::EveryItemFieldEquals {
                            path: "/ready".into(),
                            value: json!(true),
                        },
                        SpecAssertion::EveryItemHasField {
                            path: "/source".into(),
                        },
                    ],
                })
                .with_semantic(SemanticSuite::new(vec![
                    SemanticRelation::CountConservation { input },
                ]))
                .with_metamorphic(MetamorphicSuite::standard(vec![
                    json!({ "id": 0 }),
                    json!({ "id": 1 }),
                ])),
        });
    }

    cases
}

#[tokio::test]
async fn gallery_validates_runs_and_verifies_every_example() {
    let harness = VerificationHarness::new();
    let cases = gallery();
    assert_eq!(cases.len(), 7, "all seven examples present");

    println!(
        "\n  {:<18} {:<10} {:>5}  {:>8}  {:>9}  verdict",
        "workflow", "run", "items", "outcome", "engine"
    );
    println!("  {}", "-".repeat(74));

    for case in &cases {
        // run (deterministic, zero-token)
        let (status, items) = dry_observe(&case.wf, case.observe, case.run_input.clone()).await;
        assert_eq!(
            status,
            RunStatus::Completed,
            "{} should complete",
            case.name
        );

        // verify (calibrated confidence report)
        let report = verify(&harness, &case.wf, &case.plan)
            .await
            .unwrap_or_else(|e| panic!("{} verify errored: {e}", case.name));

        let outcome = report.score();
        let eng = format!(
            "{}/{}",
            report.passed_in(CheckCategory::EngineInvariant),
            report.count_in(CheckCategory::EngineInvariant)
        );
        let verdict = if report.meets(&Threshold::default()) {
            "OUTCOME VERIFIED"
        } else {
            "unverified"
        };
        println!(
            "  {:<18} {:<10} {:>5}  {:>8.2}  {:>9}  {}",
            case.name,
            format!("{status:?}"),
            items,
            outcome,
            eng,
            verdict
        );

        assert_eq!(
            outcome,
            1.0,
            "{} outcome score must be perfect:\n{}",
            case.name,
            report.summary()
        );
        assert!(
            report.meets(&Threshold::default()),
            "{} must clear the threshold:\n{}",
            case.name,
            report.summary()
        );
        // Engine guarantees held too (reported separately, not counted as outcome).
        assert!(
            report.engine_invariants_held(),
            "{} engine-invariants must hold:\n{}",
            case.name,
            report.summary()
        );
    }
    println!();
}

/// Dump every node's *actual* output payloads for every example, and assert the
/// run is byte-identical across reruns (determinism). This is the "show me the
/// real results" companion to the verified-by-plan gallery above.
#[tokio::test]
async fn gallery_dumps_actual_node_outputs_and_is_deterministic() {
    use std::collections::BTreeMap;
    for case in &gallery() {
        let engine = Engine::new(a2w_nodes::default_registry());
        let log = MemoryEventLog::new();
        let run = engine
            .run(
                &case.wf,
                case.run_input.clone(),
                ExecutionMode::DryRun,
                &log,
            )
            .await
            .expect("run");
        // Re-run: the engine is deterministic by construction; prove it.
        let log2 = MemoryEventLog::new();
        let run2 = engine
            .run(
                &case.wf,
                case.run_input.clone(),
                ExecutionMode::DryRun,
                &log2,
            )
            .await
            .expect("rerun");

        println!("\n=== {} (id {}) ===", case.name, case.wf.id);
        println!(
            "input  : {}",
            serde_json::to_string(&case.run_input).unwrap()
        );
        let ordered: BTreeMap<&String, &Vec<a2w_engine::Item>> = run.node_outputs.iter().collect();
        for (nid, items) in &ordered {
            let payloads: Vec<&Value> = items.iter().map(|i| &i.json).collect();
            println!(
                "  {:<10} [{:>2}] {}",
                nid,
                items.len(),
                serde_json::to_string(&payloads).unwrap()
            );
        }

        let a = serde_json::to_value(&run.node_outputs).unwrap();
        let b = serde_json::to_value(&run2.node_outputs).unwrap();
        assert_eq!(a, b, "{} must be byte-identical across reruns", case.name);

        // Lock in the EXACT computed payloads of the observed node — this is the
        // "produces the right result" regression guard (not just a count/verdict).
        let actual: Vec<Value> = run
            .node_outputs
            .get(case.observe)
            .map(|items| items.iter().map(|i| i.json.clone()).collect())
            .unwrap_or_default();
        let expected = expected_observed_output(case.name);
        assert_eq!(
            actual, expected,
            "{}: observed node '{}' produced the wrong payloads",
            case.name, case.observe
        );
    }
    println!();
}

/// The exact, hand-verified output payloads of each example's observed node,
/// for the gallery's representative input. Asserting on these makes "the
/// workflows produce the right results" a hard, byte-level regression guard.
fn expected_observed_output(name: &str) -> Vec<Value> {
    match name {
        // 10×2=20, 5×3=15, 8×1=8 (expression arithmetic renders as f64).
        "order_pricing" => vec![
            json!({ "price": 10, "qty": 2, "total": 20.0 }),
            json!({ "price": 5, "qty": 3, "total": 15.0 }),
            json!({ "price": 8, "qty": 1, "total": 8.0 }),
        ],
        // branch port 0 = the two "high" items, tagged escalated.
        "alert_router" => vec![
            json!({ "id": 0, "priority": "high", "escalated": true }),
            json!({ "id": 2, "priority": "high", "escalated": true }),
        ],
        // switch port 0 = the two "critical" items, tagged routed=critical.
        "severity_switch" => vec![
            json!({ "severity": "critical", "routed": "critical" }),
            json!({ "severity": "critical", "routed": "critical" }),
        ],
        // loop body fans out 2+1 = 3 line items, each tagged processed.
        "order_items_loop" => vec![
            json!({ "index": 0, "parent": { "items": [{ "k": 1 }, { "k": 2 }], "order": 1 }, "value": { "k": 1 }, "processed": true }),
            json!({ "index": 1, "parent": { "items": [{ "k": 1 }, { "k": 2 }], "order": 1 }, "value": { "k": 2 }, "processed": true }),
            json!({ "index": 0, "parent": { "items": [{ "k": 3 }], "order": 2 }, "value": { "k": 3 }, "processed": true }),
        ],
        // concurrent diamond: region's 2 items then tier's 2 items.
        "enrich_merge" => vec![
            json!({ "id": 0, "region": "us-east" }),
            json!({ "id": 1, "region": "us-east" }),
            json!({ "id": 0, "tier": "gold" }),
            json!({ "id": 1, "tier": "gold" }),
        ],
        // http mocked deterministically (zero-token), then shaped.
        "http_fetch_shape" => vec![
            json!({ "_mock": true, "status": 200, "url": "https://example.com/api/items", "shaped": true }),
            json!({ "_mock": true, "status": 200, "url": "https://example.com/api/items", "shaped": true }),
        ],
        // staged transforms accumulate all three flags, count conserved.
        "deep_pipeline" => vec![
            json!({ "id": 0, "normalized": true, "source": "a2w", "ready": true }),
            json!({ "id": 1, "normalized": true, "source": "a2w", "ready": true }),
            json!({ "id": 2, "normalized": true, "source": "a2w", "ready": true }),
        ],
        other => panic!("no expected output registered for example '{other}'"),
    }
}

/// Empirically confirm the *edge-case* behavior an adversarial audit predicted
/// from the executor source: every example still completes (no panic, no
/// whole-run error) and routes/coerces per the engine's documented contract,
/// producing a defensible — never silently wrong — result on hostile input.
#[tokio::test]
async fn gallery_edge_cases_behave_per_contract() {
    // -- order_pricing: missing field and 0 coalesce to 0.0; bad string surfaces
    //    visibly as a non-number rather than corrupting a number. --
    let wf = load(ORDER_PRICING);
    let (s, out) = run_node(&wf, "price", vec![json!({ "qty": 3 })]).await; // price absent
    assert_eq!(s, RunStatus::Completed);
    assert_eq!(
        out,
        vec![json!({ "qty": 3, "total": 0.0 })],
        "missing price → 0.0"
    );

    let (_, out) = run_node(&wf, "price", vec![json!({ "price": 8, "qty": 0 })]).await;
    assert_eq!(
        out,
        vec![json!({ "price": 8, "qty": 0, "total": 0.0 })],
        "qty 0 → 0.0"
    );

    // A non-numeric price now FAILS LOUDLY (strict whole-expression eval)
    // instead of silently poisoning `total` with a "${{!...!}}" marker string.
    // Under the default Stop policy the engine aborts the run rather than
    // reporting a bogus "completed".
    let bad = vec![json!({ "price": "abc", "qty": 2 })];
    let engine = Engine::new(a2w_nodes::default_registry());
    let log = MemoryEventLog::new();
    let res = engine
        .run(&wf, bad.clone(), ExecutionMode::DryRun, &log)
        .await;
    assert!(
        res.is_err(),
        "a non-numeric price aborts the run (strict eval), not a poisoned total: {res:?}"
    );
    // ...and with on_error: Continue the failure is *contained* by policy: the
    // price node yields zero items and the run completes.
    let mut lenient = wf.clone();
    for n in &mut lenient.nodes {
        if n.id == "price" {
            n.on_error = Some(a2w_ir::ErrorPolicy::Continue);
        }
    }
    let (s, out) = run_node(&lenient, "price", bad).await;
    assert_eq!(
        s,
        RunStatus::Completed,
        "on_error:Continue contains the failure"
    );
    assert!(
        out.is_empty(),
        "the failed node produced zero items under Continue"
    );

    // -- alert_router: anything that is not exactly "high" routes to `note`,
    //    including a case near-miss; the high port stays empty. --
    let wf = load(ALERT_ROUTER);
    for input in [json!({ "message": "x" }), json!({ "priority": "High" })] {
        let (s, esc) = run_node(&wf, "escalate", vec![input.clone()]).await;
        assert_eq!(s, RunStatus::Completed);
        assert!(esc.is_empty(), "non-'high' must not escalate: {input}");
        let (_, note) = run_node(&wf, "note", vec![input.clone()]).await;
        assert_eq!(note.len(), 1, "it routes to note instead: {input}");
        assert_eq!(note[0]["noted"], json!(true));
    }

    // -- severity_switch: an unmatched value and a missing key both fall to the
    //    default port (discard); nothing reaches `page`; every item routes once. --
    let wf = load(SEVERITY_SWITCH);
    for input in [json!({ "severity": "nope" }), json!({ "other": 1 })] {
        let (s, page) = run_node(&wf, "page", vec![input.clone()]).await;
        assert_eq!(s, RunStatus::Completed);
        assert!(page.is_empty(), "unmatched must not page: {input}");
        let (_, disc) = run_node(&wf, "discard", vec![input.clone()]).await;
        assert_eq!(disc.len(), 1, "it lands in discard (default port): {input}");
        assert_eq!(disc[0]["routed"], json!("default"));
    }

    // -- order_items_loop: the two "empty-ish" inputs have DISTINCT, documented
    //    contracts (the Audit-2 fix in loop_node.rs). A real array — even an
    //    empty one — iterates and emits a count summary. A MISSING /items is
    //    not an array, so the item passes through the body port (data is never
    //    silently dropped) and NO summary is emitted. Neither path panics. --
    let wf = load(ORDER_ITEMS_LOOP);
    // (a) empty array → zero body items, one count-0 summary.
    let (s, body) = run_node(&wf, "process", vec![json!({ "order": 5, "items": [] })]).await;
    assert_eq!(s, RunStatus::Completed, "empty array must not crash");
    assert!(body.is_empty(), "empty array → no line items");
    let (_, summ) = run_node(&wf, "summary", vec![json!({ "order": 5, "items": [] })]).await;
    assert_eq!(summ.len(), 1, "empty array still emits a done-summary");
    assert_eq!(summ[0]["count"], json!(0), "with count 0");
    // (b) missing /items → not an array: the original item passes through the
    //     body port unchanged (then `process` tags it), and NO summary fires.
    let (s, body) = run_node(&wf, "process", vec![json!({ "order": 5 })]).await;
    assert_eq!(s, RunStatus::Completed, "missing /items must not crash");
    assert_eq!(
        body,
        vec![json!({ "order": 5, "processed": true })],
        "missing /items passes the item through the body port (no data dropped)"
    );
    let (_, summ) = run_node(&wf, "summary", vec![json!({ "order": 5 })]).await;
    assert!(summ.is_empty(), "the non-iterating path emits no summary");

    // -- enrich_merge: empty input completes with an empty merge (no items lost,
    //    none fabricated). --
    let wf = load(ENRICH_MERGE);
    let (s, merged) = run_node(&wf, "merge", vec![]).await;
    assert_eq!(s, RunStatus::Completed);
    assert!(merged.is_empty(), "empty in → empty out");

    // -- deep_pipeline: a pre-existing conflicting key is overwritten by the
    //    stage that sets it (last-writer-wins), not duplicated or ignored. --
    let wf = load(DEEP_PIPELINE);
    let (_, fin) = run_node(&wf, "finalize", vec![json!({ "id": 0, "ready": false })]).await;
    assert_eq!(
        fin,
        vec![json!({ "id": 0, "normalized": true, "source": "a2w", "ready": true })],
        "the finalize stage overwrites a conflicting ready:false → true"
    );

    println!(
        "\n  all 7 examples behave per-contract on hostile input (no panic, no silent-wrong)\n"
    );
}

#[tokio::test]
async fn promote_a_verified_example_into_the_skill_library() {
    let harness = VerificationHarness::new();
    let wf = load(ORDER_PRICING);
    let input = vec![
        json!({ "price": 6, "qty": 7 }),
        json!({ "price": 2, "qty": 9 }),
    ];
    let plan = VerificationPlan::new("price")
        .with_spec(WorkflowSpec {
            input: input.clone(),
            assertions: vec![SpecAssertion::EveryItemHasField {
                path: "/total".into(),
            }],
        })
        .with_semantic(SemanticSuite::new(vec![SemanticRelation::FieldScaling {
            in_field: "/price".into(),
            out_field: "/total".into(),
            factor: 3.0,
            base_input: input,
        }]));
    let report = verify(&harness, &wf, &plan).await.expect("verify");

    let mut lib = SkillLibrary::with_default_threshold();
    let id = lib
        .promote(
            "compute order line totals from price and quantity",
            wf,
            "price",
            &report,
        )
        .expect("a verified workflow is promotable");

    // Retrieved for a similar query.
    let (skill, sim) = lib
        .best_match("calculate the total cost of each order line")
        .expect("a match exists");
    assert_eq!(skill.id, id);
    assert!(sim > 0.0, "similarity should be positive: {sim}");
    assert_eq!(skill.evidence.score, 1.0);
    println!(
        "\n  promoted '{}' (similarity {:.2} for a paraphrased query)\n",
        skill.id, sim
    );
}

#[tokio::test]
async fn evolve_a_broken_pricing_workflow_certified_on_a_holdout() {
    let harness = VerificationHarness::new();

    // Break the pricing workflow: derive `total` from the WRONG field (`cost`).
    let mut broken = load(ORDER_PRICING);
    broken.id = "wf_order_pricing_broken".into();
    for n in &mut broken.nodes {
        if n.id == "price" {
            n.params = json!({ "set": { "total": "${{ $.cost * $.qty }}" } });
        }
    }

    // Golden fixtures expect total = price × qty (rendered as a JSON float).
    let golden = |name: &str, price: i64, qty: i64, cost: i64| GoldenFixture {
        name: name.into(),
        input: vec![json!({ "price": price, "qty": qty, "cost": cost })],
        expected: vec![
            json!({ "price": price, "qty": qty, "cost": cost, "total": (price * qty) as f64 }),
        ],
        match_mode: MatchMode::Exact,
    };
    // Fitness and holdout are evidence-disjoint (different inputs + fixtures).
    let fitness = VerificationPlan::new("price").with_golden(vec![golden("fit", 10, 2, 7)]);
    let holdout = VerificationPlan::new("price").with_golden(vec![golden("hold", 5, 3, 99)]);

    // The broken seed fails the holdout (cost ≠ price).
    let seed_holdout = verify(&harness, &broken, &holdout).await.unwrap().score();
    assert!(
        seed_holdout < 1.0,
        "broken seed must be imperfect on the holdout"
    );

    let ops: Vec<Box<dyn Mutation>> = vec![
        Box::new(SetTransformField {
            vocabulary: vec![
                ("total".into(), json!("${{ $.price * $.qty }}")), // the correct fix
                ("total".into(), json!(0)),                        // decoy
            ],
            frozen: vec![],
        }),
        Box::new(InsertPassthrough),
    ];
    let outcome = evolve(
        &harness,
        &broken,
        &fitness,
        &holdout,
        &ops,
        SearchConfig::default(),
    )
    .await
    .expect("search runs");

    assert!(
        outcome.improved_on_holdout() && outcome.best_holdout_score >= 1.0,
        "search must lift the CERTIFIED (holdout) score: {} -> {}",
        outcome.initial_holdout_score,
        outcome.best_holdout_score
    );
    assert!(
        !outcome.overfit(),
        "no overfit gap on a legit fix: {}",
        outcome.overfit_gap
    );
    assert!(a2w_validator::validate(&outcome.best_workflow).is_valid);
    println!(
        "\n  evolved: holdout {:.2} -> {:.2} (overfit_gap {:.2}), {} candidates\n",
        outcome.initial_holdout_score,
        outcome.best_holdout_score,
        outcome.overfit_gap,
        outcome.candidates_evaluated
    );
}
