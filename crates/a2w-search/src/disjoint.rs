//! Fitness/holdout disjointness guard (F3).
//!
//! The fitness/holdout split (F2) only kills Goodhart if the two plans are
//! genuinely independent. If they share the same inputs, assertions, fixtures
//! or relations, a blind spot in the fitness set is a blind spot in the holdout
//! too — the holdout would "certify" exactly what the fitness optimized, which
//! is the correlated-blind-spot failure the test/generator decoupling rule
//! exists to prevent.
//!
//! [`assert_plans_disjoint`] makes this a **checked contract**: it errors if the
//! plans share any input set, spec assertion, golden fixture, or semantic
//! relation.

use std::collections::BTreeSet;

use a2w_verify::VerificationPlan;
use serde_json::Value;

/// The kinds of evidence a plan can contribute, fingerprinted for overlap
/// detection.
#[derive(Default)]
struct Fingerprints {
    inputs: BTreeSet<String>,
    assertions: BTreeSet<String>,
    goldens: BTreeSet<String>,
    semantics: BTreeSet<String>,
}

fn canon_vec(v: &[Value]) -> String {
    serde_json::to_string(&Value::Array(v.to_vec())).unwrap_or_default()
}

fn fingerprints(plan: &VerificationPlan) -> Fingerprints {
    let mut fp = Fingerprints::default();
    if let Some(spec) = &plan.spec {
        fp.inputs.insert(canon_vec(&spec.input));
        for a in &spec.assertions {
            fp.assertions
                .insert(serde_json::to_string(a).unwrap_or_default());
        }
    }
    for g in &plan.golden {
        fp.inputs.insert(canon_vec(&g.input));
        fp.goldens
            .insert(serde_json::to_string(g).unwrap_or_default());
    }
    if let Some(sem) = &plan.semantic {
        for r in &sem.relations {
            fp.semantics
                .insert(serde_json::to_string(r).unwrap_or_default());
            for inp in r.inputs() {
                fp.inputs.insert(canon_vec(&inp));
            }
        }
    }
    fp
}

fn first_overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> Option<String> {
    a.intersection(b).next().cloned()
}

/// Return `Some(reason)` if the two plans share evidence; `None` if disjoint.
#[must_use]
pub fn shared_evidence(fitness: &VerificationPlan, holdout: &VerificationPlan) -> Option<String> {
    let f = fingerprints(fitness);
    let h = fingerprints(holdout);

    if let Some(s) = first_overlap(&f.inputs, &h.inputs) {
        return Some(format!("a shared input set ({s})"));
    }
    if let Some(s) = first_overlap(&f.assertions, &h.assertions) {
        return Some(format!("an identical spec assertion ({s})"));
    }
    if let Some(s) = first_overlap(&f.goldens, &h.goldens) {
        return Some(format!("an identical golden fixture ({s})"));
    }
    if let Some(s) = first_overlap(&f.semantics, &h.semantics) {
        return Some(format!("an identical semantic relation ({s})"));
    }
    None
}

/// Error if the fitness and holdout plans are not evidence-disjoint.
///
/// # Errors
/// [`crate::SearchError::CorrelatedEvidence`] with a human-readable reason.
pub fn assert_plans_disjoint(
    fitness: &VerificationPlan,
    holdout: &VerificationPlan,
) -> Result<(), crate::SearchError> {
    match shared_evidence(fitness, holdout) {
        Some(reason) => Err(crate::SearchError::CorrelatedEvidence(reason)),
        None => Ok(()),
    }
}
