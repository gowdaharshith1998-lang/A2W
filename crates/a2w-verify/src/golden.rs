//! Golden fixtures (M3b): known input → known output regression checks.
//!
//! When ground truth *is* available, a golden fixture pins it. Because runs are
//! deterministic and zero-token, a golden suite is a fast, exact regression net.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::harness::VerificationHarness;
use crate::report::{CheckCategory, CheckResult};
use crate::{multiset_eq, VerifyError};

/// How a fixture's expected output is compared to the actual output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    /// Exact, ordered equality.
    Exact,
    /// Multiset equality (order-insensitive) — for workflows whose output order
    /// is not part of the contract.
    Multiset,
}

/// A single golden fixture: an input and the expected observed output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenFixture {
    /// A stable, human-readable name.
    pub name: String,
    /// The input seeded into the trigger.
    pub input: Vec<Value>,
    /// The expected `.json` payloads of the observed node, in order (or as a
    /// multiset, per `match_mode`).
    pub expected: Vec<Value>,
    /// How to compare.
    #[serde(default = "default_match_mode")]
    pub match_mode: MatchMode,
}

fn default_match_mode() -> MatchMode {
    MatchMode::Exact
}

impl GoldenFixture {
    /// Run the fixture against `wf` observing `observe_node`.
    ///
    /// # Errors
    /// [`VerifyError`] only if the run itself fails.
    pub async fn check(
        &self,
        harness: &VerificationHarness,
        wf: &a2w_ir::Workflow,
        observe_node: &str,
    ) -> Result<CheckResult, VerifyError> {
        let actual = harness
            .observe(wf, observe_node, self.input.clone())
            .await?;
        let matched = match self.match_mode {
            MatchMode::Exact => actual == self.expected,
            MatchMode::Multiset => multiset_eq(&actual, &self.expected),
        };
        let name = format!("golden:{}", self.name);
        Ok(if matched {
            CheckResult::pass(
                CheckCategory::Golden,
                name,
                format!("matched ({} item(s), {:?})", actual.len(), self.match_mode),
            )
        } else {
            CheckResult::fail(
                CheckCategory::Golden,
                name,
                format!(
                    "mismatch ({:?}): expected {} item(s), got {} item(s)",
                    self.match_mode,
                    self.expected.len(),
                    actual.len()
                ),
            )
        })
    }
}
