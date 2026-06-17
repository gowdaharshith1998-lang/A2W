//! In-process tests for the A2W MCP tool logic.
//!
//! These exercise the transport-free `*_logic` methods on [`A2wServer`] — the
//! exact functions the `#[tool]` handlers delegate to — so we test the real
//! wiring over the core crates without standing up a stdio loop. Inputs are
//! built as JSON (`serde_json::json!`) exactly as an agent would supply them,
//! and outputs are asserted as JSON.

use a2w_llm::MockLlm;
use a2w_mcp::{
    A2wServer, ApplyOpsInput, GetTemplateInput, OptimizeInput, RunInput, RunTestsInput,
    SearchTemplatesInput, WorkflowInput,
};
use serde_json::{json, Value};

/// A small valid workflow: `webhook_trigger -> transform`. Pure, no network.
fn valid_workflow() -> Value {
    json!({
        "schema_version": 1,
        "id": "wf_test",
        "name": "test",
        "nodes": [
            { "id": "trigger", "kind": "webhook_trigger", "params": {} },
            { "id": "shape", "kind": "transform", "params": { "set": { "tag": "x" } } }
        ],
        "connections": [
            { "from_node": "trigger", "from_port": 0, "to_node": "shape" }
        ]
    })
}

/// `trigger -> a(http literal) -> b(http literal)`: B is data-independent of A,
/// so the optimizer should suggest parallelizing it.
fn independent_chain() -> Value {
    json!({
        "schema_version": 1,
        "id": "wf_par",
        "name": "independent chain",
        "nodes": [
            { "id": "trigger", "kind": "webhook_trigger", "params": {} },
            { "id": "a", "kind": "http_request", "params": { "url": "https://example.com/a" } },
            { "id": "b", "kind": "http_request", "params": { "url": "https://example.com/b" } }
        ],
        "connections": [
            { "from_node": "trigger", "from_port": 0, "to_node": "a" },
            { "from_node": "a", "from_port": 0, "to_node": "b" }
        ]
    })
}

#[test]
fn get_schema_returns_workflow_schema() {
    let schema = A2wServer::get_schema_logic().expect("schema");
    let s = serde_json::to_string(&schema).unwrap();
    assert!(s.contains("nodes"), "schema must mention nodes");
    assert!(s.contains("connections"), "schema must mention connections");
}

#[test]
fn describe_nodes_lists_taxonomy() {
    let nodes = A2wServer::describe_nodes_logic().expect("taxonomy");
    let arr = nodes.as_array().expect("array of node kinds");
    assert_eq!(arr.len(), 14, "all 14 NodeKind variants");

    // webhook_trigger: 1 port, is a trigger.
    let webhook = arr
        .iter()
        .find(|n| n["name"] == "webhook_trigger")
        .expect("webhook_trigger present");
    assert_eq!(webhook["output_port_count"], json!(1));
    assert_eq!(webhook["is_trigger"], json!(true));
    assert_eq!(webhook["dynamic_ports"], json!(false));

    // branch: 2 ports, not a trigger.
    let branch = arr.iter().find(|n| n["name"] == "branch").unwrap();
    assert_eq!(branch["output_port_count"], json!(2));
    assert_eq!(branch["is_trigger"], json!(false));

    // switch: dynamic ports -> null count, dynamic_ports true.
    let switch = arr.iter().find(|n| n["name"] == "switch").unwrap();
    assert_eq!(switch["output_port_count"], Value::Null);
    assert_eq!(switch["dynamic_ports"], json!(true));
}

#[test]
fn validate_invalid_workflow_reports_errors() {
    let server = A2wServer::new();
    // No trigger and a dangling target -> at minimum a NoTrigger error.
    let bad = json!({
        "schema_version": 1,
        "id": "wf_bad",
        "name": "bad",
        "nodes": [ { "id": "a", "kind": "transform", "params": {} } ],
        "connections": []
    });
    let report = server
        .validate_logic(WorkflowInput { workflow: bad })
        .expect("validate returns a report even for invalid workflows");
    assert_eq!(report["is_valid"], json!(false), "report: {report}");
    let codes: Vec<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap())
        .collect();
    assert!(
        codes.contains(&"no_trigger"),
        "expected a no_trigger finding, got {codes:?}"
    );
}

#[test]
fn validate_valid_workflow_is_valid() {
    let server = A2wServer::new();
    let report = server
        .validate_logic(WorkflowInput {
            workflow: valid_workflow(),
        })
        .expect("validate");
    assert_eq!(report["is_valid"], json!(true), "report: {report}");
}

#[test]
fn validate_rejects_malformed_json_as_tool_error() {
    let server = A2wServer::new();
    // `nodes` is the wrong type -> serde fails -> invalid_params tool error.
    let garbage = json!({ "schema_version": 1, "id": "x", "name": "x", "nodes": "not-an-array", "connections": [] });
    let err = server
        .validate_logic(WorkflowInput { workflow: garbage })
        .expect_err("malformed workflow must be a tool error, not a panic");
    assert!(
        err.message.contains("not a valid A2W workflow"),
        "message: {}",
        err.message
    );
}

#[tokio::test]
async fn dry_run_sample_completes_with_mock() {
    let server = A2wServer::new();
    let result = server
        .dry_run_logic(RunInput {
            workflow: valid_workflow(),
            trigger_input: vec![json!({ "id": 1 })],
        })
        .await
        .expect("dry run");
    assert_eq!(result["status"], json!("completed"), "result: {result}");
    // node_outputs must contain both nodes.
    let outputs = result["node_outputs"].as_object().unwrap();
    assert!(outputs.contains_key("trigger"));
    assert!(outputs.contains_key("shape"));
}

#[tokio::test]
async fn dry_run_http_node_is_mocked() {
    // The independent chain has two HttpRequest nodes; in DryRun they must NOT
    // make real network calls — they return the default mock item.
    let server = A2wServer::new();
    let result = server
        .dry_run_logic(RunInput {
            workflow: independent_chain(),
            trigger_input: vec![json!({ "seed": true })],
        })
        .await
        .expect("dry run of http chain");
    assert_eq!(result["status"], json!("completed"), "result: {result}");
    // The mock item carries `_mock: true`.
    let a_items = result["node_outputs"]["a"].as_array().unwrap();
    assert!(
        a_items.iter().any(|it| it["json"]["_mock"] == json!(true)),
        "http node 'a' should be mocked in dry run: {result}"
    );
}

#[tokio::test]
async fn dry_run_invalid_workflow_is_tool_error_with_report() {
    let server = A2wServer::new();
    let bad = json!({
        "schema_version": 1, "id": "wf_bad", "name": "bad",
        "nodes": [ { "id": "a", "kind": "transform", "params": {} } ],
        "connections": []
    });
    let err = server
        .dry_run_logic(RunInput {
            workflow: bad,
            trigger_input: vec![],
        })
        .await
        .expect_err("invalid workflow must error");
    assert!(err.message.contains("run failed"), "message: {}", err.message);
    // The structured ValidationReport rides along as the error `data`.
    let data = err.data.expect("engine validation report in error data");
    assert_eq!(data["is_valid"], json!(false), "data: {data}");
}

#[tokio::test]
async fn run_tests_evaluates_cases() {
    let server = A2wServer::new();
    let tests = vec![
        json!({
            "name": "completes",
            "trigger_input": [ { "id": 1 } ],
            "expect": { "kind": "completes" }
        }),
        json!({
            "name": "shape adds tag",
            "trigger_input": [ { "id": 1 } ],
            "expect": {
                "kind": "node_output_contains",
                "node_id": "shape",
                "json": { "tag": "x", "id": 1 }
            }
        }),
    ];
    let results = server
        .run_tests_logic(RunTestsInput {
            workflow: valid_workflow(),
            tests,
        })
        .await
        .expect("run tests");
    let arr = results.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert!(arr.iter().all(|r| r["passed"] == json!(true)), "results: {results}");
}

#[tokio::test]
async fn profile_returns_critical_path() {
    let server = A2wServer::new();
    let prof = server
        .profile_logic(RunInput {
            workflow: independent_chain(),
            trigger_input: vec![json!({ "id": 1 })],
        })
        .await
        .expect("profile");
    let path: Vec<&str> = prof["critical_path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(path, vec!["trigger", "a", "b"], "prof: {prof}");
}

#[tokio::test]
async fn optimize_independent_chain_suggests_parallelize() {
    let server = A2wServer::new();
    let suggestions = server
        .optimize_logic(OptimizeInput {
            workflow: independent_chain(),
            with_profile: false,
            trigger_input: vec![],
        })
        .await
        .expect("optimize");
    let arr = suggestions.as_array().unwrap();
    let par: Vec<&Value> = arr.iter().filter(|s| s["kind"] == json!("parallelize")).collect();
    assert_eq!(par.len(), 1, "expected one parallelize suggestion: {suggestions}");
    // Without a profile, no estimated gain.
    assert_eq!(par[0]["estimated_gain_ms"], Value::Null);
    // The ops remove a->b and add trigger[0]->b.
    let ops = par[0]["ops"].as_array().unwrap();
    assert!(ops.iter().any(|o| o["op"] == json!("remove_connection")
        && o["from_node"] == json!("a")
        && o["to_node"] == json!("b")));
    assert!(ops.iter().any(|o| o["op"] == json!("add_connection")
        && o["from_node"] == json!("trigger")
        && o["to_node"] == json!("b")));
}

#[tokio::test]
async fn optimize_with_profile_fills_estimated_gain() {
    let server = A2wServer::new();
    let suggestions = server
        .optimize_logic(OptimizeInput {
            workflow: independent_chain(),
            with_profile: true,
            trigger_input: vec![json!({ "id": 1 })],
        })
        .await
        .expect("optimize with profile");
    let arr = suggestions.as_array().unwrap();
    let par = arr
        .iter()
        .find(|s| s["kind"] == json!("parallelize"))
        .expect("a parallelize suggestion");
    // estimated_gain_ms is present (a number) when profiled. It may be 0 in a
    // fast dry run, but it must not be null.
    assert!(
        par["estimated_gain_ms"].is_number(),
        "with_profile should fill estimated_gain_ms: {par}"
    );
}

#[test]
fn apply_ops_rewires_workflow() {
    let server = A2wServer::new();
    let ops = vec![
        json!({ "op": "remove_connection", "from_node": "a", "from_port": 0, "to_node": "b" }),
        json!({ "op": "add_connection", "from_node": "trigger", "from_port": 0, "to_node": "b" }),
    ];
    let new_wf = server
        .apply_ops_logic(ApplyOpsInput {
            workflow: independent_chain(),
            ops,
        })
        .expect("apply ops");
    let conns = new_wf["connections"].as_array().unwrap();
    let edges: Vec<(String, String)> = conns
        .iter()
        .map(|c| {
            (
                c["from_node"].as_str().unwrap().to_string(),
                c["to_node"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(edges.contains(&("trigger".into(), "b".into())), "edges: {edges:?}");
    assert!(!edges.contains(&("a".into(), "b".into())), "a->b removed: {edges:?}");
}

#[test]
fn apply_ops_rejects_malformed_op() {
    let server = A2wServer::new();
    let ops = vec![json!({ "op": "teleport", "from_node": "a" })];
    let err = server
        .apply_ops_logic(ApplyOpsInput {
            workflow: independent_chain(),
            ops,
        })
        .expect_err("unknown op must be a tool error");
    assert!(err.message.contains("not a valid IrOp"), "message: {}", err.message);
}

#[test]
fn search_templates_finds_slack() {
    let server = A2wServer::new();
    let hits = server
        .search_templates_logic(SearchTemplatesInput {
            query: "slack".to_string(),
        })
        .expect("search");
    let arr = hits.as_array().expect("array of template summaries");
    assert!(
        arr.iter().any(|t| t["id"] == json!("webhook_to_slack")),
        "expected the slack template: {hits}"
    );
    // Summaries carry metadata but not the full workflow body.
    let slack = arr.iter().find(|t| t["id"] == json!("webhook_to_slack")).unwrap();
    assert!(slack["tags"].is_array());
    assert!(slack.get("workflow").is_none(), "summary omits the workflow body");
}

#[test]
fn get_template_returns_workflow() {
    let server = A2wServer::new();
    let wf = server
        .get_template_logic(GetTemplateInput {
            id: "webhook_to_slack".to_string(),
        })
        .expect("known template");
    assert_eq!(wf["id"], json!("webhook_to_slack"));
    assert!(wf["nodes"].as_array().unwrap().len() >= 2, "wf: {wf}");
}

#[test]
fn get_template_unknown_id_is_tool_error() {
    let server = A2wServer::new();
    let err = server
        .get_template_logic(GetTemplateInput {
            id: "does_not_exist".to_string(),
        })
        .expect_err("unknown template must be a tool error");
    assert!(err.message.contains("no template with id"), "message: {}", err.message);
}

#[tokio::test]
async fn generate_logic_with_mock_succeeds() {
    let server = A2wServer::new();
    // A valid, dry-runnable workflow the mock returns on the first call.
    let wf = json!({
        "schema_version": 1,
        "id": "wf_gen",
        "name": "generated",
        "nodes": [
            { "id": "trigger", "kind": "webhook_trigger", "params": {} },
            { "id": "shape", "kind": "transform", "params": { "set": { "ok": true } } }
        ],
        "connections": [
            { "from_node": "trigger", "from_port": 0, "to_node": "shape" }
        ]
    })
    .to_string();
    let mock = MockLlm::new(vec![wf]);

    let outcome = server
        .generate_logic("notify me on a webhook", 3, &mock)
        .await
        .expect("generate logic should not transport-fail with a mock");

    assert_eq!(outcome["success"], json!(true), "outcome: {outcome}");
    assert!(outcome["workflow"].is_object(), "outcome carries the workflow: {outcome}");
    assert_eq!(
        outcome["iterations"].as_array().unwrap().len(),
        1,
        "succeeds on the first attempt"
    );
}

#[tokio::test]
async fn generate_logic_with_mock_repairs_then_succeeds() {
    let server = A2wServer::new();
    // First an invalid workflow (dangling target), then a valid one.
    let invalid = json!({
        "schema_version": 1, "id": "bad", "name": "bad",
        "nodes": [ { "id": "trigger", "kind": "webhook_trigger", "params": {} } ],
        "connections": [ { "from_node": "trigger", "from_port": 0, "to_node": "ghost" } ]
    })
    .to_string();
    let valid = json!({
        "schema_version": 1, "id": "ok", "name": "ok",
        "nodes": [
            { "id": "trigger", "kind": "webhook_trigger", "params": {} },
            { "id": "shape", "kind": "transform", "params": {} }
        ],
        "connections": [ { "from_node": "trigger", "from_port": 0, "to_node": "shape" } ]
    })
    .to_string();
    let mock = MockLlm::new(vec![invalid, valid]);

    let outcome = server
        .generate_logic("make it valid", 3, &mock)
        .await
        .expect("no transport error");
    assert_eq!(outcome["success"], json!(true), "outcome: {outcome}");
    assert_eq!(
        outcome["iterations"].as_array().unwrap().len(),
        2,
        "one repair then success"
    );
}
