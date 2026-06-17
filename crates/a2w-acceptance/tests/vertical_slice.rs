//! Vertical-slice acceptance test.
//!
//! Proves the full agent loop composes end-to-end, deterministically, with
//! **zero LLM tokens**: an agent authors a workflow, the validator catches a
//! mistake with a located finding, the agent repairs it, dry-runs it (side
//! effects mocked), adds a test case and runs it, profiles the run, asks the
//! optimizer for improvements, applies the flagship "parallelize independent
//! serial steps" suggestion, and re-tests to confirm the optimization is a real
//! rewrite with no regression.
//!
//! This is the milestone the plan calls the "thin end-to-end vertical slice".

use a2w_engine::{Engine, ExecutionMode, MemoryEventLog, RunStatus};
use a2w_ir::{Connection, Node, NodeKind, Workflow, SCHEMA_VERSION};
use a2w_nodes::default_registry;
use a2w_optimizer::{analyze, apply, profile, SuggestionKind};
use a2w_testkit::{run_tests, Expectation, TestCase};
use a2w_validator::validate;
use serde_json::json;

/// Build an HTTP node with a literal URL (no `{{json}}` templating, so it does
/// not consume its predecessor's output — making it a parallelize candidate).
fn http_node(id: &str, url: &str) -> Node {
    let mut n = Node::new(id, NodeKind::HttpRequest);
    n.params = json!({ "method": "GET", "url": url });
    n
}

#[tokio::test]
async fn vertical_slice_author_validate_repair_run_test_profile_optimize() {
    // ---- 1. AUTHOR -------------------------------------------------------
    // Simulate an agent's first emission, with a deliberate mistake: the
    // second connection targets "shap", a node id that does not exist.
    let mut wf = Workflow {
        schema_version: SCHEMA_VERSION,
        id: "slice".into(),
        name: "vertical slice".into(),
        nodes: vec![
            Node::new("trigger", NodeKind::WebhookTrigger),
            http_node("fetch_a", "https://api.example.com/a"),
            http_node("fetch_b", "https://api.example.com/b"),
        ],
        connections: vec![
            Connection::new("trigger", 0, "fetch_a"),
            Connection::new("fetch_a", 0, "shap"), // BUG: typo'd target id
        ],
    };

    // ---- 2. VALIDATE -> located finding ----------------------------------
    let report = validate(&wf);
    assert!(!report.is_valid, "the buggy workflow must not validate");
    assert!(
        report.findings.iter().any(|f| f.message.contains("shap")),
        "validator should locate the dangling 'shap' target: {:?}",
        report.findings
    );

    // ---- 3. REPAIR (agent applies the fix) -------------------------------
    for c in &mut wf.connections {
        if c.to_node == "shap" {
            c.to_node = "fetch_b".into();
        }
    }
    let report = validate(&wf);
    assert!(
        report.is_valid,
        "repaired workflow should validate cleanly: {:?}",
        report.findings
    );
    // wf is now: trigger -> fetch_a -> fetch_b  (serial; fetch_b is independent).

    // ---- 4. DRY-RUN (side effects mocked; zero network, zero LLM tokens) --
    let engine = Engine::new(default_registry());
    let log = MemoryEventLog::new();
    let result = engine
        .run(&wf, vec![json!({})], ExecutionMode::DryRun, &log)
        .await
        .expect("dry run should succeed");
    assert_eq!(result.status, RunStatus::Completed);
    assert!(result.node_outputs.contains_key("fetch_a"));
    assert!(result.node_outputs.contains_key("fetch_b"));
    // Zero-token guarantee: a deterministic run spends no LLM tokens.
    assert_eq!(
        result.events.iter().map(|e| e.tokens).sum::<u64>(),
        0,
        "a deterministic run must consume zero LLM tokens"
    );
    // The HTTP node was mocked, not actually called.
    let fetch_b_out = &result.node_outputs["fetch_b"];
    assert_eq!(fetch_b_out.len(), 1);
    assert_eq!(fetch_b_out[0].json["_mock"], json!(true));

    // ---- 5. TEST (add a case, run it) ------------------------------------
    let cases = vec![TestCase {
        name: "fetch_b is reached and mocked".into(),
        trigger_input: vec![json!({})],
        expect: Expectation::NodeOutputContains {
            node_id: "fetch_b".into(),
            json: json!({ "_mock": true }),
        },
    }];
    let before = run_tests(&engine, &wf, &cases, ExecutionMode::DryRun).await;
    assert!(
        before.iter().all(|r| r.passed),
        "tests must pass before optimizing: {before:?}"
    );

    // ---- 6. PROFILE ------------------------------------------------------
    let prof = profile(&wf, &result);
    assert!(
        prof.per_step.iter().any(|s| s.node_id == "fetch_a"),
        "profile should cover fetch_a"
    );

    // ---- 7. OPTIMIZE -----------------------------------------------------
    // fetch_b is serial after fetch_a but independent of it (literal URL),
    // so the optimizer should propose parallelizing them.
    let suggestions = analyze(&wf, Some(&prof));
    let parallelize = suggestions
        .iter()
        .find(|s| s.kind == SuggestionKind::Parallelize)
        .expect("optimizer should suggest parallelizing fetch_a/fetch_b");

    // ---- 8. APPLY the suggestion -----------------------------------------
    let optimized = apply(&wf, &parallelize.ops);
    assert!(
        validate(&optimized).is_valid,
        "optimized workflow must still be valid"
    );
    let has_edge = |from: &str, to: &str| {
        optimized
            .connections
            .iter()
            .any(|c| c.from_node == from && c.to_node == to)
    };
    // Both fetches now hang directly off the trigger -> they run in parallel.
    assert!(has_edge("trigger", "fetch_a"), "trigger->fetch_a expected");
    assert!(has_edge("trigger", "fetch_b"), "trigger->fetch_b expected");
    assert!(
        !has_edge("fetch_a", "fetch_b"),
        "the serial fetch_a->fetch_b edge must be removed"
    );

    // ---- 9. RE-TEST (no regression) --------------------------------------
    let after = run_tests(&engine, &optimized, &cases, ExecutionMode::DryRun).await;
    assert!(
        after.iter().all(|r| r.passed),
        "the same tests must still pass after optimizing (no regression): {after:?}"
    );
}
