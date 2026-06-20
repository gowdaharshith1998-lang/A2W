//! Integration tests for the M2 engine + core nodes.
//!
//! All tests are network-free: they use `DryRun` or pure nodes (Transform), and
//! a tiny test-only failing executor where a deterministic failure is needed.

use std::sync::Arc;

use async_trait::async_trait;

use a2w_engine::{
    Engine, EngineError, EventLog, ExecutionMode, Item, ItemSource, MemoryEventLog, NodeContext,
    NodeError, NodeExecutor, NodeRegistry, RunStatus, StepKind,
};
use a2w_ir::{Connection, ErrorPolicy, Node, NodeKind, Workflow, SCHEMA_VERSION};
use a2w_nodes::{
    default_registry, CodeError, CodeStep, McpError, McpInvoker, McpServerSpec, McpToolCall,
    WasmRunner,
};

/// Build a workflow with the standard boilerplate filled in.
fn wf(nodes: Vec<Node>, connections: Vec<Connection>) -> Workflow {
    Workflow {
        schema_version: SCHEMA_VERSION,
        id: "wf_test".to_string(),
        name: "test".to_string(),
        nodes,
        connections,
    }
}

/// A Transform node carrying a `set` param.
fn transform_set(id: &str, set: serde_json::Value) -> Node {
    let mut n = Node::new(id, NodeKind::Transform);
    n.params = serde_json::json!({ "set": set });
    n
}

/// Finished events for a given node id.
fn finished_for<'a>(
    events: &'a [a2w_engine::StepEvent],
    node_id: &str,
) -> Vec<&'a a2w_engine::StepEvent> {
    events
        .iter()
        .filter(|e| e.node_id == node_id && matches!(e.kind, StepKind::Finished))
        .collect()
}

// --- Test 1: dry-run the sample-shaped workflow --------------------------------

#[tokio::test]
async fn dry_run_sample_workflow_shape() {
    // Webhook -> HTTP -> Transform(set). We build it inline so the Transform
    // carries a `set` param (the library sample's params are empty).
    let trigger = Node::new("trigger", NodeKind::WebhookTrigger);
    let mut fetch = Node::new("fetch", NodeKind::HttpRequest);
    fetch.params = serde_json::json!({ "method": "GET", "url": "https://example.com/{{json.hello}}" });
    let shape = transform_set("shape", serde_json::json!({ "shaped": true }));

    let workflow = wf(
        vec![trigger, fetch, shape],
        vec![
            Connection::new("trigger", 0, "fetch"),
            Connection::new("fetch", 0, "shape"),
        ],
    );

    let engine = Engine::new(default_registry());
    let log = MemoryEventLog::new();
    let result = engine
        .run(
            &workflow,
            vec![serde_json::json!({ "hello": "world" })],
            ExecutionMode::DryRun,
            &log,
        )
        .await
        .expect("run should succeed");

    assert_eq!(result.status, RunStatus::Completed);

    // HTTP node output is the mock (no network).
    let http_out = result
        .node_outputs
        .get("fetch")
        .expect("fetch output present");
    assert_eq!(http_out.len(), 1);
    assert_eq!(http_out[0].json["_mock"], serde_json::json!(true));
    // The interim templating helper resolved {{json.hello}} from the input item.
    assert_eq!(
        http_out[0].json["url"],
        serde_json::json!("https://example.com/world")
    );

    // Transform output reflects the `set`.
    let shape_out = result
        .node_outputs
        .get("shape")
        .expect("shape output present");
    assert_eq!(shape_out.len(), 1);
    assert_eq!(shape_out[0].json["shaped"], serde_json::json!(true));

    // Final-item lineage points to the transform node.
    match &shape_out[0].source {
        ItemSource::Produced { node_id, .. } => assert_eq!(node_id, "shape"),
        other => panic!("expected Produced lineage, got {other:?}"),
    }

    // Finished events exist for all three nodes, each with a recorded latency.
    for id in ["trigger", "fetch", "shape"] {
        let fin = finished_for(&result.events, id);
        assert_eq!(fin.len(), 1, "exactly one Finished for {id}");
        // latency_ms is a u64; assert the field is present/recorded (>= 0 always
        // holds, but we assert the event carries the right counts too).
        assert_eq!(fin[0].kind, StepKind::Finished);
    }
}

// --- Test 2: parallel independent branches + fan-in ----------------------------

#[tokio::test]
async fn parallel_branches_fan_in() {
    // trigger -> A(set a=1) , trigger -> B(set b=2) , A->merge , B->merge.
    let workflow = wf(
        vec![
            Node::new("trigger", NodeKind::WebhookTrigger),
            transform_set("A", serde_json::json!({ "a": 1 })),
            transform_set("B", serde_json::json!({ "b": 2 })),
            Node::new("merge", NodeKind::Transform), // passthrough (no `set`)
        ],
        vec![
            Connection::new("trigger", 0, "A"),
            Connection::new("trigger", 0, "B"),
            Connection::new("A", 0, "merge"),
            Connection::new("B", 0, "merge"),
        ],
    );

    let engine = Engine::new(default_registry());
    let log = MemoryEventLog::new();
    let result = engine
        .run(
            &workflow,
            vec![serde_json::json!({})],
            ExecutionMode::Run, // Transform is pure, safe to actually run
            &log,
        )
        .await
        .expect("run should succeed");

    assert_eq!(result.status, RunStatus::Completed);

    // Both A and B executed.
    assert!(result.node_outputs.contains_key("A"));
    assert!(result.node_outputs.contains_key("B"));

    // Merge received items from BOTH branches (fan-in): one item from A, one
    // from B = 2 items.
    let merged = result.node_outputs.get("merge").expect("merge output");
    assert_eq!(merged.len(), 2, "merge should fan-in both branches");

    // The two merged items carry the A and B payloads respectively (passthrough
    // preserves their json). Deterministic order: edges sorted by from_node, so
    // "A" before "B".
    assert_eq!(merged[0].json["a"], serde_json::json!(1));
    assert_eq!(merged[1].json["b"], serde_json::json!(2));

    // Lineage is preserved/re-stamped to the merge node.
    for (idx, item) in merged.iter().enumerate() {
        match &item.source {
            ItemSource::Produced { node_id, item_index } => {
                assert_eq!(node_id, "merge");
                assert_eq!(*item_index, idx);
            }
            other => panic!("expected Produced lineage, got {other:?}"),
        }
    }
}

// --- Test 3: on_error = Continue lets the run complete -------------------------

/// A test-only executor that always fails, for deterministic error-policy tests
/// (avoids network flakiness from pointing reqwest at a bad URL).
#[derive(Debug, Default)]
struct AlwaysFail;

#[async_trait]
impl NodeExecutor for AlwaysFail {
    fn has_side_effects(&self) -> bool {
        true
    }
    async fn execute(&self, _ctx: &NodeContext, _input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        Err(NodeError::Runtime("deliberate test failure".into()))
    }
    // Note: side-effecting => default dry_run would mock. We force real Run mode
    // in the test so execute() runs and fails.
}

#[tokio::test]
async fn on_error_continue_completes() {
    // Register AlwaysFail for the HttpRequest kind so a failing node is wired in
    // deterministically without touching the network.
    let registry = NodeRegistry::new()
        .with(NodeKind::WebhookTrigger, Arc::new(a2w_nodes::WebhookTrigger))
        .with(NodeKind::HttpRequest, Arc::new(AlwaysFail))
        .with(NodeKind::Transform, Arc::new(a2w_nodes::Transform));

    // trigger -> failing(HttpRequest) -> sink(Transform passthrough)
    let mut failing = Node::new("failing", NodeKind::HttpRequest);
    failing.on_error = Some(ErrorPolicy::Continue);

    let workflow = wf(
        vec![
            Node::new("trigger", NodeKind::WebhookTrigger),
            failing,
            Node::new("sink", NodeKind::Transform),
        ],
        vec![
            Connection::new("trigger", 0, "failing"),
            Connection::new("failing", 0, "sink"),
        ],
    );

    let engine = Engine::new(registry);
    let log = MemoryEventLog::new();
    let result = engine
        .run(
            &workflow,
            vec![serde_json::json!({})],
            ExecutionMode::Run,
            &log,
        )
        .await
        .expect("run should complete despite the failing node");

    assert_eq!(result.status, RunStatus::Completed);

    // The failing node produced zero items under Continue.
    let failing_out = result.node_outputs.get("failing").expect("failing output");
    assert!(failing_out.is_empty(), "Continue => zero items");

    // The sink ran and received zero items (nothing upstream).
    let sink_out = result.node_outputs.get("sink").expect("sink output");
    assert!(sink_out.is_empty());

    // A Failed event was still recorded for the failing node.
    assert!(
        result
            .events
            .iter()
            .any(|e| e.node_id == "failing" && matches!(e.kind, StepKind::Failed)),
        "a Failed event should be recorded even under Continue"
    );
}

// --- Test 4: invalid workflow is refused, no events ----------------------------

#[tokio::test]
async fn invalid_workflow_is_refused() {
    // No trigger => invalid.
    let workflow = wf(
        vec![
            Node::new("a", NodeKind::Transform),
            Node::new("b", NodeKind::Transform),
        ],
        vec![Connection::new("a", 0, "b")],
    );

    let engine = Engine::new(default_registry());
    let log = MemoryEventLog::new();
    let err = engine
        .run(&workflow, vec![], ExecutionMode::DryRun, &log)
        .await
        .expect_err("invalid workflow must be refused");

    match err {
        EngineError::Invalid(report) => assert!(!report.is_valid),
        other => panic!("expected Invalid, got {other:?}"),
    }

    // No step events were recorded (nothing executed).
    assert!(
        log.events().is_empty(),
        "no events should be recorded for a refused workflow"
    );
}

// --- Test 5: event log captures Started+Finished pairs -------------------------

#[tokio::test]
async fn event_log_started_finished_pairs() {
    let workflow = wf(
        vec![
            Node::new("trigger", NodeKind::WebhookTrigger),
            {
                let mut fetch = Node::new("fetch", NodeKind::HttpRequest);
                fetch.params = serde_json::json!({ "url": "https://example.com" });
                fetch
            },
            transform_set("shape", serde_json::json!({ "ok": true })),
        ],
        vec![
            Connection::new("trigger", 0, "fetch"),
            Connection::new("fetch", 0, "shape"),
        ],
    );

    let engine = Engine::new(default_registry());
    let log = MemoryEventLog::new();
    let _ = engine
        .run(
            &workflow,
            vec![serde_json::json!({})],
            ExecutionMode::DryRun,
            &log,
        )
        .await
        .expect("run should succeed");

    let events = log.events();
    for id in ["trigger", "fetch", "shape"] {
        let started = events
            .iter()
            .filter(|e| e.node_id == id && matches!(e.kind, StepKind::Started))
            .count();
        let finished = events
            .iter()
            .filter(|e| e.node_id == id && matches!(e.kind, StepKind::Finished))
            .count();
        assert_eq!(started, 1, "one Started for {id}");
        assert_eq!(finished, 1, "one Finished for {id}");
    }
}

// --- Test 6: McpToolCall real-run path via an injected mock invoker ------------

/// A network-free [`McpInvoker`] returning a canned value, used to exercise the
/// real (`Run` mode) `McpToolCall::execute` path without a live MCP server.
#[derive(Debug)]
struct CannedInvoker {
    value: serde_json::Value,
}

#[async_trait]
impl McpInvoker for CannedInvoker {
    async fn call_tool(
        &self,
        server: &McpServerSpec,
        tool: &str,
        _arguments: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        // Prove the parsed spec and tool name reach the invoker.
        assert!(matches!(server, McpServerSpec::Stdio { .. }));
        assert_eq!(tool, "do");
        Ok(self.value.clone())
    }
}

/// Build a registry that wires `McpToolCall` to the given invoker (plus the
/// trigger needed to seed a run).
fn registry_with_invoker(node: McpToolCall) -> NodeRegistry {
    NodeRegistry::new()
        .with(NodeKind::WebhookTrigger, Arc::new(a2w_nodes::WebhookTrigger))
        .with(NodeKind::McpToolCall, Arc::new(node))
}

/// An `mcp_tool_call` node with the standard `{server, tool, arguments}` params.
fn mcp_node(id: &str) -> Node {
    let mut n = Node::new(id, NodeKind::McpToolCall);
    n.params = serde_json::json!({
        "server": { "transport": "stdio", "command": "x" },
        "tool": "do",
        "arguments": {},
    });
    n
}

#[tokio::test]
async fn mcp_tool_call_real_run_carries_canned_result() {
    let invoker = CannedInvoker {
        value: serde_json::json!({ "answer": 42 }),
    };
    let node = McpToolCall::new(Arc::new(invoker));

    // webhook_trigger -> mcp_tool_call
    let workflow = wf(
        vec![Node::new("trigger", NodeKind::WebhookTrigger), mcp_node("call")],
        vec![Connection::new("trigger", 0, "call")],
    );

    let engine = Engine::new(registry_with_invoker(node));
    let log = MemoryEventLog::new();
    let result = engine
        .run(
            &workflow,
            vec![serde_json::json!({ "seed": true })],
            ExecutionMode::Run, // real run: execute() must call the invoker
            &log,
        )
        .await
        .expect("run should succeed");

    assert_eq!(result.status, RunStatus::Completed);

    let out = result.node_outputs.get("call").expect("call output present");
    assert_eq!(out.len(), 1, "one output item per trigger item");
    assert_eq!(out[0].json["tool"], serde_json::json!("do"));
    assert_eq!(out[0].json["result"], serde_json::json!({ "answer": 42 }));

    // Lineage points at the mcp node.
    match &out[0].source {
        ItemSource::Produced { node_id, .. } => assert_eq!(node_id, "call"),
        other => panic!("expected Produced lineage, got {other:?}"),
    }
}

#[tokio::test]
async fn mcp_tool_call_missing_tool_surfaces_bad_params_cleanly() {
    // Missing `tool` under the default Stop policy => the run fails with a
    // NodeError(BadParams) wrapped in an engine Node error, no panic.
    let node = McpToolCall::new(Arc::new(CannedInvoker {
        value: serde_json::Value::Null,
    }));

    let mut call = Node::new("call", NodeKind::McpToolCall);
    call.params = serde_json::json!({ "server": { "transport": "stdio", "command": "x" } });

    let workflow = wf(
        vec![Node::new("trigger", NodeKind::WebhookTrigger), call],
        vec![Connection::new("trigger", 0, "call")],
    );

    let engine = Engine::new(registry_with_invoker(node));
    let log = MemoryEventLog::new();
    let err = engine
        .run(
            &workflow,
            vec![serde_json::json!({})],
            ExecutionMode::Run,
            &log,
        )
        .await
        .expect_err("missing tool should fail the run");

    // The failure mentions bad params and there is a Failed event for the node.
    let msg = err.to_string();
    assert!(
        msg.contains("bad params") || msg.contains("tool"),
        "expected a bad-params message, got: {msg}"
    );
    assert!(
        log.events()
            .iter()
            .any(|e| e.node_id == "call" && matches!(e.kind, StepKind::Failed)),
        "a Failed event should be recorded for the mcp node"
    );
}

#[tokio::test]
async fn mcp_tool_call_dry_run_is_mocked_and_does_not_invoke() {
    // default() uses the real RmcpInvoker; a dry run must NOT spawn anything.
    let workflow = wf(
        vec![Node::new("trigger", NodeKind::WebhookTrigger), mcp_node("call")],
        vec![Connection::new("trigger", 0, "call")],
    );

    let engine = Engine::new(default_registry());
    let log = MemoryEventLog::new();
    let result = engine
        .run(
            &workflow,
            vec![serde_json::json!({})],
            ExecutionMode::DryRun,
            &log,
        )
        .await
        .expect("dry run should succeed without contacting a server");

    assert_eq!(result.status, RunStatus::Completed);
    let out = result.node_outputs.get("call").expect("call output present");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].json["_mock"], serde_json::json!(true));
    assert_eq!(out[0].json["tool"], serde_json::json!("do"));
}

// --- Test 7 (live, ignored): RmcpInvoker against the real a2w-mcp server ------
//
// Spawns the built `a2w-mcp` stdio server as a child process and calls
// `wf_describe_nodes` (a no-argument tool) via the real `RmcpInvoker`, asserting
// the node taxonomy comes back. Ignored by default because it depends on the
// `a2w-mcp` binary being built (`cargo build -p a2w-mcp`) at the expected path;
// run with `cargo test -p a2w-nodes -- --ignored mcp_tool_call_live_rmcp`.

#[tokio::test]
#[ignore = "requires the built a2w-mcp binary; run with --ignored"]
async fn mcp_tool_call_live_rmcp_describe_nodes() {
    use a2w_nodes::RmcpInvoker;

    // Resolve the a2w-mcp binary next to this test binary in target/debug.
    let exe = std::env::current_exe().expect("current test exe path");
    // .../target/debug/deps/<test>.exe -> .../target/debug
    let target_debug = exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("target/debug dir")
        .to_path_buf();
    let bin = target_debug.join(if cfg!(windows) {
        "a2w-mcp.exe"
    } else {
        "a2w-mcp"
    });
    assert!(
        bin.exists(),
        "a2w-mcp binary not found at {}; build it with `cargo build -p a2w-mcp`",
        bin.display()
    );

    let spec = McpServerSpec::Stdio {
        command: bin.to_string_lossy().into_owned(),
        args: vec![],
        env: Default::default(),
    };

    let invoker = RmcpInvoker::new();
    let result = invoker
        .call_tool(&spec, "wf_describe_nodes", serde_json::json!({}))
        .await
        .expect("live wf_describe_nodes call should succeed");

    // The tool returns the node taxonomy as a JSON array; one entry must name
    // mcp_tool_call.
    let arr = result.as_array().expect("describe_nodes returns an array");
    assert!(
        arr.iter()
            .any(|e| e.get("name") == Some(&serde_json::json!("mcp_tool_call"))),
        "taxonomy should include mcp_tool_call; got {result}"
    );
}

// --- Test 8: CodeStep real-run path via an injected mock WASM runner -----------

/// A network-/wasm-free [`WasmRunner`] returning a canned JSON byte payload,
/// used to exercise the real (`Run` mode) `CodeStep::execute` path without
/// compiling or running any WebAssembly.
#[derive(Debug)]
struct CannedRunner {
    bytes: Vec<u8>,
}

impl WasmRunner for CannedRunner {
    fn run(&self, _wasm: &[u8], function: &str, input: &[u8]) -> Result<Vec<u8>, CodeError> {
        // Prove the requested function and the serialised input reach the runner.
        assert_eq!(function, "transform");
        assert_eq!(input, serde_json::to_vec(&serde_json::json!({ "seed": true })).unwrap());
        Ok(self.bytes.clone())
    }
}

/// A registry wiring `CodeStep` to the given runner (plus the trigger).
fn registry_with_runner(node: CodeStep) -> NodeRegistry {
    NodeRegistry::new()
        .with(NodeKind::WebhookTrigger, Arc::new(a2w_nodes::WebhookTrigger))
        .with(NodeKind::CodeStep, Arc::new(node))
}

/// A `code_step` node carrying a dummy inline wasm + a `function` name. The
/// base64 is valid base64 (so it "loads" as bytes) but the MockRunner never
/// parses it as wasm.
fn code_node(id: &str) -> Node {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(b"\0asm-dummy");
    let mut n = Node::new(id, NodeKind::CodeStep);
    n.params = serde_json::json!({
        "wasm": { "base64": b64 },
        "function": "transform",
    });
    n
}

#[tokio::test]
async fn code_step_real_run_carries_parsed_wasm_output() {
    let runner = CannedRunner {
        bytes: br#"{"shaped":true,"n":42}"#.to_vec(),
    };
    let node = CodeStep::new(Arc::new(runner));

    // webhook_trigger -> code_step
    let workflow = wf(
        vec![Node::new("trigger", NodeKind::WebhookTrigger), code_node("code")],
        vec![Connection::new("trigger", 0, "code")],
    );

    let engine = Engine::new(registry_with_runner(node));
    let log = MemoryEventLog::new();
    let result = engine
        .run(
            &workflow,
            vec![serde_json::json!({ "seed": true })],
            ExecutionMode::Run, // real run: execute() must call the runner
            &log,
        )
        .await
        .expect("run should succeed");

    assert_eq!(result.status, RunStatus::Completed);

    let out = result.node_outputs.get("code").expect("code output present");
    assert_eq!(out.len(), 1, "one output item per trigger item");
    // The parsed mock JSON is carried through as the item payload.
    assert_eq!(out[0].json, serde_json::json!({ "shaped": true, "n": 42 }));

    // Lineage points at the code node.
    match &out[0].source {
        ItemSource::Produced { node_id, .. } => assert_eq!(node_id, "code"),
        other => panic!("expected Produced lineage, got {other:?}"),
    }
}

#[tokio::test]
async fn code_step_missing_wasm_surfaces_bad_params_cleanly() {
    // Missing `wasm` under the default Stop policy => the run fails with a
    // NodeError(BadParams) wrapped in an engine Node error, no panic.
    let node = CodeStep::new(Arc::new(CannedRunner { bytes: vec![] }));

    let mut code = Node::new("code", NodeKind::CodeStep);
    code.params = serde_json::json!({ "function": "transform" }); // no `wasm`

    let workflow = wf(
        vec![Node::new("trigger", NodeKind::WebhookTrigger), code],
        vec![Connection::new("trigger", 0, "code")],
    );

    let engine = Engine::new(registry_with_runner(node));
    let log = MemoryEventLog::new();
    let err = engine
        .run(&workflow, vec![serde_json::json!({})], ExecutionMode::Run, &log)
        .await
        .expect_err("missing wasm should fail the run");

    let msg = err.to_string();
    assert!(
        msg.contains("bad params") || msg.contains("wasm"),
        "expected a bad-params message, got: {msg}"
    );
    assert!(
        log.events()
            .iter()
            .any(|e| e.node_id == "code" && matches!(e.kind, StepKind::Failed)),
        "a Failed event should be recorded for the code node"
    );
}

#[tokio::test]
async fn code_step_dry_run_is_mocked_and_does_not_run_wasm() {
    // default() uses the real ExtismRunner; a dry run must NOT load or run wasm.
    let workflow = wf(
        vec![Node::new("trigger", NodeKind::WebhookTrigger), code_node("code")],
        vec![Connection::new("trigger", 0, "code")],
    );

    let engine = Engine::new(default_registry());
    let log = MemoryEventLog::new();
    let result = engine
        .run(&workflow, vec![serde_json::json!({})], ExecutionMode::DryRun, &log)
        .await
        .expect("dry run should succeed without running wasm");

    assert_eq!(result.status, RunStatus::Completed);
    let out = result.node_outputs.get("code").expect("code output present");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].json["_mock"], serde_json::json!(true));
    assert_eq!(out[0].json["code_step"], serde_json::json!(true));
    assert_eq!(out[0].json["function"], serde_json::json!("transform"));
}

// --- Test 9 (live): ExtismRunner against a real count_vowels.wasm plugin -------
//
// Loads the canonical extism `count_vowels.wasm` fixture (downloaded into
// tests/fixtures/ via PowerShell from the extism/plugins releases) and calls its
// `count_vowels` export on `b"hello world"` through the REAL `ExtismRunner`,
// proving the extism 1.30 host SDK path works end-to-end inside the sandbox.
//
// The fixture is read at runtime from CARGO_MANIFEST_DIR/tests/fixtures so the
// test self-skips (passes with a printed note) if the file is absent (e.g. on a
// machine without network at fixture-fetch time) rather than failing the suite.

#[tokio::test]
async fn code_step_live_count_vowels_via_extism_runner() {
    use a2w_nodes::ExtismRunner;

    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("count_vowels.wasm");

    if !fixture.exists() {
        eprintln!(
            "skipping live count_vowels test: fixture not found at {} \
             (download it with: Invoke-WebRequest -Uri \
             https://github.com/extism/plugins/releases/latest/download/count_vowels.wasm \
             -OutFile crates/a2w-nodes/tests/fixtures/count_vowels.wasm)",
            fixture.display()
        );
        return;
    }

    let wasm = std::fs::read(&fixture).expect("read count_vowels.wasm fixture");

    // The extism Plugin::call is blocking; run it off the async runtime exactly
    // as the node does.
    let result = tokio::task::spawn_blocking(move || {
        let runner = ExtismRunner::new();
        runner.run(&wasm, "count_vowels", b"hello world")
    })
    .await
    .expect("blocking task joined")
    .expect("count_vowels should run in the extism sandbox");

    let value: serde_json::Value =
        serde_json::from_slice(&result).expect("count_vowels returns JSON");

    // "hello world" has 3 vowels (e, o, o). The canonical plugin returns
    // { "count": N, "total": ..., "vowels": "aeiouAEIOU" }.
    assert_eq!(
        value.get("count"),
        Some(&serde_json::json!(3)),
        "expected vowel count 3 for 'hello world'; got {value}"
    );
}

// --- Port-routed end-to-end: Branch routes items to true/false sinks --------

#[tokio::test]
async fn branch_routes_items_via_ports_end_to_end() {
    // Workflow:
    //   trigger -> br (Branch on /is_alert)
    //     br[0] -> hot  (Transform tagging items as hot)
    //     br[1] -> cold (Transform tagging items as cold)
    //
    // Trigger seeds 3 items; two alerts and one non-alert. The engine should
    // route 2 items to `hot` and 1 to `cold`, with no cross-contamination.
    let mut br = Node::new("br", NodeKind::Branch);
    br.params = serde_json::json!({ "condition": "/is_alert" });

    let nodes = vec![
        Node::new("trigger", NodeKind::WebhookTrigger),
        br,
        transform_set("hot", serde_json::json!({ "label": "hot" })),
        transform_set("cold", serde_json::json!({ "label": "cold" })),
    ];
    let connections = vec![
        Connection::new("trigger", 0, "br"),
        Connection::new("br", 0, "hot"),
        Connection::new("br", 1, "cold"),
    ];
    let w = wf(nodes, connections);

    let engine = Engine::new(default_registry());
    let log = MemoryEventLog::new();
    let r = engine
        .run(
            &w,
            vec![
                serde_json::json!({ "id": 1, "is_alert": true }),
                serde_json::json!({ "id": 2, "is_alert": false }),
                serde_json::json!({ "id": 3, "is_alert": true }),
            ],
            ExecutionMode::Run,
            &log,
        )
        .await
        .expect("branch wf runs");
    assert_eq!(r.status, RunStatus::Completed);

    let hot = r.node_outputs.get("hot").expect("hot outputs");
    let cold = r.node_outputs.get("cold").expect("cold outputs");
    assert_eq!(hot.len(), 2, "two alerts routed to hot: {hot:?}");
    assert_eq!(cold.len(), 1, "one non-alert routed to cold: {cold:?}");
    // Hot items carry the `hot` label (the Transform output merges that in).
    for item in hot {
        assert_eq!(item.json["label"], serde_json::json!("hot"));
    }
    for item in cold {
        assert_eq!(item.json["label"], serde_json::json!("cold"));
    }
}

#[tokio::test]
async fn switch_multi_way_routing_end_to_end() {
    let mut sw = Node::new("sw", NodeKind::Switch);
    sw.params = serde_json::json!({
        "key": "/severity",
        "cases": [
            { "value": "critical", "port": 0 },
            { "value": "warning",  "port": 1 }
        ],
        "default_port": 2
    });

    let nodes = vec![
        Node::new("trigger", NodeKind::WebhookTrigger),
        sw,
        transform_set("crit", serde_json::json!({ "tier": "crit" })),
        transform_set("warn", serde_json::json!({ "tier": "warn" })),
        transform_set("info", serde_json::json!({ "tier": "info" })),
    ];
    let connections = vec![
        Connection::new("trigger", 0, "sw"),
        Connection::new("sw", 0, "crit"),
        Connection::new("sw", 1, "warn"),
        Connection::new("sw", 2, "info"),
    ];
    let w = wf(nodes, connections);

    let engine = Engine::new(default_registry());
    let log = MemoryEventLog::new();
    let r = engine
        .run(
            &w,
            vec![
                serde_json::json!({ "severity": "critical" }),
                serde_json::json!({ "severity": "warning" }),
                serde_json::json!({ "severity": "ok" }),
            ],
            ExecutionMode::Run,
            &log,
        )
        .await
        .expect("switch wf runs");
    assert_eq!(r.status, RunStatus::Completed);
    assert_eq!(r.node_outputs["crit"].len(), 1);
    assert_eq!(r.node_outputs["warn"].len(), 1);
    assert_eq!(r.node_outputs["info"].len(), 1);
}
