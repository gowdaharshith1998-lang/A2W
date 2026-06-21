//! The calibrated confidence report.
//!
//! This is the deliberate inverse of a binary "verified" stamp. A
//! [`ConfidenceReport`] enumerates **exactly which checks ran and which
//! passed**, grouped by category, so a reader can see the evidence rather than
//! a verdict. The [`ConfidenceReport::summary`] explicitly names categories
//! that were **not** exercised (zero checks) — silence is never reported as
//! success.

use serde::Serialize;

/// The kind of evidence a [`CheckResult`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckCategory {
    /// A spec assertion the agent co-authored and a human can confirm.
    Spec,
    /// A golden input→output fixture (known ground truth).
    Golden,
    /// A metamorphic relation across related runs (no oracle needed).
    Metamorphic,
    /// A differential / N-version cross-check (two independent computations).
    CrossCheck,
    /// The determinism / re-run identity guarantee.
    Determinism,
}

impl CheckCategory {
    /// Human-readable label used in the calibrated summary.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            CheckCategory::Spec => "spec assertions",
            CheckCategory::Golden => "golden fixtures",
            CheckCategory::Metamorphic => "metamorphic relations",
            CheckCategory::CrossCheck => "differential cross-checks",
            CheckCategory::Determinism => "determinism checks",
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

/// The threshold a workflow must clear to be considered high-confidence (used
/// by M4 promotion and M5 fitness gating).
///
/// Crucially, this is **never** "it ran": [`Threshold::min_metamorphic`]
/// forces at least some no-oracle evidence, and `require_all_passed` means a
/// single failing check disqualifies the workflow.
#[derive(Debug, Clone, Copy)]
pub struct Threshold {
    /// Minimum total checks that must have run.
    pub min_total: usize,
    /// Minimum metamorphic relations that must have held.
    pub min_metamorphic: usize,
    /// Minimum overall pass ratio in `[0.0, 1.0]`.
    pub min_score: f64,
    /// If true, *every* check must pass (no partial credit).
    pub require_all_passed: bool,
}

impl Default for Threshold {
    /// A sensible default: ≥3 metamorphic relations, ≥4 checks total, all must
    /// pass.
    fn default() -> Self {
        Self {
            min_total: 4,
            min_metamorphic: 3,
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

    /// Total number of checks that ran.
    #[must_use]
    pub fn total(&self) -> usize {
        self.checks.len()
    }

    /// Number of checks that passed.
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

    /// Pass ratio in `[0.0, 1.0]`. Zero checks → `0.0` (absence of evidence is
    /// not confidence).
    #[must_use]
    pub fn score(&self) -> f64 {
        if self.checks.is_empty() {
            return 0.0;
        }
        self.passed() as f64 / self.total() as f64
    }

    /// The failing checks (useful for repair loops and M5 fitness diagnostics).
    #[must_use]
    pub fn failures(&self) -> Vec<&CheckResult> {
        self.checks.iter().filter(|c| !c.passed).collect()
    }

    /// Whether this report clears `threshold`. This is the promotion / fitness
    /// gate — deliberately strict.
    #[must_use]
    pub fn meets(&self, threshold: &Threshold) -> bool {
        let metamorphic_passed = self.passed_in(CheckCategory::Metamorphic);
        if self.total() < threshold.min_total {
            return false;
        }
        if metamorphic_passed < threshold.min_metamorphic {
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

    /// A calibrated, human-readable summary. Enumerates which categories ran
    /// and how many passed, and **explicitly names categories with zero
    /// checks** so the absence of evidence is visible rather than implied.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!(
            "confidence report for workflow '{}' (observing node '{}')",
            self.workflow_id, self.observe_node
        ));
        lines.push(format!(
            "  overall: {}/{} checks passed (score {:.2})",
            self.passed(),
            self.total(),
            self.score()
        ));
        for category in [
            CheckCategory::Spec,
            CheckCategory::Golden,
            CheckCategory::Metamorphic,
            CheckCategory::CrossCheck,
            CheckCategory::Determinism,
        ] {
            let total = self.count_in(category);
            if total == 0 {
                lines.push(format!("  {}: 0 — NOT CHECKED", category.label()));
            } else {
                lines.push(format!(
                    "  {}: {}/{} passed",
                    category.label(),
                    self.passed_in(category),
                    total
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

    #[test]
    fn empty_report_has_zero_score() {
        let r = ConfidenceReport::new("wf", "n");
        assert_eq!(r.score(), 0.0);
        assert!(!r.meets(&Threshold::default()));
        assert!(r.summary().contains("NOT CHECKED"));
    }

    #[test]
    fn all_passing_meets_default_threshold() {
        let mut r = ConfidenceReport::new("wf", "n");
        for i in 0..3 {
            r.push(CheckResult::pass(
                CheckCategory::Metamorphic,
                format!("mr{i}"),
                "held",
            ));
        }
        r.push(CheckResult::pass(CheckCategory::Determinism, "rerun", "identical"));
        assert_eq!(r.score(), 1.0);
        assert!(r.meets(&Threshold::default()));
    }

    #[test]
    fn one_failure_disqualifies_under_require_all() {
        let mut r = ConfidenceReport::new("wf", "n");
        for i in 0..3 {
            r.push(CheckResult::pass(
                CheckCategory::Metamorphic,
                format!("mr{i}"),
                "held",
            ));
        }
        r.push(CheckResult::fail(CheckCategory::Golden, "g0", "mismatch"));
        assert!(!r.meets(&Threshold::default()));
        assert!(r.summary().contains("failing check"));
    }

    #[test]
    fn insufficient_metamorphic_disqualifies_even_if_all_pass() {
        let mut r = ConfidenceReport::new("wf", "n");
        // Plenty of golden checks, but only 1 metamorphic relation.
        for i in 0..6 {
            r.push(CheckResult::pass(
                CheckCategory::Golden,
                format!("g{i}"),
                "matched",
            ));
        }
        r.push(CheckResult::pass(CheckCategory::Metamorphic, "mr0", "held"));
        assert_eq!(r.score(), 1.0);
        assert!(
            !r.meets(&Threshold::default()),
            "must require min_metamorphic relations, not just 'it ran'"
        );
    }
}
