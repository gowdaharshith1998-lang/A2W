//! Full-capability surface sweep: exercises the parts of A2W beyond the core
//! engine — importing, authoring, optimizing, templating, and persistence —
//! end to end and hermetically (zero network, zero LLM tokens via `MockLlm`).
//!
//!   * **import**  — an n8n export translates to a valid, runnable A2W workflow.
//!   * **openapi** — an OpenAPI 3 spec generates a callable integration manifest.
//!   * **templates** — every golden template validates and dry-runs.
//!   * **author**  — the Generate→Validate→Repair loop produces a valid workflow
//!     (and *repairs* a bad first attempt) driven by a deterministic MockLlm.
//!   * **optimizer** — analyze finds a Parallelize rewrite; apply keeps the
//!     workflow valid and result-equivalent.
//!   * **store**   — workflow + skill records round-trip through sqlite.

use a2w_engine::{Engine, ExecutionMode, MemoryEventLog, RunStatus};
use a2w_ir::Workflow;
use a2w_validator::validate;
use serde_json::json;

async fn dry_run_ok(wf: &Workflow) -> RunStatus {
    let engine = Engine::new(a2w_nodes::default_registry());
    let log = MemoryEventLog::new();
    engine
        .run(wf, vec![json!({ "probe": 1 })], ExecutionMode::DryRun, &log)
        .await
        .expect("dry run completes")
        .status
}

#[tokio::test]
async fn n8n_import_produces_a_runnable_a2w_workflow() {
    // A clean webhook -> httpRequest -> set n8n export.
    let n8n = json!({
        "name": "demo",
        "nodes": [
            { "name": "Webhook", "type": "n8n-nodes-base.webhook", "parameters": {} },
            { "name": "HTTP Request", "type": "n8n-nodes-base.httpRequest",
              "parameters": { "url": "https://example.com/api", "method": "GET" } },
            { "name": "Set", "type": "n8n-nodes-base.set",
              "parameters": { "values": { "string": [ { "name": "greeting", "value": "hello" } ] } } }
        ],
        "connections": {
            "Webhook": { "main": [ [ { "node": "HTTP Request", "type": "main", "index": 0 } ] ] },
            "HTTP Request": { "main": [ [ { "node": "Set", "type": "main", "index": 0 } ] ] }
        }
    })
    .to_string();

    let result = a2w_import::import_n8n(&n8n).expect("n8n import succeeds");
    let report = validate(&result.workflow);
    assert!(
        report.is_valid,
        "imported workflow must validate: {:?}",
        report.findings
    );
    assert_eq!(dry_run_ok(&result.workflow).await, RunStatus::Completed);
    assert_eq!(result.workflow.nodes.len(), 3, "all three nodes mapped");
}

#[test]
fn openapi_spec_generates_callable_actions() {
    let spec = json!({
        "openapi": "3.0.0",
        "info": { "title": "Petstore", "version": "1.0" },
        "servers": [ { "url": "https://api.example.com" } ],
        "paths": {
            "/pets": {
                "get":  { "operationId": "listPets",  "summary": "List pets" },
                "post": { "operationId": "createPet", "summary": "Create a pet" }
            },
            "/pets/{id}": {
                "get": { "operationId": "getPet", "summary": "Get one pet" }
            }
        }
    })
    .to_string();

    let gen = a2w_openapi::generate(&spec).expect("openapi generate succeeds");
    assert_eq!(gen.integration.base_url, "https://api.example.com");
    assert_eq!(
        gen.integration.actions.len(),
        3,
        "three operations become actions"
    );
    let names: Vec<&str> = gen
        .integration
        .actions
        .iter()
        .map(|a| a.name.as_str())
        .collect();
    assert!(
        names.contains(&"listPets") && names.contains(&"createPet") && names.contains(&"getPet")
    );
    // Read vs write access is inferred from the HTTP method.
    let create = gen
        .integration
        .actions
        .iter()
        .find(|a| a.name == "createPet")
        .unwrap();
    assert_eq!(create.method, "POST");
}

#[tokio::test]
async fn every_golden_template_validates_and_runs() {
    let templates = a2w_templates::all();
    assert!(templates.len() >= 5, "a non-trivial template corpus");
    for t in &templates {
        let report = validate(&t.workflow);
        assert!(
            report.is_valid,
            "template '{}' must validate: {:?}",
            t.id, report.findings
        );
        assert_eq!(
            dry_run_ok(&t.workflow).await,
            RunStatus::Completed,
            "template '{}' must dry-run",
            t.id
        );
        // get() round-trips by id.
        assert_eq!(a2w_templates::get(&t.id).map(|g| g.id), Some(t.id.clone()));
    }
    // search surfaces relevant templates.
    assert!(
        !a2w_templates::search("http").is_empty() || !a2w_templates::search("webhook").is_empty(),
        "keyword search returns matches"
    );
}

#[tokio::test]
async fn author_loop_generates_and_repairs_with_zero_tokens() {
    use a2w_author::{generate_workflow_from_prompt, AuthorConfig};
    use a2w_llm::MockLlm;

    let good = json!({
        "schema_version": 1, "id": "wf_authored", "name": "authored",
        "nodes": [
            { "id": "t", "kind": "webhook_trigger", "params": {} },
            { "id": "x", "kind": "transform", "params": { "set": { "ok": true } } }
        ],
        "connections": [ { "from_node": "t", "from_port": 0, "to_node": "x" } ]
    })
    .to_string();

    // (a) First-try success.
    let mock = MockLlm::new(vec![good.clone()]);
    let outcome =
        generate_workflow_from_prompt("a webhook that tags items", &mock, &AuthorConfig::default())
            .await
            .expect("mock llm never errors at transport level");
    assert!(
        outcome.success,
        "valid workflow authored: {}",
        outcome.message
    );
    assert!(outcome.workflow.is_some());
    assert_eq!(outcome.iterations.len(), 1, "no repair needed");

    // (b) Repair: a broken first attempt, then a valid one.
    let mock = MockLlm::new(vec!["not json at all".to_string(), good]);
    let repaired = generate_workflow_from_prompt("same task", &mock, &AuthorConfig::default())
        .await
        .expect("transport ok");
    assert!(
        repaired.success,
        "the loop repaired itself: {}",
        repaired.message
    );
    assert_eq!(
        repaired.iterations.len(),
        2,
        "one repair attempt after the bad one"
    );
}

#[tokio::test]
async fn optimizer_finds_and_applies_a_parallelize_rewrite() {
    use a2w_optimizer::{analyze, apply, SuggestionKind};

    // trigger -> a -> b, where b does NOT consume a's output (constant `set`s),
    // so b can run in parallel with a.
    let wf: Workflow = serde_json::from_value(json!({
        "schema_version": 1, "id": "wf_opt", "name": "opt",
        "nodes": [
            { "id": "t", "kind": "webhook_trigger", "params": {} },
            { "id": "a", "kind": "transform", "params": { "set": { "a": 1 } } },
            { "id": "b", "kind": "transform", "params": { "set": { "b": 2 } } }
        ],
        "connections": [
            { "from_node": "t", "from_port": 0, "to_node": "a" },
            { "from_node": "a", "from_port": 0, "to_node": "b" }
        ]
    }))
    .unwrap();
    assert!(validate(&wf).is_valid);

    let suggestions = analyze(&wf, None);
    let parallelize = suggestions
        .iter()
        .find(|s| s.kind == SuggestionKind::Parallelize)
        .expect("a parallelize opportunity is found");
    let optimized = apply(&wf, &parallelize.ops);

    // The rewrite preserves validity and the observable result of `b`.
    assert!(
        validate(&optimized).is_valid,
        "optimized workflow stays valid"
    );
    let observe = |w: &Workflow| {
        let w = w.clone();
        async move {
            let engine = Engine::new(a2w_nodes::default_registry());
            let log = MemoryEventLog::new();
            let run = engine
                .run(&w, vec![json!({ "seed": 1 })], ExecutionMode::Run, &log)
                .await
                .expect("run");
            run.node_outputs.get("b").map(|v| v.len()).unwrap_or(0)
        }
    };
    assert_eq!(
        observe(&wf).await,
        observe(&optimized).await,
        "result-equivalent"
    );
}

#[tokio::test]
async fn store_round_trips_workflows_and_skills() {
    use a2w_store::{SkillRecord, Store};

    let store = Store::connect("sqlite::memory:")
        .await
        .expect("open in-memory db");

    let wf: Workflow = serde_json::from_value(json!({
        "schema_version": 1, "id": "wf_persist", "name": "persist",
        "nodes": [
            { "id": "t", "kind": "webhook_trigger", "params": {} },
            { "id": "calc", "kind": "transform", "params": { "set": { "total": "${{ $.x + 1 }}" } } }
        ],
        "connections": [ { "from_node": "t", "from_port": 0, "to_node": "calc" } ]
    }))
    .unwrap();

    store.save_workflow(&wf).await.expect("save workflow");
    let got = store
        .get_workflow("wf_persist")
        .await
        .expect("query")
        .expect("present");
    assert_eq!(got.id, "wf_persist");
    assert_eq!(got.nodes.len(), 2);

    let rec = SkillRecord {
        id: "skill_cap".into(),
        query: "increment x".into(),
        observe_node: "calc".into(),
        workflow_json: serde_json::to_string(&wf).unwrap(),
        signature_json: "{}".into(),
        evidence_json: "{}".into(),
        holdout_score: 1.0,
    };
    store.save_skill(&rec).await.expect("save skill");
    let skill = store
        .get_skill("skill_cap")
        .await
        .expect("query")
        .expect("present");
    assert_eq!(skill.holdout_score, 1.0);
    assert_eq!(store.list_skills().await.expect("list").len(), 1);
}
