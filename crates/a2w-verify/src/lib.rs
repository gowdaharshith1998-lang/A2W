//! # a2w-verify — the verification & accuracy spine (M3)
//!
//! The differentiator: A2W does not merely report that a workflow *ran*, it
//! produces **calibrated evidence that the outcome is correct**, and does so
//! almost for free by leaning on the engine's deterministic, zero-token
//! execution.
//!
//! Four layers of evidence, weakest assumptions last:
//! 1. **Spec assertions** ([`spec`]) — a small, human-confirmed contract.
//! 2. **Golden fixtures** ([`golden`]) — known input→output regression.
//! 3. **Metamorphic relations** ([`metamorphic`]) — invariants across related
//!    runs, requiring *no oracle*: re-run identity, permutation invariance,
//!    duplication scaling, additivity.
//! 4. **Differential cross-checks** ([`differential`]) — compute the result a
//!    second, independent way and compare.
//!
//! All four feed one [`ConfidenceReport`](report::ConfidenceReport), which
//! enumerates exactly what ran and passed — never a bare "verified".
//!
//! ## Decoupling
//! Nothing in this crate references a workflow *generator*. Assertions, golden
//! outputs, oracles and relations are all authored independently of how the
//! workflow under test was produced — so a single model error cannot hide in
//! both the artifact and its test.

#![forbid(unsafe_code)]

pub mod differential;
pub mod golden;
pub mod harness;
pub mod metamorphic;
pub mod report;
pub mod semantic;
pub mod spec;

use std::collections::BTreeMap;

use serde_json::Value;
use thiserror::Error;

pub use differential::{cross_check_oracle, cross_check_workflows, Oracle};
pub use golden::{GoldenFixture, MatchMode};
pub use harness::VerificationHarness;
pub use metamorphic::MetamorphicSuite;
pub use report::{CheckCategory, CheckResult, ConfidenceReport, Threshold};
pub use semantic::{SemanticRelation, SemanticSuite};
pub use spec::{CountOp, SpecAssertion, WorkflowSpec};

/// Errors raised by the verification harness.
///
/// A metamorphic relation or assertion that *does not hold* is **not** an error
/// — it is a failing [`CheckResult`] in the report. An `Err` means verification
/// could not be performed at all (the engine failed, or the observed node does
/// not exist).
#[derive(Debug, Error)]
pub enum VerifyError {
    /// The engine failed to validate or execute the workflow.
    #[error("engine error during verification: {0}")]
    Engine(String),
    /// The node selected for observation is not present in the workflow.
    #[error("observe node '{0}' is not a node in the workflow")]
    UnknownNode(String),
}

/// Canonicalize a JSON value to a stable string. `serde_json::Map` is a
/// `BTreeMap` (sorted keys) in this workspace's default configuration, so
/// serialization order is deterministic and usable as a multiset key.
fn canonical(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| format!("{v:?}"))
}

/// Build a multiset (canonical-string → count) from a list of values.
#[must_use]
pub fn multiset_of(values: &[Value]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for v in values {
        *m.entry(canonical(v)).or_insert(0) += 1;
    }
    m
}

/// Multiset equality: order-insensitive equality of two value lists.
#[must_use]
pub fn multiset_eq(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len() && multiset_of(a) == multiset_of(b)
}

/// The full verification plan: every layer of evidence to gather for one
/// workflow + observed node. Empty layers are simply not run (and are reported
/// as "NOT CHECKED" so absence of evidence is visible).
#[derive(Default, Clone)]
pub struct VerificationPlan {
    /// The node whose output is treated as "the result".
    pub observe_node: String,
    /// Optional human-confirmed spec (assertions + the input to check them on).
    pub spec: Option<WorkflowSpec>,
    /// Golden input→output fixtures.
    pub golden: Vec<GoldenFixture>,
    /// The engine-invariant metamorphic relations to run (NOT outcome
    /// evidence — they verify the engine's guarantees).
    pub metamorphic: Option<MetamorphicSuite>,
    /// The spec-derived semantic relations to run (outcome evidence).
    pub semantic: Option<SemanticSuite>,
}

impl VerificationPlan {
    /// Construct a plan observing `observe_node` with no checks yet.
    #[must_use]
    pub fn new(observe_node: impl Into<String>) -> Self {
        Self {
            observe_node: observe_node.into(),
            ..Default::default()
        }
    }

    /// Attach a confirmed spec.
    #[must_use]
    pub fn with_spec(mut self, spec: WorkflowSpec) -> Self {
        self.spec = Some(spec);
        self
    }

    /// Attach golden fixtures.
    #[must_use]
    pub fn with_golden(mut self, golden: Vec<GoldenFixture>) -> Self {
        self.golden = golden;
        self
    }

    /// Attach an engine-invariant metamorphic suite.
    #[must_use]
    pub fn with_metamorphic(mut self, suite: MetamorphicSuite) -> Self {
        self.metamorphic = Some(suite);
        self
    }

    /// Attach a spec-derived semantic-relation suite (outcome evidence).
    #[must_use]
    pub fn with_semantic(mut self, suite: SemanticSuite) -> Self {
        self.semantic = Some(suite);
        self
    }
}

/// Run a full verification plan against `wf`, producing a calibrated
/// [`ConfidenceReport`].
///
/// Cross-checks are intentionally *not* part of [`VerificationPlan`] because
/// they need closures / second workflows that don't serialize; run them
/// separately via [`cross_check_oracle`] / [`cross_check_workflows`] and
/// [`ConfidenceReport::push`] the results.
///
/// # Errors
/// [`VerifyError`] if the workflow cannot be run or the observed node is absent.
/// A relation/assertion that does not hold is recorded as a failing check, not
/// an error.
pub async fn verify(
    harness: &VerificationHarness,
    wf: &a2w_ir::Workflow,
    plan: &VerificationPlan,
) -> Result<ConfidenceReport, VerifyError> {
    let mut report = ConfidenceReport::new(wf.id.clone(), plan.observe_node.clone());

    // Fail fast if the observe node is unknown (so empty plans still validate
    // the target exists).
    if !wf.nodes.iter().any(|n| n.id == plan.observe_node) {
        return Err(VerifyError::UnknownNode(plan.observe_node.clone()));
    }

    if let Some(spec) = &plan.spec {
        for result in spec.evaluate(harness, wf, &plan.observe_node).await? {
            report.push(result);
        }
    }

    for fixture in &plan.golden {
        report.push(fixture.check(harness, wf, &plan.observe_node).await?);
    }

    if let Some(suite) = &plan.semantic {
        for result in suite.run(harness, wf, &plan.observe_node).await? {
            report.push(result);
        }
    }

    if let Some(suite) = &plan.metamorphic {
        for result in suite.run(harness, wf, &plan.observe_node).await? {
            report.push(result);
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn multiset_eq_is_order_insensitive() {
        let a = vec![json!({ "x": 1 }), json!({ "x": 2 })];
        let b = vec![json!({ "x": 2 }), json!({ "x": 1 })];
        assert!(multiset_eq(&a, &b));
    }

    #[test]
    fn multiset_eq_respects_counts() {
        let a = vec![json!(1), json!(1), json!(2)];
        let b = vec![json!(1), json!(2), json!(2)];
        assert!(!multiset_eq(&a, &b));
    }
}
