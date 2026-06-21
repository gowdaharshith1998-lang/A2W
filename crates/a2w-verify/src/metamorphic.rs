//! Metamorphic relations: correctness evidence **without an oracle**.
//!
//! A metamorphic relation checks an invariant *across related runs* of the same
//! workflow, so it needs no known-correct output. These are the heart of M3 —
//! they run cheaply (zero-token, deterministic) and catch the bugs an LLM
//! workflow generator actually makes: dropped items, cross-item contamination,
//! order dependence, off-by-one fan-out.
//!
//! Each relation is independent of how the workflow was generated, so the same
//! model error cannot hide in both the workflow and its test.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::harness::VerificationHarness;
use crate::report::{CheckCategory, CheckResult};
use crate::{multiset_eq, multiset_of, VerifyError};

/// Which metamorphic relations to run, with the seed inputs they need.
///
/// Build with [`MetamorphicSuite::standard`] for the relations that hold for any
/// deterministic per-item pipeline, then add or remove relations to match the
/// workflow's class (e.g. drop [`MetamorphicSuite::additivity`] for a workflow
/// that genuinely aggregates across items in an order-sensitive way).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MetamorphicSuite {
    /// Seed input used by single-input relations (rerun, permutation, scaling).
    pub seed: Vec<Value>,
    /// Run re-run identity (determinism). Always safe.
    pub rerun_identity: bool,
    /// Run permutation invariance (output multiset unchanged when inputs are
    /// reordered). Holds for per-item maps/filters and order-insensitive
    /// aggregations.
    pub permutation_invariance: bool,
    /// Run duplication scaling with this factor (`output_count(k×input) ==
    /// k × output_count(input)`). `0` disables. Holds for per-item pipelines.
    pub duplication_factor: usize,
    /// Run additivity: `output(a ++ b) == output(a) ⊎ output(b)` as multisets.
    /// The strongest no-cross-contamination signal for per-item pipelines.
    /// When set, the seed is split in half to form `a` and `b`.
    pub additivity: bool,
}

impl MetamorphicSuite {
    /// The standard suite for a deterministic per-item pipeline: re-run
    /// identity, permutation invariance, ×2 and ×3 duplication scaling, and
    /// additivity. Needs a seed with ≥2 items for additivity to be meaningful.
    #[must_use]
    pub fn standard(seed: Vec<Value>) -> Self {
        Self {
            seed,
            rerun_identity: true,
            permutation_invariance: true,
            duplication_factor: 3,
            additivity: true,
        }
    }

    /// Just the always-safe relation (determinism). Useful when a workflow is
    /// an order-sensitive aggregation and only re-run identity is guaranteed.
    #[must_use]
    pub fn determinism_only(seed: Vec<Value>) -> Self {
        Self {
            seed,
            rerun_identity: true,
            permutation_invariance: false,
            duplication_factor: 0,
            additivity: false,
        }
    }

    /// Number of relations this suite will run (for capacity / reporting).
    #[must_use]
    pub fn relation_count(&self) -> usize {
        usize::from(self.rerun_identity)
            + usize::from(self.permutation_invariance)
            + usize::from(self.duplication_factor >= 2)
            + usize::from(self.additivity)
    }

    /// Run every enabled relation against `wf` observing `observe_node`,
    /// returning one [`CheckResult`] per relation.
    ///
    /// # Errors
    /// [`VerifyError`] only if the engine itself fails to run (a *relation*
    /// that does not hold is a failing `CheckResult`, not an `Err`).
    pub async fn run(
        &self,
        harness: &VerificationHarness,
        wf: &a2w_ir::Workflow,
        observe_node: &str,
    ) -> Result<Vec<CheckResult>, VerifyError> {
        let mut results = Vec::with_capacity(self.relation_count() + 1);

        if self.rerun_identity {
            results.push(check_rerun_identity(harness, wf, observe_node, &self.seed).await?);
        }
        if self.permutation_invariance {
            results.push(check_permutation(harness, wf, observe_node, &self.seed).await?);
        }
        if self.duplication_factor >= 2 {
            results.push(
                check_duplication(
                    harness,
                    wf,
                    observe_node,
                    &self.seed,
                    self.duplication_factor,
                )
                .await?,
            );
        }
        if self.additivity {
            results.push(check_additivity(harness, wf, observe_node, &self.seed).await?);
        }
        Ok(results)
    }
}

/// Re-run identity: the determinism the engine guarantees, asserted. Two runs
/// with identical input must produce byte-identical observed output, in order.
pub async fn check_rerun_identity(
    harness: &VerificationHarness,
    wf: &a2w_ir::Workflow,
    observe_node: &str,
    seed: &[Value],
) -> Result<CheckResult, VerifyError> {
    let a = harness.observe(wf, observe_node, seed.to_vec()).await?;
    let b = harness.observe(wf, observe_node, seed.to_vec()).await?;
    Ok(if a == b {
        CheckResult::pass(
            CheckCategory::EngineInvariant,
            "rerun_identity",
            format!("two runs produced identical output ({} item(s))", a.len()),
        )
    } else {
        CheckResult::fail(
            CheckCategory::EngineInvariant,
            "rerun_identity",
            format!(
                "non-deterministic: run A had {} item(s), run B had {} item(s) \
                 (or differing payloads)",
                a.len(),
                b.len()
            ),
        )
    })
}

/// Permutation invariance: reordering the inputs must not change the output
/// multiset. Catches order-dependent cross-item contamination.
pub async fn check_permutation(
    harness: &VerificationHarness,
    wf: &a2w_ir::Workflow,
    observe_node: &str,
    seed: &[Value],
) -> Result<CheckResult, VerifyError> {
    let base = harness.observe(wf, observe_node, seed.to_vec()).await?;
    let mut reversed = seed.to_vec();
    reversed.reverse();
    let permuted = harness.observe(wf, observe_node, reversed).await?;
    Ok(if multiset_eq(&base, &permuted) {
        CheckResult::pass(
            CheckCategory::EngineInvariant,
            "permutation_invariance",
            format!(
                "output multiset unchanged under input reversal ({} item(s))",
                base.len()
            ),
        )
    } else {
        CheckResult::fail(
            CheckCategory::EngineInvariant,
            "permutation_invariance",
            format!(
                "output multiset changed under input reversal: {} vs {} item(s)",
                base.len(),
                permuted.len()
            ),
        )
    })
}

/// Duplication scaling: feeding the input `factor` times must produce `factor`×
/// the output count (and `factor` copies of the output multiset). Catches
/// dropped/duplicated items in fan-out.
pub async fn check_duplication(
    harness: &VerificationHarness,
    wf: &a2w_ir::Workflow,
    observe_node: &str,
    seed: &[Value],
    factor: usize,
) -> Result<CheckResult, VerifyError> {
    let base = harness.observe(wf, observe_node, seed.to_vec()).await?;
    let mut scaled_input = Vec::with_capacity(seed.len() * factor);
    for _ in 0..factor {
        scaled_input.extend_from_slice(seed);
    }
    let scaled = harness.observe(wf, observe_node, scaled_input).await?;

    // Expected multiset = `factor` copies of base's multiset.
    let mut expected: Vec<Value> = Vec::with_capacity(base.len() * factor);
    for _ in 0..factor {
        expected.extend(base.iter().cloned());
    }
    Ok(if multiset_eq(&expected, &scaled) {
        CheckResult::pass(
            CheckCategory::EngineInvariant,
            "duplication_scaling",
            format!(
                "×{factor} input → ×{factor} output ({} → {} item(s))",
                base.len(),
                scaled.len()
            ),
        )
    } else {
        CheckResult::fail(
            CheckCategory::EngineInvariant,
            "duplication_scaling",
            format!(
                "×{factor} input did not scale output: expected {} item(s), got {}",
                base.len() * factor,
                scaled.len()
            ),
        )
    })
}

/// Additivity: `output(a ++ b)` must equal `output(a) ⊎ output(b)` as
/// multisets. The seed is split in half to form `a` and `b`. This is the
/// strongest signal that items are processed independently (no leakage).
pub async fn check_additivity(
    harness: &VerificationHarness,
    wf: &a2w_ir::Workflow,
    observe_node: &str,
    seed: &[Value],
) -> Result<CheckResult, VerifyError> {
    if seed.len() < 2 {
        return Ok(CheckResult::pass(
            CheckCategory::EngineInvariant,
            "additivity",
            "skipped: seed has fewer than 2 items (additivity is trivial)".to_string(),
        ));
    }
    let mid = seed.len() / 2;
    let a = &seed[..mid];
    let b = &seed[mid..];

    let out_a = harness.observe(wf, observe_node, a.to_vec()).await?;
    let out_b = harness.observe(wf, observe_node, b.to_vec()).await?;
    let out_ab = harness.observe(wf, observe_node, seed.to_vec()).await?;

    let mut union = out_a.clone();
    union.extend(out_b.iter().cloned());

    // Compare multisets via canonicalized counts.
    Ok(if multiset_of(&union) == multiset_of(&out_ab) {
        CheckResult::pass(
            CheckCategory::EngineInvariant,
            "additivity",
            format!(
                "output(a++b) == output(a) ⊎ output(b) ({} + {} == {} item(s))",
                out_a.len(),
                out_b.len(),
                out_ab.len()
            ),
        )
    } else {
        CheckResult::fail(
            CheckCategory::EngineInvariant,
            "additivity",
            format!(
                "output(a++b) != output(a) ⊎ output(b): {} + {} = {} expected, got {} item(s) \
                 — items are not processed independently",
                out_a.len(),
                out_b.len(),
                out_a.len() + out_b.len(),
                out_ab.len()
            ),
        )
    })
}
