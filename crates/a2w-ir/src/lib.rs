//! # a2w-ir
//!
//! The A2W (Agent-to-Workflow) Intermediate Representation: a **narrow,
//! versioned, declarative JSON IR** describing a workflow as a graph of typed
//! nodes connected by explicit, port-indexed edges.
//!
//! Rust structs are the single source of truth. [`schemars`] derives the JSON
//! Schema an LLM emits against; [`serde`] (de)serializes instances.
//!
//! ## Design goals
//! - **Stable node IDs.** Nodes are referenced by `id`, never by display name.
//! - **Explicit output-port indices.** Connections address a source node's
//!   output by zero-based index ([`Connection::from_port`]), not by name.
//! - **Shallow structure.** A flat list of nodes plus a flat list of
//!   connections — easy for an LLM to emit reliably.
//!
//! ## Example
//! ```
//! let wf = a2w_ir::sample_workflow();
//! let json = wf.to_json_pretty().unwrap();
//! let round_tripped = a2w_ir::Workflow::from_json(&json).unwrap();
//! assert_eq!(wf, round_tripped);
//! ```

#![forbid(unsafe_code)]

mod error;
mod node;
mod value;

pub use error::IrError;
pub use node::{Node, NodeKind, DYNAMIC_PORTS};
pub use value::{ErrorPolicy, RetryPolicy};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Version of the IR schema this crate emits and accepts.
///
/// Instances carry their own [`Workflow::schema_version`]; consumers compare it
/// against this constant to decide compatibility.
pub const SCHEMA_VERSION: u32 = 1;

/// A directed edge from one node's output port to another node's single input.
///
/// Fan-out and fan-in are expressed purely by repetition:
/// - multiple connections sharing a `(from_node, from_port)` fan **out**;
/// - multiple connections sharing a `to_node` fan **in**.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    /// `id` of the source node.
    pub from_node: String,
    /// Zero-based index of the source node's output port. Must be less than the
    /// source kind's [`NodeKind::output_port_count`] unless that count is
    /// dynamic.
    pub from_port: usize,
    /// `id` of the target node (which has a single, implicit input).
    pub to_node: String,
}

impl Connection {
    /// Convenience constructor.
    #[must_use]
    pub fn new(from_node: impl Into<String>, from_port: usize, to_node: impl Into<String>) -> Self {
        Self {
            from_node: from_node.into(),
            from_port,
            to_node: to_node.into(),
        }
    }
}

/// A complete workflow: the top-level IR document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    /// IR schema version this document was authored against.
    pub schema_version: u32,
    /// Stable workflow identifier.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// The nodes, in no particular order (ordering is not semantically
    /// significant; edges define flow).
    pub nodes: Vec<Node>,
    /// The directed edges between node ports.
    pub connections: Vec<Connection>,
}

impl Workflow {
    /// Deserialize a [`Workflow`] from a JSON string.
    ///
    /// # Errors
    /// Returns [`IrError::Json`] if the input is not valid JSON or does not
    /// match the schema (including unknown fields, which are rejected).
    pub fn from_json(s: &str) -> Result<Self, IrError> {
        Ok(serde_json::from_str(s)?)
    }

    /// Serialize this workflow to a pretty-printed JSON string.
    ///
    /// # Errors
    /// Returns [`IrError::Json`] if serialization fails (practically only on
    /// non-string map keys, which this type does not produce).
    pub fn to_json_pretty(&self) -> Result<String, IrError> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

/// The derived JSON Schema for [`Workflow`].
///
/// This is the schema an LLM is instructed to emit against. Built via
/// [`schemars::schema_for!`], which returns a fully-resolved
/// [`schemars::schema::RootSchema`].
#[must_use]
pub fn workflow_json_schema() -> schemars::schema::RootSchema {
    schemars::schema_for!(Workflow)
}

/// A small, valid sample workflow: `WebhookTrigger -> HttpRequest -> Transform`.
///
/// Used by tests and intended to seed a later few-shot corpus. Always valid
/// against the M1 validator.
#[must_use]
pub fn sample_workflow() -> Workflow {
    let trigger = Node::new("trigger", NodeKind::WebhookTrigger);
    let fetch = Node::new("fetch", NodeKind::HttpRequest);
    let shape = Node::new("shape", NodeKind::Transform);

    Workflow {
        schema_version: SCHEMA_VERSION,
        id: "wf_sample".to_string(),
        name: "Sample webhook → fetch → transform".to_string(),
        nodes: vec![trigger, fetch, shape],
        connections: vec![
            Connection::new("trigger", 0, "fetch"),
            Connection::new("fetch", 0, "shape"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_sample_workflow() {
        let wf = sample_workflow();
        let json = wf.to_json_pretty().expect("serialize sample");
        let back = Workflow::from_json(&json).expect("deserialize sample");
        assert_eq!(wf, back);
    }

    #[test]
    fn schema_smoke_mentions_workflow_and_nodes() {
        let schema = workflow_json_schema();
        let json = serde_json::to_string(&schema).expect("serialize schema");
        assert!(json.contains("Workflow"), "schema should mention Workflow");
        assert!(json.contains("nodes"), "schema should mention nodes");
        assert!(
            json.contains("connections"),
            "schema should mention connections"
        );
    }

    #[test]
    fn deserialize_handwritten_json() {
        let raw = r#"
        {
            "schema_version": 1,
            "id": "wf_hand",
            "name": "Hand written",
            "nodes": [
                {
                    "id": "t",
                    "kind": "schedule_trigger",
                    "params": { "cron": "*/5 * * * *" }
                },
                {
                    "id": "call",
                    "kind": "http_request",
                    "params": { "url": "https://example.com" },
                    "retry": { "max_attempts": 3, "backoff_ms": 250 },
                    "on_error": "continue"
                }
            ],
            "connections": [
                { "from_node": "t", "from_port": 0, "to_node": "call" }
            ]
        }
        "#;

        let wf = Workflow::from_json(raw).expect("parse hand-written workflow");
        assert_eq!(wf.schema_version, 1);
        assert_eq!(wf.id, "wf_hand");
        assert_eq!(wf.name, "Hand written");
        assert_eq!(wf.nodes.len(), 2);
        assert_eq!(wf.nodes[0].id, "t");
        assert_eq!(wf.nodes[0].kind, NodeKind::ScheduleTrigger);
        assert_eq!(wf.nodes[1].kind, NodeKind::HttpRequest);
        assert_eq!(
            wf.nodes[1].retry,
            Some(RetryPolicy {
                max_attempts: 3,
                backoff_ms: 250
            })
        );
        assert_eq!(wf.nodes[1].on_error, Some(ErrorPolicy::Continue));
        assert_eq!(wf.connections.len(), 1);
        assert_eq!(wf.connections[0].from_node, "t");
        assert_eq!(wf.connections[0].to_node, "call");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let raw = r#"
        {
            "schema_version": 1,
            "id": "wf",
            "name": "n",
            "nodes": [],
            "connections": [],
            "surprise": true
        }
        "#;
        assert!(Workflow::from_json(raw).is_err());
    }

    #[test]
    fn node_kind_ports_and_triggers() {
        assert!(NodeKind::WebhookTrigger.is_trigger());
        assert!(NodeKind::ScheduleTrigger.is_trigger());
        assert!(!NodeKind::HttpRequest.is_trigger());

        assert_eq!(NodeKind::Branch.output_port_count(), 2);
        assert_eq!(NodeKind::HttpRequest.output_port_count(), 1);
        assert!(NodeKind::Switch.has_dynamic_ports());
        assert!(!NodeKind::Branch.has_dynamic_ports());
    }
}
