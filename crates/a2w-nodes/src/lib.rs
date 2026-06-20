//! # a2w-nodes
//!
//! The four core [`NodeExecutor`](a2w_engine::NodeExecutor) implementations for
//! A2W M2, plus [`default_registry`] that wires them to their
//! [`NodeKind`](a2w_ir::NodeKind)s:
//!
//! | Kind             | Executor          | Side effects | Notes                              |
//! |------------------|-------------------|--------------|------------------------------------|
//! | `WebhookTrigger` | [`WebhookTrigger`]| no           | passes seeded trigger items through|
//! | `HttpRequest`    | [`HttpRequest`]   | yes          | shared `reqwest` client; mock dry-run |
//! | `Transform`      | [`Transform`]     | no           | `set`-merge mapping                |
//! | `McpToolCall`    | [`McpToolCall`]   | yes          | mock dry-run; real run via [`McpInvoker`] |
//! | `CodeStep`       | [`CodeStep`]      | yes          | sandboxed WASM via [`WasmRunner`]; mock dry-run |

#![forbid(unsafe_code)]

mod approval;
mod branch;
mod code_step;
mod http_request;
mod llm_call;
mod loop_node;
mod mcp_tool_call;
mod merge;
mod schedule_trigger;
mod sub_workflow;
mod switch;
mod template;
mod transform;
mod wait;
mod webhook_trigger;

pub use approval::Approval;
pub use branch::Branch;
pub use code_step::{CodeError, CodeStep, ExtismRunner, WasmRunner};
pub use http_request::{check_url_allowed, ip_is_blocked, EgressPolicy, HttpRequest};
pub use llm_call::LlmCall;
pub use loop_node::Loop;
pub use mcp_tool_call::{
    check_mcp_command_allowed, check_mcp_command_allowed_with_list, McpError, McpInvoker,
    McpServerSpec, McpToolCall, RmcpInvoker,
};
pub use merge::Merge;
pub use schedule_trigger::ScheduleTrigger;
pub use sub_workflow::SubWorkflow;
pub use switch::Switch;
pub use transform::Transform;
pub use wait::Wait;
pub use webhook_trigger::WebhookTrigger;

use std::sync::Arc;

use a2w_engine::NodeRegistry;
use a2w_ir::NodeKind;

/// Build a [`NodeRegistry`] wiring the four core node kinds to their executors.
///
/// `HttpRequest` shares a single `reqwest::Client` (connection pooling) across
/// every HTTP node in a run.
#[must_use]
pub fn default_registry() -> NodeRegistry {
    NodeRegistry::new()
        .with(NodeKind::WebhookTrigger, Arc::new(WebhookTrigger))
        .with(NodeKind::ScheduleTrigger, Arc::new(ScheduleTrigger))
        .with(NodeKind::HttpRequest, Arc::new(HttpRequest))
        .with(NodeKind::Transform, Arc::new(Transform))
        .with(NodeKind::Merge, Arc::new(Merge))
        .with(NodeKind::McpToolCall, Arc::new(McpToolCall::default()))
        .with(NodeKind::CodeStep, Arc::new(CodeStep::default()))
        .with(NodeKind::Branch, Arc::new(Branch))
        .with(NodeKind::Switch, Arc::new(Switch))
        .with(NodeKind::Loop, Arc::new(Loop))
        .with(NodeKind::Wait, Arc::new(Wait))
        .with(NodeKind::SubWorkflow, Arc::new(SubWorkflow))
        .with(NodeKind::LlmCall, Arc::new(LlmCall::default()))
        .with(NodeKind::Approval, Arc::new(Approval))
}
