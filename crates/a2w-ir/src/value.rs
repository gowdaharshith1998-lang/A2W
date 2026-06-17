//! Shared value-ish IR pieces: retry and error policies.
//!
//! These are kept in their own module so that, as the IR grows, policy-related
//! types have a natural home separate from the node taxonomy.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How a node should be retried on transient failure.
///
/// Semantics are intentionally minimal for M1: a fixed number of attempts and
/// a single backoff value in milliseconds. Richer strategies (exponential
/// jitter, per-error-class policies) are deferred to later milestones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    /// Maximum number of attempts, including the first one. `0` and `1` both
    /// mean "do not retry"; the validator may later flag `0` as suspicious.
    pub max_attempts: u32,
    /// Delay between attempts, in milliseconds.
    pub backoff_ms: u64,
}

/// What to do when a node ultimately fails (after exhausting retries).
///
/// `Route` is a placeholder for M1: in a later milestone it will target a
/// dedicated error output port. For now it carries no payload and simply
/// signals intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorPolicy {
    /// Halt the workflow run.
    Stop,
    /// Swallow the error and continue downstream as if the node produced no
    /// output.
    Continue,
    /// Route the error to an error path (wiring defined in a later milestone).
    Route,
}
