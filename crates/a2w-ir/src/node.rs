//! Node taxonomy and node structure for the A2W IR.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::value::{ErrorPolicy, RetryPolicy};

/// Sentinel returned by [`NodeKind::output_port_count`] for kinds whose port
/// count is *dynamic* — determined by the node's `params` rather than by the
/// kind alone.
///
/// For M1 only [`NodeKind::Switch`] is dynamic. Consumers (notably the
/// validator) treat this value as "do not enforce an upper bound on
/// `from_port`": any non-negative port index is structurally acceptable, and
/// the real per-case bound is validated once typed params land.
pub const DYNAMIC_PORTS: usize = usize::MAX;

/// The kind (behavioural class) of a node.
///
/// This is the closed taxonomy an LLM emits against. Variants are
/// `snake_case` on the wire (e.g. `"http_request"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Entry point fired by an inbound HTTP webhook.
    WebhookTrigger,
    /// Entry point fired on a time schedule (cron-like).
    ScheduleTrigger,
    /// Perform an outbound HTTP request.
    HttpRequest,
    /// Invoke a tool exposed over the Model Context Protocol.
    McpToolCall,
    /// Pure data transformation / mapping.
    Transform,
    /// Two-way conditional split (true / false).
    Branch,
    /// Multi-way conditional split keyed on a value.
    Switch,
    /// Iterate over a collection.
    Loop,
    /// Combine multiple inbound paths back into one.
    Merge,
    /// Pause for a duration or until a condition.
    Wait,
    /// Invoke another workflow as a sub-routine.
    SubWorkflow,
    /// Call a large language model.
    LlmCall,
    /// Run an inline code step.
    CodeStep,
    /// Human-in-the-loop approval gate.
    Approval,
}

impl NodeKind {
    /// Whether this kind is a workflow entry point (a trigger).
    ///
    /// A valid workflow has exactly one trigger node.
    #[must_use]
    pub fn is_trigger(&self) -> bool {
        matches!(self, NodeKind::WebhookTrigger | NodeKind::ScheduleTrigger)
    }

    /// Number of output ports this kind exposes.
    ///
    /// Connections address output ports by explicit zero-based index, so this
    /// defines the valid range of `Connection::from_port` for a node of this
    /// kind.
    ///
    /// Design choices for M1:
    /// - [`NodeKind::Branch`] has exactly **2** ports: index `0` = "true",
    ///   index `1` = "false". This ordering is a stable convention.
    /// - [`NodeKind::Switch`] is **dynamic**: its real port count depends on
    ///   the cases declared in `params`, which are still untyped in M1. It
    ///   therefore returns [`DYNAMIC_PORTS`] and the validator skips the strict
    ///   upper-bound check for it (any `from_port` is accepted for now).
    /// - Every other kind, including triggers, exposes a single port (index
    ///   `0`). Triggers still have one output port: the path their event
    ///   kicks off.
    #[must_use]
    pub fn output_port_count(&self) -> usize {
        match self {
            NodeKind::Branch => 2,
            NodeKind::Switch => DYNAMIC_PORTS,
            _ => 1,
        }
    }

    /// Whether this kind's port count is dynamic (see [`DYNAMIC_PORTS`]).
    #[must_use]
    pub fn has_dynamic_ports(&self) -> bool {
        self.output_port_count() == DYNAMIC_PORTS
    }
}

/// A single node in a workflow graph.
///
/// `params` is intentionally an untyped [`serde_json::Value`] for M1; typed
/// per-kind parameter structs arrive in a later milestone. `retry` and
/// `on_error` are optional execution policies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// Stable, unique-within-workflow identifier. Connections reference nodes
    /// by this id, never by display name.
    pub id: String,
    /// The behavioural class of the node.
    pub kind: NodeKind,
    /// Free-form, kind-specific configuration. Untyped in M1.
    pub params: serde_json::Value,
    /// Optional retry policy for transient failures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryPolicy>,
    /// Optional policy for terminal failures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_error: Option<ErrorPolicy>,
}

impl Node {
    /// Construct a minimal node with empty params and no policies.
    ///
    /// Convenience for tests and sample construction.
    #[must_use]
    pub fn new(id: impl Into<String>, kind: NodeKind) -> Self {
        Self {
            id: id.into(),
            kind,
            params: serde_json::Value::Object(serde_json::Map::new()),
            retry: None,
            on_error: None,
        }
    }
}
