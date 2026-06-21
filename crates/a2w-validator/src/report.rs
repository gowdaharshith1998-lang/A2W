//! Finding/report types returned by the validator.
//!
//! These derive `Serialize` + `JsonSchema` so a report can be handed back to an
//! agent verbatim in a later validate→repair loop.

use schemars::JsonSchema;
use serde::Serialize;

/// Severity of a [`Finding`]. `Error` makes a workflow invalid; `Warning` does
/// not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    // NOTE: ordering matters. `Error` sorts before `Warning` so that, under the
    // derived `Ord`, errors lead the deterministically-sorted finding list.
    /// A blocking problem; the workflow is not valid.
    Error,
    /// A non-blocking concern worth surfacing.
    Warning,
}

/// Stable, machine-readable classification of a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FindingCode {
    /// Workflow has no nodes at all.
    EmptyWorkflow,
    /// Two or more nodes share the same `id`.
    DuplicateNodeId,
    /// Workflow has no trigger node.
    NoTrigger,
    /// Workflow has more than one trigger node.
    MultipleTriggers,
    /// A connection's `from_node` does not name an existing node.
    DanglingConnectionSource,
    /// A connection's `to_node` does not name an existing node.
    DanglingConnectionTarget,
    /// A connection's `from_port` is out of range for the source node's kind.
    InvalidOutputPort,
    /// The graph contains a directed cycle.
    Cycle,
    /// A node is not reachable from any trigger.
    UnreachableNode,
    /// A SubWorkflow node references the enclosing workflow by id — a
    /// self-cycle that would loop until the runtime depth cap fires. The
    /// validator rejects this at save time so the operator gets a clear
    /// error before the workflow ever runs.
    SubWorkflowSelfReference,
    /// M1: A node is missing a parameter required by its `NodeKind`
    /// (e.g. an `http_request` with no `url`, a `branch` with no
    /// `condition`). The executor would reject this at runtime with
    /// `BadParams`; the validator surfaces it at authoring time so the
    /// agent's search space is safe by construction.
    MissingRequiredParam,
    /// M1: A node's `params` has the right shape but a field is the wrong
    /// type — e.g. `http_request.url` is an array instead of a string,
    /// `branch.condition.op` is a number, `transform.set` is not an object.
    InvalidParamType,
    /// M1: A trigger node has an incoming connection. Triggers are
    /// workflow entry points; they only emit. The executor effectively
    /// ignores any upstream items, so the connection is dead weight at
    /// best and a logic bug at worst.
    TriggerHasIncomingConnection,
}

/// Where a [`Finding`] applies.
///
/// Adjacently tagged (`kind` + `value`): internal tagging cannot serialize the
/// `Node(String)` newtype variant (serde rejects tagged newtypes wrapping a
/// primitive at runtime), so the payload lives under a `value` key. A report
/// must always be serializable — it is returned to the agent verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Location {
    /// A specific node, by id.
    Node(String),
    /// A specific connection.
    Connection {
        /// Source node id.
        from_node: String,
        /// Source output-port index.
        from_port: usize,
        /// Target node id.
        to_node: String,
    },
    /// The workflow as a whole.
    Workflow,
}

impl Location {
    /// A stable, human-readable key used as the final tiebreaker when sorting
    /// findings, so output order is fully deterministic.
    pub(crate) fn sort_key(&self) -> String {
        match self {
            Location::Node(id) => format!("node:{id}"),
            Location::Connection {
                from_node,
                from_port,
                to_node,
            } => format!("conn:{from_node}:{from_port}->{to_node}"),
            Location::Workflow => "workflow".to_string(),
        }
    }
}

/// A single located, fix-suggesting validation finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Finding {
    /// Blocking vs. advisory.
    pub severity: Severity,
    /// Machine-readable classification.
    pub code: FindingCode,
    /// Human-readable description; always mentions the offending id(s).
    pub message: String,
    /// Where the problem is.
    pub location: Location,
    /// Optional concrete repair hint.
    pub suggestion: Option<String>,
}

/// The result of validating a workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ValidationReport {
    /// All findings, in a deterministic order.
    pub findings: Vec<Finding>,
    /// `true` iff there are no `Error`-severity findings.
    pub is_valid: bool,
}

impl ValidationReport {
    /// Build a report from raw findings: sort deterministically and compute
    /// `is_valid`.
    pub(crate) fn from_findings(mut findings: Vec<Finding>) -> Self {
        // Deterministic order: severity, then code, then location, then message.
        // This stability matters for the validate→repair loop, where identical
        // inputs must yield byte-identical reports.
        findings.sort_by(|a, b| {
            a.severity
                .cmp(&b.severity)
                .then_with(|| a.code.cmp(&b.code))
                .then_with(|| a.location.sort_key().cmp(&b.location.sort_key()))
                .then_with(|| a.message.cmp(&b.message))
        });

        let is_valid = !findings.iter().any(|f| f.severity == Severity::Error);
        Self { findings, is_valid }
    }
}
