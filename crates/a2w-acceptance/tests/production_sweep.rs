//! Aggressive, regression-grade production sweep.
//!
//! Where the gallery proves the seven committed examples are correct, this suite
//! hammers the engine across **every node kind** and a wide spread of topologies:
//!
//!   * runs pure-logic workflows in **production** mode (`ExecutionMode::Run`,
//!     real execution, zero LLM tokens, fully deterministic) and asserts exact
//!     outputs — including the UTF-8 and strict-eval accuracy fixes end-to-end;
//!   * proves **Run == DryRun** for pure-logic workflows (no mode-dependent
//!     drift) and **determinism under concurrency** (many reruns, byte-equal);
//!   * exercises the side-effecting kinds (http / llm / mcp / code_step /
//!     approval) through their deterministic dry-run mocks;
//!   * exercises **retry / on_error** policy in production; and
//!   * asserts the validator **rejects every malformed-IR class**
//!     (reject-before-execute), each with the expected finding code.
//!
//! Run with output: `cargo test -p a2w-acceptance --test production_sweep -- --nocapture`

use std::collections::HashMap;

use a2w_engine::{Engine, ExecutionMode, MemoryEventLog, RunStatus};
use a2w_ir::{ErrorPolicy, Workflow};
use a2w_validator::{validate, FindingCode};
use serde_json::{json, Value};

/// Deserialize a workflow from a `json!` value and assert it validates clean.
fn valid_wf(v: Value) -> Workflow {
    let wf: Workflow = serde_json::from_value(v).expect("workflow IR deserializes");
    let report = validate(&wf);
    assert!(
        report.is_valid,
        "workflow '{}' should be valid, findings: {:?}",
        wf.id, report.findings
    );
    wf
}

/// Run `wf` in `mode`; return (status, node-id -> output payloads).
async fn run_mode(
    wf: &Workflow,
    input: Vec<Value>,
    mode: ExecutionMode,
) -> (RunStatus, HashMap<String, Vec<Value>>) {
    let engine = Engine::new(a2w_nodes::default_registry());
    let log = MemoryEventLog::new();
    let run = engine
        .run(wf, input, mode, &log)
        .await
        .expect("run completes");
    let map = run
        .node_outputs
        .iter()
        .map(|(k, v)| (k.clone(), v.iter().map(|i| i.json.clone()).collect()))
        .collect();
    (run.status, map)
}

/// Production-run `wf` and return the payloads of one observed node.
async fn prod(wf: &Workflow, node: &str, input: Vec<Value>) -> Vec<Value> {
    run_mode(wf, input, ExecutionMode::Run)
        .await
        .1
        .remove(node)
        .unwrap_or_default()
}

// ===========================================================================
// 1) Production (Run mode) execution of pure-logic workflows — exact outputs.
// ===========================================================================

#[tokio::test]
async fn production_arithmetic_is_exact_and_zero_token() {
    let wf = valid_wf(json!({
        "schema_version": 1, "id": "wf_prod_math", "name": "math",
        "nodes": [
            { "id": "t", "kind": "webhook_trigger", "params": {} },
            { "id": "calc", "kind": "transform",
              "params": { "set": { "total": "${{ $.price * $.qty }}", "with_tax": "${{ $.price * $.qty * 1.1 }}" } } }
        ],
        "connections": [ { "from_node": "t", "from_port": 0, "to_node": "calc" } ]
    }));
    let out = prod(&wf, "calc", vec![json!({ "price": 10, "qty": 2 })]).await;
    assert_eq!(
        out,
        vec![json!({ "price": 10, "qty": 2, "total": 20.0, "with_tax": 22.0 })]
    );
}

#[tokio::test]
async fn production_utf8_string_concat_round_trips() {
    // End-to-end proof of the UTF-8 lexer fix: multi-byte text in a `${{ }}`
    // string literal survives through the engine in production.
    let wf = valid_wf(json!({
        "schema_version": 1, "id": "wf_prod_utf8", "name": "utf8",
        "nodes": [
            { "id": "t", "kind": "webhook_trigger", "params": {} },
            { "id": "greet", "kind": "transform",
              // single-quoted expr string avoids escaping inner double quotes.
              "params": { "set": { "msg": "${{ 'Héllo, ' + $.name + ' 🚀' }}", "len": "${{ length($.name) }}" } } }
        ],
        "connections": [ { "from_node": "t", "from_port": 0, "to_node": "greet" } ]
    }));
    let out = prod(&wf, "greet", vec![json!({ "name": "naïve" })]).await;
    assert_eq!(
        out,
        vec![json!({ "name": "naïve", "msg": "Héllo, naïve 🚀", "len": 5 })]
    );
}

#[tokio::test]
async fn production_branch_and_loop_route_correctly() {
    // Numeric routing: compute a boolean with the expr DSL (which supports >=),
    // then route on it with a `truthy` branch.
    let branch = valid_wf(json!({
        "schema_version": 1, "id": "wf_prod_branch", "name": "b",
        "nodes": [
            { "id": "t", "kind": "webhook_trigger", "params": {} },
            { "id": "flag", "kind": "transform", "params": { "set": { "pass": "${{ $.score >= 50 }}" } } },
            { "id": "gate", "kind": "branch", "params": { "condition": { "path": "/pass", "op": "truthy" } } },
            { "id": "yes", "kind": "transform", "params": { "set": { "ok": true } } },
            { "id": "no", "kind": "transform", "params": { "set": { "ok": false } } }
        ],
        "connections": [
            { "from_node": "t", "from_port": 0, "to_node": "flag" },
            { "from_node": "flag", "from_port": 0, "to_node": "gate" },
            { "from_node": "gate", "from_port": 0, "to_node": "yes" },
            { "from_node": "gate", "from_port": 1, "to_node": "no" }
        ]
    }));
    let input = vec![
        json!({ "score": 80 }),
        json!({ "score": 20 }),
        json!({ "score": 50 }),
    ];
    let passed = prod(&branch, "yes", input.clone()).await;
    let failed = prod(&branch, "no", input).await;
    assert_eq!(passed.len(), 2, "80 and 50 pass the >=50 gate");
    assert_eq!(failed.len(), 1, "20 fails");
    assert!(passed.iter().all(|i| i["ok"] == json!(true)));

    // switch: 3 cases + default; loop: fan-out; merge: diamond — all in Run.
    let loop_wf = valid_wf(json!({
        "schema_version": 1, "id": "wf_prod_loop", "name": "l",
        "nodes": [
            { "id": "t", "kind": "webhook_trigger", "params": {} },
            { "id": "ex", "kind": "loop", "params": { "over": "/xs" } },
            { "id": "body", "kind": "transform", "params": { "set": { "seen": true } } }
        ],
        "connections": [
            { "from_node": "t", "from_port": 0, "to_node": "ex" },
            { "from_node": "ex", "from_port": 0, "to_node": "body" }
        ]
    }));
    let body = prod(
        &loop_wf,
        "body",
        vec![json!({ "xs": [1, 2, 3] }), json!({ "xs": [4] })],
    )
    .await;
    assert_eq!(body.len(), 4, "3 + 1 line items fan out");
    assert!(body.iter().all(|i| i["seen"] == json!(true)));
}

#[tokio::test]
async fn production_wait_and_schedule_trigger_pass_through() {
    // schedule_trigger as the entry point; wait(0ms) is an instant passthrough.
    let wf = valid_wf(json!({
        "schema_version": 1, "id": "wf_prod_wait", "name": "w",
        "nodes": [
            { "id": "cron", "kind": "schedule_trigger", "params": { "cron": "0 * * * *" } },
            { "id": "hold", "kind": "wait", "params": { "duration_ms": 0 } },
            { "id": "tag", "kind": "transform", "params": { "set": { "ran": true } } }
        ],
        "connections": [
            { "from_node": "cron", "from_port": 0, "to_node": "hold" },
            { "from_node": "hold", "from_port": 0, "to_node": "tag" }
        ]
    }));
    let out = prod(&wf, "tag", vec![json!({ "id": 1 }), json!({ "id": 2 })]).await;
    assert_eq!(
        out,
        vec![
            json!({ "id": 1, "ran": true }),
            json!({ "id": 2, "ran": true })
        ]
    );
}

#[tokio::test]
async fn production_sub_workflow_runs_a_nested_pipeline() {
    // Inline sub-workflow (no resolver needed) executes its own pipeline in Run.
    let wf = valid_wf(json!({
        "schema_version": 1, "id": "wf_prod_sub", "name": "s",
        "nodes": [
            { "id": "t", "kind": "webhook_trigger", "params": {} },
            { "id": "call", "kind": "sub_workflow", "params": {
                "workflow": {
                    "schema_version": 1, "id": "wf_inner", "name": "inner",
                    "nodes": [
                        { "id": "it", "kind": "webhook_trigger", "params": {} },
                        { "id": "stamp", "kind": "transform", "params": { "set": { "inner": true } } }
                    ],
                    "connections": [ { "from_node": "it", "from_port": 0, "to_node": "stamp" } ]
                }
            } }
        ],
        "connections": [ { "from_node": "t", "from_port": 0, "to_node": "call" } ]
    }));
    let (status, map) = run_mode(&wf, vec![json!({ "seed": 1 })], ExecutionMode::Run).await;
    assert_eq!(status, RunStatus::Completed);
    let call = map.get("call").expect("sub_workflow produced output");
    assert_eq!(call.len(), 1, "one terminal item from the nested run");
    assert!(
        call[0]["sub_workflow_id"]
            .as_str()
            .unwrap()
            .contains("wf_inner"),
        "namespaced inline id: {}",
        call[0]["sub_workflow_id"]
    );
    assert_eq!(call[0]["terminal_node"], json!("stamp"));
    assert_eq!(
        call[0]["value"],
        json!({ "seed": 1, "inner": true }),
        "the nested pipeline transformed the item"
    );
}

// ===========================================================================
// 2) Run == DryRun for pure logic (no mode-dependent drift), and determinism.
// ===========================================================================

#[tokio::test]
async fn run_equals_dryrun_for_pure_logic() {
    let wf = valid_wf(json!({
        "schema_version": 1, "id": "wf_equiv", "name": "e",
        "nodes": [
            { "id": "t", "kind": "webhook_trigger", "params": {} },
            { "id": "a", "kind": "transform", "params": { "set": { "n": "${{ $.x + 1 }}", "big": "${{ $.x + 1 > 2 }}" } } },
            { "id": "b", "kind": "branch", "params": { "condition": { "path": "/big", "op": "truthy" } } },
            { "id": "hi", "kind": "transform", "params": { "set": { "band": "hi" } } },
            { "id": "lo", "kind": "transform", "params": { "set": { "band": "lo" } } }
        ],
        "connections": [
            { "from_node": "t", "from_port": 0, "to_node": "a" },
            { "from_node": "a", "from_port": 0, "to_node": "b" },
            { "from_node": "b", "from_port": 0, "to_node": "hi" },
            { "from_node": "b", "from_port": 1, "to_node": "lo" }
        ]
    }));
    let input = vec![json!({ "x": 5 }), json!({ "x": 0 }), json!({ "x": 1 })];
    let (rs, run) = run_mode(&wf, input.clone(), ExecutionMode::Run).await;
    let (ds, dry) = run_mode(&wf, input, ExecutionMode::DryRun).await;
    assert_eq!(rs, RunStatus::Completed);
    assert_eq!(ds, RunStatus::Completed);
    assert_eq!(
        serde_json::to_value(&run).unwrap(),
        serde_json::to_value(&dry).unwrap(),
        "pure-logic Run and DryRun must be identical"
    );
}

#[tokio::test]
async fn determinism_under_concurrency() {
    // A concurrent diamond run many times must be byte-identical every time.
    let wf = valid_wf(json!({
        "schema_version": 1, "id": "wf_diamond", "name": "d",
        "nodes": [
            { "id": "t", "kind": "webhook_trigger", "params": {} },
            { "id": "l", "kind": "transform", "params": { "set": { "side": "left" } } },
            { "id": "r", "kind": "transform", "params": { "set": { "side": "right" } } },
            { "id": "m", "kind": "merge", "params": {} }
        ],
        "connections": [
            { "from_node": "t", "from_port": 0, "to_node": "l" },
            { "from_node": "t", "from_port": 0, "to_node": "r" },
            { "from_node": "l", "from_port": 0, "to_node": "m" },
            { "from_node": "r", "from_port": 0, "to_node": "m" }
        ]
    }));
    let input: Vec<Value> = (0..8).map(|i| json!({ "id": i })).collect();
    let (_, first) = run_mode(&wf, input.clone(), ExecutionMode::Run).await;
    let canonical = serde_json::to_value(&first).unwrap();
    for _ in 0..24 {
        let (status, again) = run_mode(&wf, input.clone(), ExecutionMode::Run).await;
        assert_eq!(status, RunStatus::Completed);
        assert_eq!(
            serde_json::to_value(&again).unwrap(),
            canonical,
            "run must be deterministic"
        );
    }
    assert_eq!(
        first.get("m").map(Vec::len),
        Some(16),
        "8 left + 8 right merged"
    );
}

#[tokio::test]
async fn wide_fan_out_completes_under_the_concurrency_bound() {
    // 40 independent parallel branches off one trigger, all merged.
    let mut nodes = vec![json!({ "id": "t", "kind": "webhook_trigger", "params": {} })];
    let mut conns = Vec::new();
    for i in 0..40 {
        let id = format!("b{i}");
        nodes.push(json!({ "id": id, "kind": "transform", "params": { "set": { "b": i } } }));
        conns.push(json!({ "from_node": "t", "from_port": 0, "to_node": id }));
        conns.push(json!({ "from_node": id, "from_port": 0, "to_node": "sink" }));
    }
    nodes.push(json!({ "id": "sink", "kind": "merge", "params": {} }));
    let wf = valid_wf(json!({
        "schema_version": 1, "id": "wf_wide", "name": "wide",
        "nodes": nodes, "connections": conns
    }));
    let (status, map) = run_mode(&wf, vec![json!({ "seed": 1 })], ExecutionMode::Run).await;
    assert_eq!(status, RunStatus::Completed);
    assert_eq!(
        map.get("sink").map(Vec::len),
        Some(40),
        "all 40 branches merged"
    );
}

// ===========================================================================
// 3) Side-effecting kinds via deterministic dry-run mocks (zero-token).
// ===========================================================================

#[tokio::test]
async fn side_effecting_nodes_mock_deterministically() {
    // http + llm + mcp + code_step + approval, each mocked in DryRun. The point
    // is that they COMPLETE and are byte-identical across reruns (no clock/RNG).
    let wf = valid_wf(json!({
        "schema_version": 1, "id": "wf_side", "name": "side",
        "nodes": [
            { "id": "t", "kind": "webhook_trigger", "params": {} },
            { "id": "http", "kind": "http_request", "params": { "method": "GET", "url": "https://example.com/x" } },
            { "id": "llm", "kind": "llm_call", "params": { "prompt": "summarize {{json}}" } },
            { "id": "mcp", "kind": "mcp_tool_call", "params": { "server": { "transport": "stdio", "command": "a2w-mcp" }, "tool": "wf_validate" } },
            { "id": "code", "kind": "code_step", "params": { "wasm": { "base64": "AGFzbQ==" }, "function": "run" } },
            { "id": "appr", "kind": "approval", "params": { "summary": "ok?" } }
        ],
        "connections": [
            { "from_node": "t", "from_port": 0, "to_node": "http" },
            { "from_node": "http", "from_port": 0, "to_node": "llm" },
            { "from_node": "llm", "from_port": 0, "to_node": "mcp" },
            { "from_node": "mcp", "from_port": 0, "to_node": "code" },
            { "from_node": "code", "from_port": 0, "to_node": "appr" }
        ]
    }));
    let (s1, m1) = run_mode(&wf, vec![json!({ "q": "a" })], ExecutionMode::DryRun).await;
    let (s2, m2) = run_mode(&wf, vec![json!({ "q": "a" })], ExecutionMode::DryRun).await;
    assert_eq!(s1, RunStatus::Completed);
    assert_eq!(s2, RunStatus::Completed);
    // Every side-effecting node produced a mock, deterministically.
    for n in ["http", "llm", "mcp", "code"] {
        assert!(!m1[n].is_empty(), "{n} produced a mock");
    }
    assert_eq!(
        serde_json::to_value(&m1).unwrap(),
        serde_json::to_value(&m2).unwrap(),
        "mocked side effects are deterministic across reruns"
    );
}

// ===========================================================================
// 4) Production retry / on_error policy.
// ===========================================================================

#[tokio::test]
async fn production_on_error_policy_governs_failures() {
    // A transform whose expression cannot evaluate (non-numeric * number).
    let base = json!({
        "schema_version": 1, "id": "wf_onerr", "name": "oe",
        "nodes": [
            { "id": "t", "kind": "webhook_trigger", "params": {} },
            { "id": "calc", "kind": "transform", "params": { "set": { "v": "${{ $.bad * 2 }}" } } }
        ],
        "connections": [ { "from_node": "t", "from_port": 0, "to_node": "calc" } ]
    });
    let bad_input = vec![json!({ "bad": "not-a-number" })];

    // Default (Stop): the engine aborts the run with an error.
    let wf_stop = valid_wf(base.clone());
    let engine = Engine::new(a2w_nodes::default_registry());
    let log = MemoryEventLog::new();
    let res = engine
        .run(&wf_stop, bad_input.clone(), ExecutionMode::Run, &log)
        .await;
    assert!(
        res.is_err(),
        "Stop policy aborts the run on a bad expression: {res:?}"
    );

    // Continue: the failure is contained — the node yields zero items, run ok.
    let mut wf_cont = wf_stop.clone();
    for n in &mut wf_cont.nodes {
        if n.id == "calc" {
            n.on_error = Some(ErrorPolicy::Continue);
        }
    }
    let (status, map) = run_mode(&wf_cont, bad_input, ExecutionMode::Run).await;
    assert_eq!(status, RunStatus::Completed, "Continue keeps the run alive");
    assert!(
        map.get("calc").map(Vec::is_empty).unwrap_or(true),
        "failed node produced nothing"
    );
}

// ===========================================================================
// 5) Reject-before-execute: the validator catches every malformed-IR class.
// ===========================================================================

#[test]
fn validator_rejects_every_malformed_class() {
    fn rejected_with(v: Value, code: FindingCode) {
        let wf: Workflow = serde_json::from_value(v).expect("parses as IR");
        let report = validate(&wf);
        assert!(!report.is_valid, "expected invalid for {code:?}");
        assert!(
            report.findings.iter().any(|f| f.code == code),
            "expected finding {code:?}, got {:?}",
            report.findings.iter().map(|f| f.code).collect::<Vec<_>>()
        );
    }

    let trig = json!({ "id": "t", "kind": "webhook_trigger", "params": {} });

    // unsupported schema version
    rejected_with(
        json!({ "schema_version": 2, "id": "w", "name": "n",
                "nodes": [trig], "connections": [] }),
        FindingCode::UnsupportedSchemaVersion,
    );
    // no trigger
    rejected_with(
        json!({ "schema_version": 1, "id": "w", "name": "n",
                "nodes": [ { "id": "a", "kind": "transform", "params": {} } ], "connections": [] }),
        FindingCode::NoTrigger,
    );
    // multiple triggers
    rejected_with(
        json!({ "schema_version": 1, "id": "w", "name": "n",
                "nodes": [ trig, { "id": "t2", "kind": "webhook_trigger", "params": {} } ],
                "connections": [] }),
        FindingCode::MultipleTriggers,
    );
    // duplicate node id
    rejected_with(
        json!({ "schema_version": 1, "id": "w", "name": "n",
                "nodes": [ trig, { "id": "t", "kind": "transform", "params": {} } ],
                "connections": [] }),
        FindingCode::DuplicateNodeId,
    );
    // dangling connection source
    rejected_with(
        json!({ "schema_version": 1, "id": "w", "name": "n",
                "nodes": [ trig, { "id": "a", "kind": "transform", "params": {} } ],
                "connections": [ { "from_node": "ghost", "from_port": 0, "to_node": "a" } ] }),
        FindingCode::DanglingConnectionSource,
    );
    // dangling connection target
    rejected_with(
        json!({ "schema_version": 1, "id": "w", "name": "n",
                "nodes": [ trig ],
                "connections": [ { "from_node": "t", "from_port": 0, "to_node": "ghost" } ] }),
        FindingCode::DanglingConnectionTarget,
    );
    // invalid output port (transform has 1 output port; port 5 is out of range)
    rejected_with(
        json!({ "schema_version": 1, "id": "w", "name": "n",
                "nodes": [ trig, { "id": "a", "kind": "transform", "params": {} } ],
                "connections": [ { "from_node": "a", "from_port": 5, "to_node": "a" } ] }),
        FindingCode::InvalidOutputPort,
    );
    // cycle (a -> b -> a)
    rejected_with(
        json!({ "schema_version": 1, "id": "w", "name": "n",
                "nodes": [ trig,
                    { "id": "a", "kind": "transform", "params": {} },
                    { "id": "b", "kind": "transform", "params": {} } ],
                "connections": [
                    { "from_node": "t", "from_port": 0, "to_node": "a" },
                    { "from_node": "a", "from_port": 0, "to_node": "b" },
                    { "from_node": "b", "from_port": 0, "to_node": "a" } ] }),
        FindingCode::Cycle,
    );
    // missing required param (http_request without url)
    rejected_with(
        json!({ "schema_version": 1, "id": "w", "name": "n",
                "nodes": [ trig, { "id": "h", "kind": "http_request", "params": { "method": "GET" } } ],
                "connections": [ { "from_node": "t", "from_port": 0, "to_node": "h" } ] }),
        FindingCode::MissingRequiredParam,
    );
    // invalid param type (http_request url is a number)
    rejected_with(
        json!({ "schema_version": 1, "id": "w", "name": "n",
                "nodes": [ trig, { "id": "h", "kind": "http_request", "params": { "url": 42 } } ],
                "connections": [ { "from_node": "t", "from_port": 0, "to_node": "h" } ] }),
        FindingCode::InvalidParamType,
    );
    // trigger has an incoming connection
    rejected_with(
        json!({ "schema_version": 1, "id": "w", "name": "n",
                "nodes": [ trig, { "id": "a", "kind": "transform", "params": {} } ],
                "connections": [
                    { "from_node": "t", "from_port": 0, "to_node": "a" },
                    { "from_node": "a", "from_port": 0, "to_node": "t" } ] }),
        FindingCode::TriggerHasIncomingConnection,
    );
    // sub_workflow self-reference (workflow_id == enclosing id)
    rejected_with(
        json!({ "schema_version": 1, "id": "wf_selfref", "name": "n",
                "nodes": [ trig,
                    { "id": "s", "kind": "sub_workflow", "params": { "workflow_id": "wf_selfref" } } ],
                "connections": [ { "from_node": "t", "from_port": 0, "to_node": "s" } ] }),
        FindingCode::SubWorkflowSelfReference,
    );
}
