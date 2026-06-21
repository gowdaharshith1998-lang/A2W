//! The assertion / spec layer (M3a).
//!
//! Trust relocates to a **small, human-checkable spec**: a declarative list of
//! assertions an agent co-authors and a user confirms. The spec is plain
//! serializable data with no reference to how the workflow was generated, so it
//! stays decoupled from the generator — the same model error cannot appear in
//! both the workflow and the assertions checking it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::harness::VerificationHarness;
use crate::report::{CheckCategory, CheckResult};
use crate::VerifyError;

/// A comparison operator for scalar assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CountOp {
    /// Equal.
    Eq,
    /// Greater than or equal.
    Ge,
    /// Less than or equal.
    Le,
}

impl CountOp {
    fn eval(self, actual: usize, bound: usize) -> bool {
        match self {
            CountOp::Eq => actual == bound,
            CountOp::Ge => actual >= bound,
            CountOp::Le => actual <= bound,
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            CountOp::Eq => "==",
            CountOp::Ge => ">=",
            CountOp::Le => "<=",
        }
    }
}

/// A single assertion about the observed node's output for a given input.
///
/// Kept intentionally small and inspectable — an agent emits these and a human
/// reads them at a glance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpecAssertion {
    /// The number of output items satisfies `op` against `count`.
    OutputCount {
        /// Comparison operator.
        op: CountOp,
        /// The bound.
        count: usize,
    },
    /// Every output item has a value at `path` (a JSON pointer).
    EveryItemHasField {
        /// JSON pointer into each item.
        path: String,
    },
    /// Every output item's value at `path` equals `value`.
    EveryItemFieldEquals {
        /// JSON pointer into each item.
        path: String,
        /// The required value.
        value: Value,
    },
    /// At least one output item's value at `path` equals `value`.
    SomeItemFieldEquals {
        /// JSON pointer into each item.
        path: String,
        /// The value some item must have.
        value: Value,
    },
    /// No output item's value at `path` equals `value` (a negative assertion).
    NoItemFieldEquals {
        /// JSON pointer into each item.
        path: String,
        /// The forbidden value.
        value: Value,
    },
}

impl SpecAssertion {
    /// A stable name for this assertion, used in the report.
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            SpecAssertion::OutputCount { op, count } => {
                format!("output_count {} {count}", op.symbol())
            }
            SpecAssertion::EveryItemHasField { path } => format!("every_item_has '{path}'"),
            SpecAssertion::EveryItemFieldEquals { path, .. } => {
                format!("every_item_eq '{path}'")
            }
            SpecAssertion::SomeItemFieldEquals { path, .. } => format!("some_item_eq '{path}'"),
            SpecAssertion::NoItemFieldEquals { path, .. } => format!("no_item_eq '{path}'"),
        }
    }

    /// Evaluate this assertion against an observed output list.
    #[must_use]
    pub fn evaluate(&self, output: &[Value]) -> CheckResult {
        let (passed, detail) = match self {
            SpecAssertion::OutputCount { op, count } => {
                let actual = output.len();
                (
                    op.eval(actual, *count),
                    format!("actual count {actual} {} {count}", op.symbol()),
                )
            }
            SpecAssertion::EveryItemHasField { path } => {
                let missing = output
                    .iter()
                    .filter(|item| item.pointer(path).is_none())
                    .count();
                (
                    missing == 0,
                    format!("{missing}/{} item(s) missing '{path}'", output.len()),
                )
            }
            SpecAssertion::EveryItemFieldEquals { path, value } => {
                let bad = output
                    .iter()
                    .filter(|item| item.pointer(path) != Some(value))
                    .count();
                (
                    bad == 0,
                    format!(
                        "{bad}/{} item(s) had '{path}' != expected value",
                        output.len()
                    ),
                )
            }
            SpecAssertion::SomeItemFieldEquals { path, value } => {
                let any = output.iter().any(|item| item.pointer(path) == Some(value));
                (
                    any,
                    if any {
                        format!("at least one item has '{path}' == expected")
                    } else {
                        format!("no item had '{path}' == expected ({} item(s))", output.len())
                    },
                )
            }
            SpecAssertion::NoItemFieldEquals { path, value } => {
                let hits = output
                    .iter()
                    .filter(|item| item.pointer(path) == Some(value))
                    .count();
                (
                    hits == 0,
                    format!("{hits} item(s) violated 'no '{path}' == forbidden value'"),
                )
            }
        };
        if passed {
            CheckResult::pass(CheckCategory::Spec, self.name(), detail)
        } else {
            CheckResult::fail(CheckCategory::Spec, self.name(), detail)
        }
    }
}

/// A confirmed spec: the assertions plus the single input they are evaluated
/// against. The agent proposes it; the user confirms it; M3 checks it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSpec {
    /// The input seeded into the trigger when evaluating the assertions.
    pub input: Vec<Value>,
    /// The assertions to check against the observed output.
    pub assertions: Vec<SpecAssertion>,
}

impl WorkflowSpec {
    /// Evaluate every assertion against a single run of `wf` observing
    /// `observe_node`.
    ///
    /// # Errors
    /// [`VerifyError`] only if the run itself fails.
    pub async fn evaluate(
        &self,
        harness: &VerificationHarness,
        wf: &a2w_ir::Workflow,
        observe_node: &str,
    ) -> Result<Vec<CheckResult>, VerifyError> {
        let output = harness
            .observe(wf, observe_node, self.input.clone())
            .await?;
        Ok(self
            .assertions
            .iter()
            .map(|a| a.evaluate(&output))
            .collect())
    }
}
