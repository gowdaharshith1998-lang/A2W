//! Differential / N-version cross-checks (M3d): compute the same result two
//! independent ways and compare.
//!
//! Two independent computations failing the *same* way is far less likely than
//! one being wrong, so agreement is strong evidence. A cross-check can pit the
//! workflow against a tiny reference oracle (a Rust closure) or against a second,
//! independently-authored workflow.

use serde_json::Value;

use crate::harness::VerificationHarness;
use crate::report::{CheckCategory, CheckResult};
use crate::{multiset_eq, VerifyError};

/// A reference computation: the "second way" to compute the result. Returns the
/// expected observed-output list for a given input.
pub type Oracle = dyn Fn(&[Value]) -> Vec<Value> + Send + Sync;

/// Compare a workflow's observed output against a reference oracle closure on a
/// given input. Comparison is multiset-based (order-insensitive).
///
/// # Errors
/// [`VerifyError`] only if the run itself fails.
pub async fn cross_check_oracle(
    harness: &VerificationHarness,
    wf: &a2w_ir::Workflow,
    observe_node: &str,
    name: &str,
    input: Vec<Value>,
    oracle: &Oracle,
) -> Result<CheckResult, VerifyError> {
    let expected = oracle(&input);
    let actual = harness.observe(wf, observe_node, input).await?;
    let check_name = format!("xcheck:oracle:{name}");
    Ok(if multiset_eq(&expected, &actual) {
        CheckResult::pass(
            CheckCategory::CrossCheck,
            check_name,
            format!("workflow agrees with oracle ({} item(s))", actual.len()),
        )
    } else {
        CheckResult::fail(
            CheckCategory::CrossCheck,
            check_name,
            format!(
                "workflow disagrees with oracle: oracle {} item(s), workflow {} item(s)",
                expected.len(),
                actual.len()
            ),
        )
    })
}

/// Compare two independently-authored workflows that should compute the same
/// result, on the same input. Comparison is multiset-based.
///
/// # Errors
/// [`VerifyError`] if either run fails.
pub async fn cross_check_workflows(
    harness: &VerificationHarness,
    wf_a: &a2w_ir::Workflow,
    observe_a: &str,
    wf_b: &a2w_ir::Workflow,
    observe_b: &str,
    name: &str,
    input: Vec<Value>,
) -> Result<CheckResult, VerifyError> {
    let out_a = harness.observe(wf_a, observe_a, input.clone()).await?;
    let out_b = harness.observe(wf_b, observe_b, input).await?;
    let check_name = format!("xcheck:nversion:{name}");
    Ok(if multiset_eq(&out_a, &out_b) {
        CheckResult::pass(
            CheckCategory::CrossCheck,
            check_name,
            format!(
                "the two workflows agree ({} vs {} item(s), multiset-equal)",
                out_a.len(),
                out_b.len()
            ),
        )
    } else {
        CheckResult::fail(
            CheckCategory::CrossCheck,
            check_name,
            format!(
                "the two workflows disagree: A produced {} item(s), B produced {}",
                out_a.len(),
                out_b.len()
            ),
        )
    })
}
