//! The calibrated confidence report.
//!
//! This is the deliberate inverse of a binary "verified" stamp. A
//! [`ConfidenceReport`] enumerates **exactly which checks ran and which
//! passed**, grouped by category, so a reader can see the evidence rather than
//! a verdict.
//!
//! ## Engine-invariants vs. outcome evidence (F1)
//! A2W's engine is deterministic and per-item-independent **by construction**,
//! so a whole class of checks — re-run identity, permutation invariance,
//! duplication scaling, additivity — hold for *any* structurally-valid
//! workflow regardless of whether its logic is correct. Those are
//! [`CheckCategory::EngineInvariant`]: they verify the *engine's guarantees*,
//! **not the outcome**. Treating them as outcome evidence would let "the engine
//! behaved" masquerade as "the answer is right."
//!
//! Everything else — spec assertions, golden fixtures, differential
//! cross-checks, and **spec-derived semantic relations** that encode the
//! workflow's intent — is *outcome evidence* ([`CheckCategory::is_outcome_evidence`]).
//! Only outcome evidence can support an outcome-correctness claim;
//! [`ConfidenceReport::score`] and [`ConfidenceReport::meets`] are defined over
//! outcome evidence alone. A report holding only engine-invariants is reported
//! as **"engine-verified; outcome UNVERIFIED."**

use serde::Serialize;

/// The kind of evidence a [`CheckResult`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckCategory {
    /// An A2W execution guarantee (re-run identity, permutation invariance,
    /// duplication scaling, additivity). Holds for **any** valid workflow, so
    /// it is **not** outcome evidence — it verifies the engine, not the answer.
    EngineInvariant,
    /// A spec assertion the agent co-authored and a human can confirm.
    Spec,
    /// A golden input→output fixture (known ground truth).
    Golden,
    /// A spec-derived *semantic* metamorphic relation that encodes the
    /// workflow's intent (e.g. "scaling input `price` by k scales output
    /// `total` by k"). Authored from intent, never read off the workflow under
    /// test — so it catches logic faults engine-invariants cannot.
    SemanticRelation,
    /// A differential / N-version cross-check (two independent computations).
    CrossCheck,
}

impl CheckCategory {
    /// Whether this category counts as evidence about the **outcome** (the
    /// answer), as opposed to the engine's behaviour. Engine-invariants are the
    /// only non-outcome category.
    #[must_use]
    pub fn is_outcome_evidence(self) -> bool {
        !matches!(self, CheckCategory::EngineInvariant)
    }

    /// Human-readable label used in the calibrated summary.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            CheckCategory::EngineInvariant => "engine invariants",
            CheckCategory::Spec => "spec assertions",
            CheckCategory::Golden => "golden fixtures",
            CheckCategory::SemanticRelation => "semantic relations",
            CheckCategory::CrossCheck => "differential cross-checks",
        }
    }
}

/// The outcome of a single check.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    /// Which evidence category this check belongs to.
    pub category: CheckCategory,
    /// A stable, human-readable name (e.g. `"rerun_identity"`,
    /// `"golden:weekend_surcharge"`).
    pub name: String,
    /// Whether the check held.
    pub passed: bool,
    /// Expected-vs-actual detail; on failure this is specific enough to act on.
    pub detail: String,
}

impl CheckResult {
    /// Construct a passing result.
    #[must_use]
    pub fn pass(category: CheckCategory, name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            category,
            name: name.into(),
            passed: true,
            detail: detail.into(),
        }
    }

    /// Construct a failing result.
    #[must_use]
    pub fn fail(category: CheckCategory, name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            category,
            name: name.into(),
            passed: false,
            detail: detail.into(),
        }
    }
}

/// The threshold a workflow must clear to be considered outcome-verified (used
/// by M4 promotion and M5 holdout certification).
///
/// Crucially this is defined over **outcome evidence**: engine-invariants alone
/// can never clear it ([`Threshold::min_outcome_total`] > 0), and at least one
/// intent-encoding semantic relation is required by default
/// ([`Threshold::min_semantic_relations`]) — so "the engine behaved" can never
/// be mistaken for "the outcome is correct."
#[derive(Debug, Clone, Copy)]
pub struct Threshold {
    /// Minimum **outcome-evidence** checks that must have run.
    pub min_outcome_total: usize,
    /// Minimum spec-derived semantic relations that must have held.
    pub min_semantic_relations: usize,
    /// Minimum outcome pass ratio in `[0.0, 1.0]`.
    pub min_score: f64,
    /// If true, *every* check (including engine-invariants) must pass — a
    /// failing engine-invariant (e.g. non-determinism) blocks promotion.
    pub require_all_passed: bool,
}

impl Default for Threshold {
    /// A strict default: ≥2 outcome checks, ≥1 semantic relation, perfect
    /// outcome score, and no failing check of any kind.
    fn default() -> Self {
        Self {
            min_outcome_total: 2,
            min_semantic_relations: 1,
            min_score: 1.0,
            require_all_passed: true,
        }
    }
}

/// A calibrated confidence report: the evidence, not a verdict.
#[derive(Debug, Clone, Serialize)]
pub struct ConfidenceReport {
    /// The workflow this report concerns.
    pub workflow_id: String,
    /// The node whose output was observed as "the result".
    pub observe_node: String,
    /// Every check that ran, in execution order.
    pub checks: Vec<CheckResult>,
}

impl ConfidenceReport {
    /// Start an empty report for `workflow_id` observing `observe_node`.
    #[must_use]
    pub fn new(workflow_id: impl Into<String>, observe_node: impl Into<String>) -> Self {
        Self {
            workflow_id: workflow_id.into(),
            observe_node: observe_node.into(),
            checks: Vec::new(),
        }
    }

    /// Append a check result.
    pub fn push(&mut self, result: CheckResult) {
        self.checks.push(result);
    }

    /// Total number of checks that ran (all categories).
    #[must_use]
    pub fn total(&self) -> usize {
        self.checks.len()
    }

    /// Number of checks that passed (all categories).
    #[must_use]
    pub fn passed(&self) -> usize {
        self.checks.iter().filter(|c| c.passed).count()
    }

    /// Number of checks in a given category.
    #[must_use]
    pub fn count_in(&self, category: CheckCategory) -> usize {
        self.checks.iter().filter(|c| c.category == category).count()
    }

    /// Number of *passing* checks in a given category.
    #[must_use]
    pub fn passed_in(&self, category: CheckCategory) -> usize {
        self.checks
            .iter()
            .filter(|c| c.category == category && c.passed)
            .count()
    }

    /// Total **outcome-evidence** checks (everything but engine-invariants).
    #[must_use]
    pub fn outcome_total(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.category.is_outcome_evidence())
            .count()
    }

    /// Passing outcome-evidence checks.
    #[must_use]
    pub fn outcome_passed(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.category.is_outcome_evidence() && c.passed)
            .count()
    }

    /// Whether any outcome evidence ran at all.
    #[must_use]
    pub fn has_outcome_evidence(&self) -> bool {
        self.outcome_total() > 0
    }

    /// Whether every engine-invariant that ran held (and ≥1 ran).
    #[must_use]
    pub fn engine_invariants_held(&self) -> bool {
        let total = self.count_in(CheckCategory::EngineInvariant);
        total > 0 && self.passed_in(CheckCategory::EngineInvariant) == total
    }

    /// The **outcome** pass ratio in `[0.0, 1.0]`. Zero outcome checks → `0.0`:
    /// absence of outcome evidence is never confidence, no matter how many
    /// engine-invariants held.
    #[must_use]
    pub fn score(&self) -> f64 {
        let total = self.outcome_total();
        if total == 0 {
            return 0.0;
        }
        self.outcome_passed() as f64 / total as f64
    }

    /// The failing checks (useful for repair loops and M5 fitness diagnostics).
    #[must_use]
    pub fn failures(&self) -> Vec<&CheckResult> {
        self.checks.iter().filter(|c| !c.passed).collect()
    }

    /// Whether this report clears `threshold` for an **outcome-correctness**
    /// claim. Engine-invariants alone can never satisfy this.
    #[must_use]
    pub fn meets(&self, threshold: &Threshold) -> bool {
        if self.outcome_total() < threshold.min_outcome_total {
            return false;
        }
        if self.passed_in(CheckCategory::SemanticRelation) < threshold.min_semantic_relations {
            return false;
        }
        if self.score() < threshold.min_score {
            return false;
        }
        if threshold.require_all_passed && self.passed() != self.total() {
            return false;
        }
        true
    }

    /// Whether the outcome is verified at the default threshold. Convenience
    /// for callers and summaries.
    #[must_use]
    pub fn is_outcome_verified(&self) -> bool {
        self.meets(&Threshold::default())
    }

    /// A calibrated, human-readable summary. Names which categories ran and how
    /// many passed, explicitly prints "NOT CHECKED" for empty categories, and —
    /// most importantly — states the **outcome-verification status** truthfully,
    /// never letting engine-invariants read as outcome correctness.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!(
            "confidence report for workflow '{}' (observing node '{}')",
            self.workflow_id, self.observe_node
        ));

        // The headline: outcome status, stated without ambiguity.
        let engine_total = self.count_in(CheckCategory::EngineInvariant);
        if self.outcome_total() == 0 {
            if engine_total > 0 {
                lines.push(format!(
                    "  OUTCOME: UNVERIFIED — engine-verified only ({}/{} engine invariants held; \
                     engine invariants are NOT outcome evidence)",
                    self.passed_in(CheckCategory::EngineInvariant),
                    engine_total
                ));
            } else {
                lines.push("  OUTCOME: UNVERIFIED — no checks ran".to_string());
            }
        } else {
            lines.push(format!(
                "  OUTCOME: {}/{} outcome checks passed (outcome score {:.2}){}",
                self.outcome_passed(),
                self.outcome_total(),
                self.score(),
                if engine_total > 0 {
                    format!(
                        "; {}/{} engine invariants held",
                        self.passed_in(CheckCategory::EngineInvariant),
                        engine_total
                    )
                } else {
                    String::new()
                }
            ));
        }

        for category in [
            CheckCategory::Spec,
            CheckCategory::Golden,
            CheckCategory::SemanticRelation,
            CheckCategory::CrossCheck,
            CheckCategory::EngineInvariant,
        ] {
            let total = self.count_in(category);
            let tag = if category.is_outcome_evidence() {
                ""
            } else {
                " [engine, not outcome]"
            };
            if total == 0 {
                lines.push(format!("  {}: 0 — NOT CHECKED{}", category.label(), tag));
            } else {
                lines.push(format!(
                    "  {}: {}/{} passed{}",
                    category.label(),
                    self.passed_in(category),
                    total,
                    tag
                ));
            }
        }

        let failures = self.failures();
        if failures.is_empty() {
            lines.push("  no failing checks".to_string());
        } else {
            lines.push(format!("  {} failing check(s):", failures.len()));
            for f in failures {
                lines.push(format!("    - [{}] {}: {}", f.category.label(), f.name, f.detail));
            }
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn semantic(name: &str, pass: bool) -> CheckResult {
        if pass {
            CheckResult::pass(CheckCategory::SemanticRelation, name, "held")
        } else {
            CheckResult::fail(CheckCategory::SemanticRelation, name, "violated")
        }
    }

    #[test]
    fn empty_report_has_zero_score_and_is_unverified() {
        let r = ConfidenceReport::new("wf", "n");
        assert_eq!(r.score(), 0.0);
        assert!(!r.meets(&Threshold::default()));
        assert!(r.summary().contains("UNVERIFIED"));
    }

    #[test]
    fn engine_invariants_only_does_not_verify_outcome() {
        let mut r = ConfidenceReport::new("wf", "n");
        for i in 0..4 {
            r.push(CheckResult::pass(
                CheckCategory::EngineInvariant,
                format!("ei{i}"),
                "held",
            ));
        }
        // Every engine invariant held...
        assert!(r.engine_invariants_held());
        // ...but the OUTCOME is unverified: no outcome evidence.
        assert_eq!(r.score(), 0.0);
        assert!(!r.meets(&Threshold::default()));
        let s = r.summary();
        assert!(s.contains("OUTCOME: UNVERIFIED — engine-verified only"), "{s}");
        assert!(s.contains("[engine, not outcome]"), "{s}");
    }

    #[test]
    fn outcome_evidence_with_semantic_relation_verifies() {
        let mut r = ConfidenceReport::new("wf", "n");
        r.push(CheckResult::pass(CheckCategory::Spec, "count", "ok"));
        r.push(semantic("scaling", true));
        r.push(CheckResult::pass(CheckCategory::EngineInvariant, "rerun", "held"));
        assert_eq!(r.score(), 1.0);
        assert!(r.meets(&Threshold::default()));
    }

    #[test]
    fn outcome_without_any_semantic_relation_fails_default_threshold() {
        // Two spec assertions, no semantic relation → default requires ≥1.
        let mut r = ConfidenceReport::new("wf", "n");
        r.push(CheckResult::pass(CheckCategory::Spec, "a", "ok"));
        r.push(CheckResult::pass(CheckCategory::Spec, "b", "ok"));
        assert_eq!(r.score(), 1.0);
        assert!(
            !r.meets(&Threshold::default()),
            "default threshold requires a semantic relation"
        );
    }

    #[test]
    fn one_failing_check_disqualifies_under_require_all() {
        let mut r = ConfidenceReport::new("wf", "n");
        r.push(CheckResult::pass(CheckCategory::Spec, "a", "ok"));
        r.push(semantic("scaling", true));
        r.push(CheckResult::fail(CheckCategory::EngineInvariant, "rerun", "non-deterministic"));
        // Outcome score is perfect, but a failing engine-invariant blocks it.
        assert_eq!(r.score(), 1.0);
        assert!(!r.meets(&Threshold::default()));
    }
}
