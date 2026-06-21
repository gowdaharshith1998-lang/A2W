//! Spec-derived **semantic** metamorphic relations (F1 outcome evidence).
//!
//! Unlike the engine-invariant relations in [`crate::metamorphic`] (which hold
//! for *any* valid workflow and therefore say nothing about the answer), a
//! semantic relation encodes the workflow's **intent** and is authored from a
//! spec/intent object — never read off the workflow under test. It can
//! therefore catch logic faults engine-invariants structurally cannot, e.g. a
//! workflow that computes `total` from the *wrong* field still satisfies every
//! engine-invariant but violates a scaling relation on the right field.
//!
//! Results are reported under [`CheckCategory::SemanticRelation`] — outcome
//! evidence.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::harness::VerificationHarness;
use crate::report::{CheckCategory, CheckResult};
use crate::VerifyError;

/// Relative tolerance for floating-point relation checks.
const EPS: f64 = 1e-9;

/// A single spec-derived semantic relation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SemanticRelation {
    /// **Multiplicative homomorphism.** Multiplying every input item's
    /// `in_field` (a JSON pointer) by `factor` must multiply the *summed*
    /// output `out_field` by `factor`. Encodes "the output total is
    /// proportional to this input field"; catches arithmetic on the wrong
    /// field. Authored from intent: the field names and factor come from the
    /// spec, not the workflow.
    FieldScaling {
        /// JSON pointer to the input field to scale (must be numeric).
        in_field: String,
        /// JSON pointer to the output field to sum.
        out_field: String,
        /// The multiplier applied to every input `in_field`.
        factor: f64,
        /// The base input the relation is evaluated against.
        base_input: Vec<Value>,
    },
    /// **Append homomorphism.** Appending `passing_extra` items (each intended
    /// to yield `per_item` outputs) must increase the observed output count by
    /// exactly `passing_extra.len() * per_item`. Encodes a filter/map's intent;
    /// catches a predicate that drops against intent.
    AppendAddsOutputs {
        /// The base input.
        base_input: Vec<Value>,
        /// Extra items that, under the intended semantics, all pass.
        passing_extra: Vec<Value>,
        /// Outputs each appended item should produce (usually 1).
        per_item: usize,
    },
    /// **Count conservation.** For a pure map, the observed output count equals
    /// the input count. Catches drop / duplicate.
    CountConservation {
        /// The input to check conservation against.
        input: Vec<Value>,
    },
}

impl SemanticRelation {
    /// The input set(s) this relation evaluates against. Used by the search
    /// layer's disjointness guard to detect a fitness/holdout overlap.
    #[must_use]
    pub fn inputs(&self) -> Vec<Vec<Value>> {
        match self {
            SemanticRelation::FieldScaling { base_input, .. } => vec![base_input.clone()],
            SemanticRelation::AppendAddsOutputs {
                base_input,
                passing_extra,
                ..
            } => {
                vec![base_input.clone(), passing_extra.clone()]
            }
            SemanticRelation::CountConservation { input } => vec![input.clone()],
        }
    }

    /// A stable name for the report.
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            SemanticRelation::FieldScaling {
                in_field,
                out_field,
                factor,
                ..
            } => {
                format!("field_scaling {in_field}*{factor}→Σ{out_field}")
            }
            SemanticRelation::AppendAddsOutputs {
                passing_extra,
                per_item,
                ..
            } => {
                format!("append_adds_outputs +{}*{per_item}", passing_extra.len())
            }
            SemanticRelation::CountConservation { .. } => "count_conservation".to_string(),
        }
    }

    /// Evaluate this relation against `wf` observing `observe_node`.
    ///
    /// # Errors
    /// [`VerifyError`] only if the engine fails to run (a relation that does
    /// not hold is a failing `CheckResult`, not an `Err`).
    pub async fn check(
        &self,
        harness: &VerificationHarness,
        wf: &a2w_ir::Workflow,
        observe_node: &str,
    ) -> Result<CheckResult, VerifyError> {
        match self {
            SemanticRelation::FieldScaling {
                in_field,
                out_field,
                factor,
                base_input,
            } => {
                self.check_scaling(
                    harness,
                    wf,
                    observe_node,
                    in_field,
                    out_field,
                    *factor,
                    base_input,
                )
                .await
            }
            SemanticRelation::AppendAddsOutputs {
                base_input,
                passing_extra,
                per_item,
            } => {
                self.check_append(
                    harness,
                    wf,
                    observe_node,
                    base_input,
                    passing_extra,
                    *per_item,
                )
                .await
            }
            SemanticRelation::CountConservation { input } => {
                self.check_conservation(harness, wf, observe_node, input)
                    .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn check_scaling(
        &self,
        harness: &VerificationHarness,
        wf: &a2w_ir::Workflow,
        observe_node: &str,
        in_field: &str,
        out_field: &str,
        factor: f64,
        base_input: &[Value],
    ) -> Result<CheckResult, VerifyError> {
        let base_out = harness
            .observe(wf, observe_node, base_input.to_vec())
            .await?;
        let scaled_input: Vec<Value> = base_input
            .iter()
            .map(|item| scale_field(item, in_field, factor))
            .collect();
        let scaled_out = harness.observe(wf, observe_node, scaled_input).await?;

        let base_sum = sum_field(&base_out, out_field);
        let scaled_sum = sum_field(&scaled_out, out_field);
        let expected = base_sum * factor;
        let ok = approx_eq(scaled_sum, expected);
        let name = self.name();
        Ok(if ok {
            CheckResult::pass(
                CheckCategory::SemanticRelation,
                name,
                format!(
                    "Σ{out_field} scaled by {factor}: {base_sum} → {scaled_sum} (expected {expected})"
                ),
            )
        } else {
            CheckResult::fail(
                CheckCategory::SemanticRelation,
                name,
                format!(
                    "Σ{out_field} did NOT scale by {factor}: base {base_sum}, scaled {scaled_sum}, \
                     expected {expected} — output likely derives from a different input field"
                ),
            )
        })
    }

    async fn check_append(
        &self,
        harness: &VerificationHarness,
        wf: &a2w_ir::Workflow,
        observe_node: &str,
        base_input: &[Value],
        passing_extra: &[Value],
        per_item: usize,
    ) -> Result<CheckResult, VerifyError> {
        let base_out = harness
            .observe(wf, observe_node, base_input.to_vec())
            .await?;
        let mut combined = base_input.to_vec();
        combined.extend(passing_extra.iter().cloned());
        let combined_out = harness.observe(wf, observe_node, combined).await?;

        let expected = base_out.len() + passing_extra.len() * per_item;
        let actual = combined_out.len();
        let name = self.name();
        Ok(if actual == expected {
            CheckResult::pass(
                CheckCategory::SemanticRelation,
                name,
                format!(
                    "appending {} intended-passing item(s) added exactly {} output(s) ({} → {})",
                    passing_extra.len(),
                    passing_extra.len() * per_item,
                    base_out.len(),
                    actual
                ),
            )
        } else {
            CheckResult::fail(
                CheckCategory::SemanticRelation,
                name,
                format!(
                    "appending {} intended-passing item(s) changed output by {} (expected {}); \
                     the workflow drops or duplicates against intent ({} → {})",
                    passing_extra.len(),
                    actual as i64 - base_out.len() as i64,
                    passing_extra.len() * per_item,
                    base_out.len(),
                    actual
                ),
            )
        })
    }

    async fn check_conservation(
        &self,
        harness: &VerificationHarness,
        wf: &a2w_ir::Workflow,
        observe_node: &str,
        input: &[Value],
    ) -> Result<CheckResult, VerifyError> {
        let out = harness.observe(wf, observe_node, input.to_vec()).await?;
        let name = self.name();
        Ok(if out.len() == input.len() {
            CheckResult::pass(
                CheckCategory::SemanticRelation,
                name,
                format!("count conserved: {} in → {} out", input.len(), out.len()),
            )
        } else {
            CheckResult::fail(
                CheckCategory::SemanticRelation,
                name,
                format!(
                    "count not conserved: {} in → {} out (a pure map should preserve count)",
                    input.len(),
                    out.len()
                ),
            )
        })
    }
}

/// A bundle of semantic relations to evaluate together.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SemanticSuite {
    /// The relations to run.
    pub relations: Vec<SemanticRelation>,
}

impl SemanticSuite {
    /// Construct a suite from a list of relations.
    #[must_use]
    pub fn new(relations: Vec<SemanticRelation>) -> Self {
        Self { relations }
    }

    /// Number of relations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.relations.len()
    }

    /// Whether the suite is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.relations.is_empty()
    }

    /// Run every relation, returning one [`CheckResult`] each.
    ///
    /// # Errors
    /// [`VerifyError`] if the engine fails on any relation.
    pub async fn run(
        &self,
        harness: &VerificationHarness,
        wf: &a2w_ir::Workflow,
        observe_node: &str,
    ) -> Result<Vec<CheckResult>, VerifyError> {
        let mut out = Vec::with_capacity(self.relations.len());
        for r in &self.relations {
            out.push(r.check(harness, wf, observe_node).await?);
        }
        Ok(out)
    }
}

/// Return a copy of `item` with the numeric value at `pointer` multiplied by
/// `factor`. If the pointer is absent or non-numeric, the item is unchanged.
fn scale_field(item: &Value, pointer: &str, factor: f64) -> Value {
    let mut copy = item.clone();
    if let Some(slot) = copy.pointer_mut(pointer) {
        if let Some(n) = slot.as_f64() {
            if let Some(num) = serde_json::Number::from_f64(n * factor) {
                *slot = Value::Number(num);
            }
        }
    }
    copy
}

/// Sum the numeric value at `pointer` across all output items (missing /
/// non-numeric contribute 0).
fn sum_field(items: &[Value], pointer: &str) -> f64 {
    items
        .iter()
        .filter_map(|it| it.pointer(pointer).and_then(Value::as_f64))
        .sum()
}

/// Relative floating-point equality.
fn approx_eq(a: f64, b: f64) -> bool {
    let scale = a.abs().max(b.abs()).max(1.0);
    (a - b).abs() <= EPS * scale
}
