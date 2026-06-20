//! [`NodeKind::Approval`] executor — human-in-the-loop approval gate.
//!
//! Params:
//! ```json
//! { "summary"?: "<text>", "timeout_secs"?: 3600, "poll_interval_secs"?: 5 }
//! ```
//!
//! For each input item the executor:
//! 1. Writes a pending approval row (via [`a2w_engine::ApprovalGate`]) carrying
//!    the item's JSON payload + the optional `summary`.
//! 2. Polls the gate on a backoff until a decision arrives or `timeout_secs`
//!    elapses (default 1 h).
//! 3. Routes approved items to port `0`, rejected items to port `1`.
//!
//! A timeout is treated as REJECTED (port 1) — fail-closed for safety.
//!
//! `dry_run` skips the gate entirely and emits one approved+one rejected
//! mock per input so the run shape can be inspected.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use a2w_engine::{ApprovalOutcome, ExecutionMode, Item, NodeContext, NodeError, NodeExecutor};

/// Executor for [`a2w_ir::NodeKind::Approval`](a2w_ir::NodeKind::Approval).
#[derive(Debug, Default, Clone)]
pub struct Approval;

#[derive(Debug, Deserialize)]
struct ApprovalSpec {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
    #[serde(default = "default_poll_secs")]
    poll_interval_secs: u64,
}

fn default_timeout_secs() -> u64 {
    3600
}
fn default_poll_secs() -> u64 {
    5
}

impl ApprovalSpec {
    fn parse(params: &serde_json::Value) -> Result<Self, NodeError> {
        // An empty params object is acceptable — defaults are reasonable.
        if params.is_null() || params.as_object().is_some_and(|o| o.is_empty()) {
            return Ok(Self {
                summary: None,
                timeout_secs: default_timeout_secs(),
                poll_interval_secs: default_poll_secs(),
            });
        }
        serde_json::from_value(params.clone())
            .map_err(|e| NodeError::BadParams(format!("Approval params invalid: {e}")))
    }
}

#[async_trait]
impl NodeExecutor for Approval {
    fn has_side_effects(&self) -> bool {
        // Approval writes a row visible to humans; treat as side-effecting.
        true
    }

    async fn execute(&self, ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let spec = ApprovalSpec::parse(&ctx.params)?;
        let gate = ctx.approvals.as_ref().ok_or_else(|| {
            NodeError::Runtime(
                "Approval requires the engine to be configured with an ApprovalGate \
                 (Engine::with_approvals)"
                    .into(),
            )
        })?;

        let timeout = Duration::from_secs(spec.timeout_secs.max(1));
        let poll = Duration::from_secs(spec.poll_interval_secs.max(1));

        // R3 audit-fix: payload cap. Default 16 KiB; configurable via
        // `A2W_APPROVAL_MAX_PAYLOAD_BYTES`. Prevents an attacker (or a buggy
        // upstream node) from bloating the approvals table.
        let max_payload = std::env::var("A2W_APPROVAL_MAX_PAYLOAD_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(16 * 1024);

        let mut out = Vec::with_capacity(input.len());
        for (idx, item) in input.into_iter().enumerate() {
            // Build the payload row: the item, optionally annotated with the
            // human-readable summary.
            let mut payload = item.json.clone();
            if let Some(s) = spec.summary.as_deref() {
                if let serde_json::Value::Object(ref mut obj) = payload {
                    obj.insert(
                        "_summary".to_string(),
                        serde_json::Value::String(s.to_string()),
                    );
                }
            }
            let payload_str = serde_json::to_string(&payload).map_err(|e| {
                NodeError::Runtime(format!("approval payload serialise failed: {e}"))
            })?;
            if payload_str.len() > max_payload {
                return Err(NodeError::Runtime(format!(
                    "approval payload ({} bytes) exceeds the maximum allowed \
                     ({} bytes, set A2W_APPROVAL_MAX_PAYLOAD_BYTES to change)",
                    payload_str.len(),
                    max_payload
                )));
            }

            let approval_id = gate
                .request(&ctx.run_id, &ctx.node_id, idx, &payload_str)
                .await
                .map_err(|e| NodeError::Runtime(format!("approval request failed: {e}")))?;

            // Poll until decided or timeout. R3 audit-fix: distinguish
            // timeout from explicit human rejection so the audit trail
            // (and downstream policy) can tell them apart.
            let started = std::time::Instant::now();
            let mut timed_out = false;
            let outcome = loop {
                match gate.poll(&approval_id).await {
                    Ok(Some(o)) => break o,
                    Ok(None) => {
                        if started.elapsed() >= timeout {
                            timed_out = true;
                            // Timeout = rejection (fail-closed).
                            break ApprovalOutcome::Rejected { decided_by: None };
                        }
                        tokio::time::sleep(poll).await;
                    }
                    Err(e) => {
                        return Err(NodeError::Runtime(format!(
                            "approval poll failed for '{approval_id}': {e}"
                        )));
                    }
                }
            };

            // Route the item to the appropriate port + record the audit
            // metadata. R3 audit-fix: an `_outcome_reason` field
            // distinguishes `"timeout"` from `"rejected"` so an operator
            // chasing a stuck workflow can tell at a glance whether the
            // human said no or never showed up.
            let (port, decided_by, reason) = match outcome {
                ApprovalOutcome::Approved { decided_by } => (0, decided_by, "approved"),
                ApprovalOutcome::Rejected { decided_by } if timed_out => (1, decided_by, "timeout"),
                ApprovalOutcome::Rejected { decided_by } => (1, decided_by, "rejected"),
            };
            let mut out_json = item.json.clone();
            if let serde_json::Value::Object(ref mut obj) = out_json {
                obj.insert(
                    "_approval_id".to_string(),
                    serde_json::Value::String(approval_id),
                );
                obj.insert(
                    "_outcome_reason".to_string(),
                    serde_json::Value::String(reason.to_string()),
                );
                if let Some(by) = decided_by {
                    obj.insert("_decided_by".to_string(), serde_json::Value::String(by));
                }
            }
            out.push(Item::produced(out_json, ctx.node_id.clone(), idx).on_port(port));
        }
        Ok(out)
    }

    async fn dry_run(&self, ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        // No gate; alternate approved/rejected per input so both branches are
        // exercised by the dry run.
        let _ = ApprovalSpec::parse(&ctx.params)?;
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(input.len());
        for (i, item) in input.into_iter().enumerate() {
            let port = u8::from(i % 2 == 1) as usize;
            out.push(Item::produced(item.json, ctx.node_id.clone(), i).on_port(port));
        }
        let _ = ctx.mode; // appease lint
        let _ = ExecutionMode::DryRun;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2w_engine::CredentialError;
    use a2w_ir::NodeKind;
    use std::sync::Arc;
    use std::sync::Mutex;

    /// Mock gate that returns a queued outcome on each `poll`.
    struct MockGate {
        outcomes: Mutex<Vec<ApprovalOutcome>>,
    }

    impl MockGate {
        fn new(outcomes: Vec<ApprovalOutcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes),
            }
        }
    }

    #[async_trait]
    impl a2w_engine::ApprovalGate for MockGate {
        async fn request(
            &self,
            _run: &str,
            _node: &str,
            idx: usize,
            _payload: &str,
        ) -> Result<String, CredentialError> {
            Ok(format!("ap_{idx}"))
        }
        async fn poll(
            &self,
            _approval_id: &str,
        ) -> Result<Option<ApprovalOutcome>, CredentialError> {
            let mut g = self.outcomes.lock().unwrap();
            if g.is_empty() {
                Ok(None)
            } else {
                Ok(Some(g.remove(0)))
            }
        }
    }

    fn ctx(
        params: serde_json::Value,
        gate: Option<Arc<dyn a2w_engine::ApprovalGate>>,
    ) -> NodeContext {
        NodeContext {
            run_id: "r".into(),
            node_id: "ap".into(),
            kind: NodeKind::Approval,
            params,
            mode: ExecutionMode::Run,
            credentials: None,
            sub_workflows: None,
            sub_workflow_depth: 0,
            workflow_id: None,
            approvals: gate,
        }
    }

    #[tokio::test]
    async fn approved_routes_port_0_rejected_routes_port_1() {
        let gate = Arc::new(MockGate::new(vec![
            ApprovalOutcome::Approved {
                decided_by: Some("ops".into()),
            },
            ApprovalOutcome::Rejected { decided_by: None },
        ]));
        let ap = Approval;
        let c = ctx(
            serde_json::json!({ "timeout_secs": 1, "poll_interval_secs": 1 }),
            Some(gate),
        );
        let out = ap
            .execute(
                &c,
                vec![
                    Item::root(serde_json::json!({ "x": 1 })),
                    Item::root(serde_json::json!({ "x": 2 })),
                ],
            )
            .await
            .expect("ok");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].output_port, 0);
        assert_eq!(out[1].output_port, 1);
        assert_eq!(out[0].json["_decided_by"], "ops");
    }

    #[tokio::test]
    async fn missing_gate_returns_runtime_error() {
        let ap = Approval;
        let c = ctx(serde_json::json!({}), None);
        let err = ap
            .execute(&c, vec![Item::root(serde_json::json!({}))])
            .await
            .unwrap_err();
        assert!(matches!(err, NodeError::Runtime(_)));
        assert!(err.to_string().contains("ApprovalGate"));
    }

    #[tokio::test]
    async fn empty_input_skips_gate() {
        let ap = Approval;
        let c = ctx(serde_json::json!({}), None);
        let out = ap.execute(&c, Vec::new()).await.expect("ok");
        assert!(out.is_empty());
    }
}
