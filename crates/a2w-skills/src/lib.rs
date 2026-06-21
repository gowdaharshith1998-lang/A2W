//! # a2w-skills — the skill library / workflow memory (M4)
//!
//! Expertise that compounds. A workflow that clears the M3 confidence
//! threshold is *promoted* into a reusable [`Skill`], indexed by a
//! [`TaskSignature`]. New queries retrieve the closest skills, which are then
//! [`adapt`](compose::adapt)ed or [`compose_sequential`](compose::compose_sequential)d
//! into a solution — so the system gets better at a task family the more it
//! solves it.
//!
//! The non-negotiable rule: **promotion is gated on M3's signal, never on "it
//! ran"** (see [`SkillLibrary::promote`]). A skill also must be statically
//! valid (M1), so everything in the library is valid-by-construction.

#![forbid(unsafe_code)]

pub mod compose;
pub mod library;
pub mod signature;

use a2w_validator::ValidationReport;
use thiserror::Error;

pub use compose::{adapt, compose_sequential};
pub use library::{EvidenceSnapshot, Skill, SkillLibrary};
pub use signature::TaskSignature;

/// Errors from skill promotion and composition.
#[derive(Debug, Error)]
pub enum SkillError {
    /// The workflow failed M1 validation; a skill must be valid-by-construction.
    #[error("workflow is not valid: {} finding(s)", .0.findings.len())]
    Invalid(ValidationReport),
    /// The confidence report did not clear the promotion threshold.
    #[error(
        "below promotion threshold (outcome score {score:.2}, {semantic_relations_passed} semantic \
         relation(s) held); promotion is gated on OUTCOME evidence, not execution and not \
         engine-invariants"
    )]
    BelowThreshold {
        /// The report's outcome pass ratio.
        score: f64,
        /// How many spec-derived semantic relations held.
        semantic_relations_passed: usize,
        /// The calibrated summary, for diagnostics.
        summary: String,
    },
    /// The report describes a different workflow than the one being promoted.
    #[error("confidence report is for workflow '{report_for}', not '{workflow}'")]
    ReportMismatch {
        /// The workflow the report was computed for.
        report_for: String,
        /// The workflow being promoted.
        workflow: String,
    },
    /// Composition could not produce a well-formed graph.
    #[error("cannot compose: {0}")]
    Compose(String),
}
