//! The skill library: promote, index, retrieve.
//!
//! Promotion is the load-bearing gate. A workflow becomes a skill **only** if
//! its [`ConfidenceReport`] clears a [`Threshold`] — never because "it ran".
//! The stored skill carries a snapshot of the evidence (score, checks passed,
//! metamorphic relations held) so the basis for promotion is auditable.

use std::collections::BTreeMap;

use a2w_ir::Workflow;
use a2w_verify::{ConfidenceReport, Threshold};
use serde::{Deserialize, Serialize};

use crate::signature::TaskSignature;
use crate::SkillError;

/// The evidence snapshot recorded at promotion time. Calibrated, not a verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceSnapshot {
    /// Pass ratio at promotion.
    pub score: f64,
    /// Total checks that ran.
    pub checks_total: usize,
    /// Checks that passed.
    pub checks_passed: usize,
    /// Metamorphic relations that held (the no-oracle signal).
    pub metamorphic_passed: usize,
    /// Human-readable calibrated summary captured at promotion.
    pub summary: String,
}

/// A promoted, verification-cleared, reusable workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Stable id minted by the library.
    pub id: String,
    /// The natural-language query this skill solves.
    pub query: String,
    /// The proven workflow IR.
    pub workflow: Workflow,
    /// The node whose output is "the result".
    pub observe_node: String,
    /// Retrieval/index signature.
    pub signature: TaskSignature,
    /// The evidence that justified promotion.
    pub evidence: EvidenceSnapshot,
}

/// An in-memory, deterministic skill library.
///
/// Iteration order is stable (`BTreeMap`) and ids are minted from a monotonic
/// counter, so retrieval results are reproducible — a property the search loop
/// (M5) and any test corpus depend on.
#[derive(Debug, Clone)]
pub struct SkillLibrary {
    skills: BTreeMap<String, Skill>,
    threshold: Threshold,
    seq: u64,
}

impl SkillLibrary {
    /// A library with an explicit promotion threshold.
    #[must_use]
    pub fn new(threshold: Threshold) -> Self {
        Self {
            skills: BTreeMap::new(),
            threshold,
            seq: 0,
        }
    }

    /// A library with the default (strict) threshold.
    #[must_use]
    pub fn with_default_threshold() -> Self {
        Self::new(Threshold::default())
    }

    /// The promotion threshold in force.
    #[must_use]
    pub fn threshold(&self) -> &Threshold {
        &self.threshold
    }

    /// Number of stored skills.
    #[must_use]
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Whether the library is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Promote a workflow into the library **iff** its confidence report clears
    /// the threshold and the workflow is statically valid (M1). Returns the new
    /// skill id.
    ///
    /// # Errors
    /// - [`SkillError::Invalid`] if the workflow does not pass M1 validation
    ///   (a skill must be valid-by-construction).
    /// - [`SkillError::BelowThreshold`] if the report does not clear the
    ///   threshold — promotion is gated on the M3 signal, never on "it ran".
    /// - [`SkillError::ReportMismatch`] if the report is for a different
    ///   workflow than the one being promoted.
    pub fn promote(
        &mut self,
        query: &str,
        workflow: Workflow,
        observe_node: &str,
        report: &ConfidenceReport,
    ) -> Result<String, SkillError> {
        // The report must actually describe this workflow.
        if report.workflow_id != workflow.id {
            return Err(SkillError::ReportMismatch {
                report_for: report.workflow_id.clone(),
                workflow: workflow.id.clone(),
            });
        }

        // A promoted skill must be statically valid.
        let validation = a2w_validator::validate(&workflow);
        if !validation.is_valid {
            return Err(SkillError::Invalid(validation));
        }

        // The gate: M3 evidence, not execution.
        if !report.meets(&self.threshold) {
            return Err(SkillError::BelowThreshold {
                score: report.score(),
                metamorphic_passed: report.passed_in(a2w_verify::CheckCategory::Metamorphic),
                summary: report.summary(),
            });
        }

        let id = self.mint_id();
        let signature = TaskSignature::from_query_and_workflow(query, &workflow);
        let evidence = EvidenceSnapshot {
            score: report.score(),
            checks_total: report.total(),
            checks_passed: report.passed(),
            metamorphic_passed: report.passed_in(a2w_verify::CheckCategory::Metamorphic),
            summary: report.summary(),
        };
        let skill = Skill {
            id: id.clone(),
            query: query.to_string(),
            workflow,
            observe_node: observe_node.to_string(),
            signature,
            evidence,
        };
        self.skills.insert(id.clone(), skill);
        Ok(id)
    }

    /// Look up a skill by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Skill> {
        self.skills.get(id)
    }

    /// All skills, in deterministic id order.
    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }

    /// Retrieve the top-`k` skills most similar to `query`, each paired with its
    /// similarity score, sorted descending. Ties break by skill id for
    /// determinism.
    #[must_use]
    pub fn retrieve(&self, query: &str, k: usize) -> Vec<(&Skill, f64)> {
        let query_sig = TaskSignature::from_query(query);
        let mut scored: Vec<(&Skill, f64)> = self
            .skills
            .values()
            .map(|s| (s, query_sig.similarity(&s.signature)))
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.id.cmp(&b.0.id))
        });
        scored.truncate(k);
        scored
    }

    /// The single best match for `query`, if the library is non-empty.
    #[must_use]
    pub fn best_match(&self, query: &str) -> Option<(&Skill, f64)> {
        self.retrieve(query, 1).into_iter().next()
    }

    fn mint_id(&mut self) -> String {
        self.seq += 1;
        format!("skill_{:06}", self.seq)
    }
}

impl Default for SkillLibrary {
    fn default() -> Self {
        Self::with_default_threshold()
    }
}
