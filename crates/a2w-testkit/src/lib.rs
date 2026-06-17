//! # a2w-testkit
//!
//! Declarative **test cases** for A2W workflows plus a [`run_tests`] runner that
//! evaluates them against the [`a2w_engine`] engine. This backs the
//! `wf_add_test` / `wf_run_tests` agent tools: an agent attaches a list of
//! [`TestCase`]s to a workflow and gets back one [`TestResult`] per case.
//!
//! ## Model
//! A [`TestCase`] pairs a `name`, a `trigger_input` (the root items seeded into
//! the trigger), and an [`Expectation`] about the resulting run. The runner runs
//! each case with a **fresh** [`MemoryEventLog`](a2w_engine::MemoryEventLog) and
//! evaluates the expectation against the [`RunResult`](a2w_engine::RunResult) (or
//! the engine-level error).
//!
//! ## Expectations
//! - [`Expectation::Completes`] — the run reaches
//!   [`RunStatus::Completed`](a2w_engine::RunStatus::Completed).
//! - [`Expectation::Fails`] — the run returns an engine error **or** reaches
//!   `RunStatus::Failed`.
//! - [`Expectation::NodeOutputEquals`] — a node's output items' `.json`, in
//!   order, equal exactly a given list.
//! - [`Expectation::NodeOutputContains`] — at least one of a node's output
//!   items' `.json` is a recursive **superset** of a given object.
//!
//! Any engine-level error (invalid workflow, a node failing under a `Stop`
//! policy, a missing executor) is a **failure** for every expectation *except*
//! [`Expectation::Fails`], for which it is the expected outcome.

#![forbid(unsafe_code)]

use a2w_engine::{Engine, ExecutionMode, MemoryEventLog, RunResult, RunStatus};
use a2w_ir::Workflow;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single declarative test case for a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCase {
    /// Human-readable name, surfaced in the [`TestResult`].
    pub name: String,
    /// The root items seeded into the trigger for this run (one [`Item`] per
    /// value). May be empty.
    ///
    /// [`Item`]: a2w_engine::Item
    pub trigger_input: Vec<Value>,
    /// What this case asserts about the run.
    pub expect: Expectation,
}

/// An assertion about a workflow run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Expectation {
    /// The run reaches `RunStatus::Completed` (no engine error).
    Completes,
    /// The run returns an engine error or reaches `RunStatus::Failed`.
    Fails,
    /// The named node's output items' `.json`, in order, equal exactly `json`.
    NodeOutputEquals {
        /// The node whose output is inspected.
        node_id: String,
        /// The exact, ordered list of expected `.json` payloads.
        json: Vec<Value>,
    },
    /// At least one of the named node's output items' `.json` is a recursive
    /// superset of `json` (every key/value in `json` is present, recursively).
    NodeOutputContains {
        /// The node whose output is inspected.
        node_id: String,
        /// The object every key/value of which must appear in some output item.
        json: Value,
    },
}

/// The outcome of evaluating one [`TestCase`].
#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    /// The case's name (copied from [`TestCase::name`]).
    pub name: String,
    /// Whether the expectation held.
    pub passed: bool,
    /// A human- and machine-readable explanation. On failure this includes a
    /// clear expected-vs-actual diff.
    pub detail: String,
}

/// Run each case against `wf` and return one [`TestResult`] per case, in order.
///
/// Every case runs with a **fresh** [`MemoryEventLog`] so event histories don't
/// bleed between cases. A run that errors at the engine level (invalid workflow,
/// a node failing under `Stop`, a missing executor) is a failure for every
/// expectation except [`Expectation::Fails`].
pub async fn run_tests(
    engine: &Engine,
    wf: &Workflow,
    cases: &[TestCase],
    mode: ExecutionMode,
) -> Vec<TestResult> {
    let mut results = Vec::with_capacity(cases.len());
    for case in cases {
        let log = MemoryEventLog::new();
        let outcome = engine
            .run(wf, case.trigger_input.clone(), mode, &log)
            .await;
        results.push(evaluate(case, &outcome));
    }
    results
}

/// Evaluate a single case against the engine outcome.
fn evaluate(case: &TestCase, outcome: &Result<RunResult, a2w_engine::EngineError>) -> TestResult {
    let (passed, detail) = match (&case.expect, outcome) {
        // --- Completes ----------------------------------------------------
        (Expectation::Completes, Ok(run)) if run.status == RunStatus::Completed => {
            (true, "run completed".to_string())
        }
        (Expectation::Completes, Ok(run)) => (
            false,
            format!(
                "expected the run to complete, but status was {:?}",
                run.status
            ),
        ),
        (Expectation::Completes, Err(err)) => (
            false,
            format!("expected the run to complete, but it errored: {err}"),
        ),

        // --- Fails --------------------------------------------------------
        (Expectation::Fails, Err(err)) => {
            (true, format!("run failed as expected: {err}"))
        }
        (Expectation::Fails, Ok(run)) if run.status == RunStatus::Failed => {
            (true, "run reached status Failed as expected".to_string())
        }
        (Expectation::Fails, Ok(run)) => (
            false,
            format!(
                "expected the run to fail, but it completed with status {:?}",
                run.status
            ),
        ),

        // --- NodeOutputEquals --------------------------------------------
        (Expectation::NodeOutputEquals { node_id, json }, Ok(run)) => {
            eval_node_output_equals(node_id, json, run)
        }
        (Expectation::NodeOutputEquals { node_id, .. }, Err(err)) => (
            false,
            format!(
                "expected node '{node_id}' to produce output, but the run errored: {err}"
            ),
        ),

        // --- NodeOutputContains ------------------------------------------
        (Expectation::NodeOutputContains { node_id, json }, Ok(run)) => {
            eval_node_output_contains(node_id, json, run)
        }
        (Expectation::NodeOutputContains { node_id, .. }, Err(err)) => (
            false,
            format!(
                "expected node '{node_id}' to produce output, but the run errored: {err}"
            ),
        ),
    };

    TestResult {
        name: case.name.clone(),
        passed,
        detail,
    }
}

/// Evaluate [`Expectation::NodeOutputEquals`] against a successful run.
fn eval_node_output_equals(node_id: &str, expected: &[Value], run: &RunResult) -> (bool, String) {
    let Some(items) = run.node_outputs.get(node_id) else {
        return (
            false,
            format!(
                "node '{node_id}' produced no output (it is not in node_outputs); \
                 expected {} item(s): {}",
                expected.len(),
                compact_list(expected)
            ),
        );
    };

    let actual: Vec<&Value> = items.iter().map(|item| &item.json).collect();

    if actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected.iter())
            .all(|(a, e)| *a == e)
    {
        return (
            true,
            format!("node '{node_id}' output matched ({} item(s))", expected.len()),
        );
    }

    let actual_owned: Vec<Value> = actual.into_iter().cloned().collect();
    (
        false,
        format!(
            "node '{node_id}' output mismatch:\n  expected ({} item(s)): {}\n  actual   ({} item(s)): {}",
            expected.len(),
            compact_list(expected),
            actual_owned.len(),
            compact_list(&actual_owned)
        ),
    )
}

/// Evaluate [`Expectation::NodeOutputContains`] against a successful run.
fn eval_node_output_contains(node_id: &str, subset: &Value, run: &RunResult) -> (bool, String) {
    let Some(items) = run.node_outputs.get(node_id) else {
        return (
            false,
            format!(
                "node '{node_id}' produced no output (it is not in node_outputs); \
                 expected some item to contain: {}",
                compact(subset)
            ),
        );
    };

    if items.iter().any(|item| json_is_subset(subset, &item.json)) {
        return (
            true,
            format!(
                "node '{node_id}' has an output item containing {}",
                compact(subset)
            ),
        );
    }

    let actual: Vec<Value> = items.iter().map(|item| item.json.clone()).collect();
    (
        false,
        format!(
            "node '{node_id}': no output item contained the expected subset:\n  \
             expected subset: {}\n  actual items ({}): {}",
            compact(subset),
            actual.len(),
            compact_list(&actual)
        ),
    )
}

/// Recursive subset / containment check.
///
/// - **Objects**: `subset` matches `of` iff every key in `subset` is present in
///   `of` and its value recursively subset-matches.
/// - **Arrays** and **scalars** (`null`, bool, number, string): match by exact
///   equality.
///
/// This makes `{ "a": 1 }` a subset of `{ "a": 1, "b": 2 }`, while arrays must
/// match element-for-element.
#[must_use]
pub fn json_is_subset(subset: &Value, of: &Value) -> bool {
    match (subset, of) {
        (Value::Object(sub_map), Value::Object(of_map)) => sub_map.iter().all(|(k, sub_v)| {
            of_map
                .get(k)
                .is_some_and(|of_v| json_is_subset(sub_v, of_v))
        }),
        // Arrays and scalars must match exactly (object-only is the recursion).
        (a, b) => a == b,
    }
}

/// Render a value as compact JSON, falling back to a debug form on the
/// (practically impossible) serialization error so detail strings never panic.
fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| format!("{v:?}"))
}

/// Render a list of values as a compact JSON array.
fn compact_list(vs: &[Value]) -> String {
    let arr = Value::Array(vs.to_vec());
    compact(&arr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2w_ir::{Connection, Node, NodeKind, Workflow, SCHEMA_VERSION};
    use serde_json::json;

    /// `webhook -> shape(Transform set:{tag:"x"})`. Pure, no network.
    fn shaping_workflow() -> Workflow {
        let trigger = Node::new("trigger", NodeKind::WebhookTrigger);
        let mut shape = Node::new("shape", NodeKind::Transform);
        shape.params = json!({ "set": { "tag": "x" } });
        Workflow {
            schema_version: SCHEMA_VERSION,
            id: "wf_testkit".to_string(),
            name: "testkit shaping".to_string(),
            nodes: vec![trigger, shape],
            connections: vec![Connection::new("trigger", 0, "shape")],
        }
    }

    fn engine() -> Engine {
        Engine::new(a2w_nodes::default_registry())
    }

    #[tokio::test]
    async fn contains_passes() {
        let wf = shaping_workflow();
        let cases = vec![TestCase {
            name: "shape adds tag".to_string(),
            trigger_input: vec![json!({ "id": 1, "extra": true })],
            // Subset: only requires tag=x and id=1; the item also has `extra`.
            expect: Expectation::NodeOutputContains {
                node_id: "shape".to_string(),
                json: json!({ "tag": "x", "id": 1 }),
            },
        }];
        let results = run_tests(&engine(), &wf, &cases, ExecutionMode::Run).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].passed, "detail: {}", results[0].detail);
    }

    #[tokio::test]
    async fn completes_passes() {
        let wf = shaping_workflow();
        let cases = vec![TestCase {
            name: "run completes".to_string(),
            trigger_input: vec![json!({ "id": 1 })],
            expect: Expectation::Completes,
        }];
        let results = run_tests(&engine(), &wf, &cases, ExecutionMode::Run).await;
        assert!(results[0].passed, "detail: {}", results[0].detail);
        assert!(results[0].detail.contains("completed"));
    }

    #[tokio::test]
    async fn equals_failing_shows_diff() {
        let wf = shaping_workflow();
        let cases = vec![TestCase {
            name: "wrong expected output".to_string(),
            trigger_input: vec![json!({ "id": 1 })],
            // Deliberately wrong: actual is {id:1, tag:"x"}, not {id:999}.
            expect: Expectation::NodeOutputEquals {
                node_id: "shape".to_string(),
                json: vec![json!({ "id": 999 })],
            },
        }];
        let results = run_tests(&engine(), &wf, &cases, ExecutionMode::Run).await;
        assert!(!results[0].passed);
        // The detail must surface both sides of the mismatch.
        assert!(results[0].detail.contains("mismatch"), "{}", results[0].detail);
        assert!(results[0].detail.contains("expected"), "{}", results[0].detail);
        assert!(results[0].detail.contains("actual"), "{}", results[0].detail);
        assert!(results[0].detail.contains("999"), "{}", results[0].detail);
        assert!(results[0].detail.contains("\"tag\":\"x\""), "{}", results[0].detail);
    }

    #[tokio::test]
    async fn fails_expectation_against_invalid_workflow() {
        // No trigger -> the engine validates and returns EngineError::Invalid.
        let bad = Workflow {
            schema_version: SCHEMA_VERSION,
            id: "wf_bad".to_string(),
            name: "no trigger".to_string(),
            nodes: vec![Node::new("a", NodeKind::Transform)],
            connections: vec![],
        };
        let cases = vec![TestCase {
            name: "invalid workflow fails".to_string(),
            trigger_input: vec![],
            expect: Expectation::Fails,
        }];
        let results = run_tests(&engine(), &bad, &cases, ExecutionMode::Run).await;
        assert!(results[0].passed, "detail: {}", results[0].detail);
        assert!(results[0].detail.contains("failed as expected"), "{}", results[0].detail);
    }

    #[test]
    fn subset_matches_recursively() {
        let of = json!({ "a": 1, "b": { "c": 2, "d": 3 }, "arr": [1, 2] });
        assert!(json_is_subset(&json!({ "a": 1 }), &of));
        assert!(json_is_subset(&json!({ "b": { "c": 2 } }), &of));
        assert!(json_is_subset(&json!({ "arr": [1, 2] }), &of));
        // Missing key.
        assert!(!json_is_subset(&json!({ "z": 9 }), &of));
        // Wrong nested value.
        assert!(!json_is_subset(&json!({ "b": { "c": 99 } }), &of));
        // Array must match exactly, not as a subset.
        assert!(!json_is_subset(&json!({ "arr": [1] }), &of));
    }

    #[test]
    fn test_case_round_trips_through_json() {
        let case = TestCase {
            name: "rt".to_string(),
            trigger_input: vec![json!({ "x": 1 })],
            expect: Expectation::NodeOutputContains {
                node_id: "n".to_string(),
                json: json!({ "ok": true }),
            },
        };
        let s = serde_json::to_string(&case).expect("serialize");
        let back: TestCase = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back.name, case.name);
        assert!(matches!(back.expect, Expectation::NodeOutputContains { .. }));
    }
}
