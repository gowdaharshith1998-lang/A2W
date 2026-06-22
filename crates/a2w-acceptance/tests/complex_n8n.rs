//! Complex, n8n-style automations built in A2W and run end to end.
//!
//! Four production-shaped workflows — lead routing, order fulfillment, ETL
//! validation, and ticket triage — each combining many node kinds the way a
//! real n8n automation does. Every run is deterministic and zero-token.

use std::collections::BTreeMap;

use a2w_engine::{Engine, ExecutionMode, MemoryEventLog, RunStatus};
use a2w_ir::Workflow;
use a2w_validator::validate;
use serde_json::{json, Value};

const LEAD_ROUTING: &str = include_str!("../../../examples/complex_lead_routing.json");
const ORDER_FULFILLMENT: &str = include_str!("../../../examples/complex_order_fulfillment.json");
const ETL_SYNC: &str = include_str!("../../../examples/complex_etl_sync.json");
const TICKET_TRIAGE: &str = include_str!("../../../examples/complex_ticket_triage.json");
const ETL_LIVE: &str = include_str!("../../../examples/complex_etl_live.json");
const LLM_SUMMARIZE: &str = include_str!("../../../examples/complex_llm_summarize.json");

fn load(src: &str) -> Workflow {
    let wf = Workflow::from_json(src).expect("valid IR");
    let report = validate(&wf);
    assert!(report.is_valid, "{} invalid: {:?}", wf.id, report.findings);
    wf
}

/// Run (DryRun = deterministic, zero-token; side-effecting nodes mocked) and
/// return node-id -> output payloads.
async fn run(wf: &Workflow, input: Vec<Value>) -> (RunStatus, BTreeMap<String, Vec<Value>>) {
    let engine = Engine::new(a2w_nodes::default_registry());
    let log = MemoryEventLog::new();
    let r = engine
        .run(wf, input, ExecutionMode::DryRun, &log)
        .await
        .expect("run completes");
    let map = r
        .node_outputs
        .iter()
        .map(|(k, v)| (k.clone(), v.iter().map(|i| i.json.clone()).collect()))
        .collect();
    (r.status, map)
}

fn observe(map: &BTreeMap<String, Vec<Value>>, node: &str) -> Vec<Value> {
    map.get(node).cloned().unwrap_or_default()
}

#[tokio::test]
async fn complex_lead_routing_scores_and_routes() {
    let wf = load(LEAD_ROUTING);
    let leads = vec![
        json!({ "email": "vp@bigco.com", "company": "BigCo", "employees": 120, "source": "referral", "budget": 50000 }),
        json!({ "email": "mgr@mid.com", "company": "Mid",   "employees": 50,  "source": "webinar",  "budget": 10000 }),
        json!({ "email": "solo@tiny.com", "company": "Tiny", "employees": 3,   "source": "ad",       "budget": 500 }),
    ];
    let (status, map) = run(&wf, leads).await;
    assert_eq!(status, RunStatus::Completed);

    // Scoring: 120*0.5 + 50000/1000 + 20(referral) = 130 -> hot;
    //          50*0.5  + 10000/1000 + 0            = 35  -> warm;
    //          3*0.5   + 500/1000   + 0            = 2.0 -> cold.
    let scored = observe(&map, "classify");
    assert_eq!(scored[0]["lead_score"], json!(130.0));
    assert_eq!(scored[0]["tier"], json!("hot"));
    assert_eq!(scored[1]["tier"], json!("warm"));
    assert_eq!(scored[2]["tier"], json!("cold"));

    // Routing: each tier lands on its action node, with the right SLA.
    let hot = observe(&map, "assign_ae");
    assert_eq!(hot.len(), 1);
    assert_eq!(hot[0]["action"], json!("assign_to_ae"));
    assert_eq!(hot[0]["sla_hours"], json!(1));
    assert_eq!(observe(&map, "nurture").len(), 1);
    assert_eq!(observe(&map, "newsletter").len(), 1);
    // The hot lead also fired the CRM-sync integration (http, mocked here).
    assert_eq!(observe(&map, "crm_sync").len(), 1);
    // The outbox merges all three routed leads.
    assert_eq!(observe(&map, "outbox").len(), 3);
}

#[tokio::test]
async fn complex_order_fulfillment_loops_lines_and_gates_on_value() {
    let wf = load(ORDER_FULFILLMENT);
    let orders = vec![
        json!({ "id": "o1", "payment_status": "paid",    "total": 1500, "items": [ { "sku": "A", "qty": 2, "unit_price": 300 }, { "sku": "B", "qty": 1, "unit_price": 900 } ] }),
        json!({ "id": "o2", "payment_status": "paid",    "total": 500,  "items": [ { "sku": "C", "qty": 5, "unit_price": 100 } ] }),
        json!({ "id": "o3", "payment_status": "pending", "total": 2000, "items": [ { "sku": "D", "qty": 1, "unit_price": 2000 } ] }),
    ];
    let (status, map) = run(&wf, orders).await;
    assert_eq!(status, RunStatus::Completed);

    // Line items fan out and price individually: 600, 900, 500, 2000.
    let lines = observe(&map, "price_line");
    let totals: Vec<&Value> = lines.iter().map(|l| &l["line_total"]).collect();
    assert_eq!(
        totals,
        vec![&json!(600.0), &json!(900.0), &json!(500.0), &json!(2000.0)]
    );

    // o1 paid + high-value(>1000) -> approval (approved in dry-run) -> express ship.
    let hi = observe(&map, "ship_hi");
    assert_eq!(hi.len(), 1);
    assert_eq!(hi[0]["channel"], json!("express"));
    // o2 paid + low-value -> auto standard ship.
    let lo = observe(&map, "ship_lo");
    assert_eq!(lo.len(), 1);
    assert_eq!(lo[0]["auto"], json!(true));
    // o3 unpaid -> held.
    let held = observe(&map, "hold");
    assert_eq!(held.len(), 1);
    assert_eq!(held[0]["reason"], json!("awaiting_payment"));
    // All fulfillment outcomes converge on the merge.
    assert_eq!(observe(&map, "fulfilled").len(), 3);
}

#[tokio::test]
async fn complex_etl_sync_validates_and_splits() {
    let wf = load(ETL_SYNC);
    let batch = vec![
        json!({ "id": 1, "email": "alice@corp.com" }),
        json!({ "id": 2, "email": "bad" }),
        json!({ "id": 3, "email": "carol@x.io" }),
    ];
    let (status, map) = run(&wf, batch).await;
    assert_eq!(status, RunStatus::Completed);

    // Normalization computes a lowercased email and a validity flag.
    let norm = observe(&map, "normalize");
    assert_eq!(norm[0]["email_norm"], json!("alice@corp.com"));
    assert_eq!(norm[0]["is_valid"], json!(true));
    assert_eq!(
        norm[1]["is_valid"],
        json!(false),
        "'bad' is not a valid email"
    );

    // Valid records load; the invalid one is quarantined.
    let loaded = observe(&map, "load");
    assert_eq!(loaded.len(), 2);
    assert!(loaded.iter().all(|r| r["ready"] == json!(true)));
    let quarantined = observe(&map, "quarantine");
    assert_eq!(quarantined.len(), 1);
    assert_eq!(quarantined[0]["reason"], json!("invalid_email"));
    assert_eq!(observe(&map, "sink").len(), 3);
}

#[tokio::test]
async fn complex_ticket_triage_routes_by_severity() {
    let wf = load(TICKET_TRIAGE);
    let tickets = vec![
        json!({ "id": "t1", "severity": "critical", "subject": "site down" }),
        json!({ "id": "t2", "severity": "high",     "subject": "checkout slow" }),
        json!({ "id": "t3", "severity": "normal",   "subject": "how do I export?" }),
        json!({ "id": "t4", "severity": "low",      "subject": "love the product" }),
    ];
    let (status, map) = run(&wf, tickets).await;
    assert_eq!(status, RunStatus::Completed);

    assert_eq!(observe(&map, "page")[0]["action"], json!("page_oncall"));
    assert_eq!(observe(&map, "escalate")[0]["sla_min"], json!(60));
    // The "normal" ticket gets an LLM-drafted reply (mocked, zero-token here).
    let drafted = observe(&map, "draft");
    assert_eq!(drafted.len(), 1);
    assert_eq!(drafted[0]["_mock"], json!(true));
    assert_eq!(
        observe(&map, "autoclose")[0]["action"],
        json!("auto_acknowledge")
    );
    // Every ticket reaches dispatch.
    assert_eq!(observe(&map, "dispatch").len(), 4);
}

/// The live-endpoint ETL (`complex_etl_live.json`) fetches from and POSTs to a
/// real public API, so it is NOT executed in the network-free CI — but its IR
/// must still be statically valid (and actually target the live endpoints).
/// It is run for real, in production mode, against `jsonplaceholder.typicode.com`
/// by the live demo; see `docs/LIVE_PRODUCTION_ETL.md`.
#[test]
fn complex_etl_live_is_statically_valid_real_endpoints() {
    let wf = Workflow::from_json(ETL_LIVE).expect("valid IR");
    let report = validate(&wf);
    assert!(
        report.is_valid,
        "live ETL IR invalid: {:?}",
        report.findings
    );
    assert!(
        ETL_LIVE.contains("https://jsonplaceholder.typicode.com/users")
            && ETL_LIVE.contains("https://jsonplaceholder.typicode.com/posts"),
        "the live ETL must target real fetch + load endpoints"
    );
}

/// The LLM summarizer calls a real model in production (consuming tokens), so it
/// is not executed in CI — but its IR must be statically valid. It is run live
/// against an LLM endpoint by the token demo; see `docs/LIVE_PRODUCTION_ETL.md`.
#[test]
fn complex_llm_summarize_is_statically_valid() {
    let wf = Workflow::from_json(LLM_SUMMARIZE).expect("valid IR");
    assert!(validate(&wf).is_valid, "llm summarize IR invalid");
    assert!(wf.nodes.iter().any(|n| n.kind == a2w_ir::NodeKind::LlmCall));
}

/// Print every node's actual output for all four workflows — the "show me the
/// real results" view (run with `-- --nocapture`).
#[tokio::test]
async fn dump_all_complex_workflow_outputs() {
    let cases: [(&str, &str, Vec<Value>); 4] = [
        (
            "lead_routing",
            LEAD_ROUTING,
            vec![
                json!({ "email": "vp@bigco.com", "employees": 120, "source": "referral", "budget": 50000 }),
                json!({ "email": "mgr@mid.com", "employees": 50, "source": "webinar", "budget": 10000 }),
                json!({ "email": "solo@tiny.com", "employees": 3, "source": "ad", "budget": 500 }),
            ],
        ),
        (
            "order_fulfillment",
            ORDER_FULFILLMENT,
            vec![
                json!({ "id": "o1", "payment_status": "paid", "total": 1500, "items": [ { "sku": "A", "qty": 2, "unit_price": 300 }, { "sku": "B", "qty": 1, "unit_price": 900 } ] }),
                json!({ "id": "o2", "payment_status": "paid", "total": 500, "items": [ { "sku": "C", "qty": 5, "unit_price": 100 } ] }),
                json!({ "id": "o3", "payment_status": "pending", "total": 2000, "items": [ { "sku": "D", "qty": 1, "unit_price": 2000 } ] }),
            ],
        ),
        (
            "etl_sync",
            ETL_SYNC,
            vec![
                json!({ "id": 1, "email": "alice@corp.com" }),
                json!({ "id": 2, "email": "bad" }),
                json!({ "id": 3, "email": "carol@x.io" }),
            ],
        ),
        (
            "ticket_triage",
            TICKET_TRIAGE,
            vec![
                json!({ "id": "t1", "severity": "critical", "subject": "site down" }),
                json!({ "id": "t2", "severity": "high", "subject": "checkout slow" }),
                json!({ "id": "t3", "severity": "normal", "subject": "how do I export?" }),
                json!({ "id": "t4", "severity": "low", "subject": "love the product" }),
            ],
        ),
    ];
    for (name, src, input) in cases {
        let wf = load(src);
        let (status, map) = run(&wf, input).await;
        println!("\n========== {name} ({:?}) ==========", status);
        for (nid, items) in &map {
            println!(
                "  {:<18} [{}] {}",
                nid,
                items.len(),
                serde_json::to_string(items).unwrap()
            );
        }
    }
    println!();
}
