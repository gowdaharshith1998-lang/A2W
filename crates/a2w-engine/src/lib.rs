//! # a2w-engine
//!
//! The A2W execution engine. It takes a validated workflow [IR](a2w_ir) and runs
//! it as a **concurrent async DAG**: independent branches execute in parallel
//! (the flagship contrast with sequential-only engines like n8n), while data
//! flows between nodes as an *item array* with **guaranteed lineage**.
//!
//! ## Pieces
//! - [`Item`] / [`ItemSource`] — the data unit and its lineage edge.
//! - [`StepEvent`] / [`EventLog`] / [`MemoryEventLog`] — observability.
//! - [`NodeExecutor`] / [`NodeContext`] / [`NodeError`] — the node abstraction.
//! - [`Engine`] / [`NodeRegistry`] / [`RunResult`] — the runtime itself.
//!
//! Node *behaviours* live in the `a2w-nodes` crate; this crate provides the
//! engine and the trait they implement.

#![forbid(unsafe_code)]

mod engine;
mod event;
mod item;
mod node;

pub use engine::{Engine, EngineError, NodeRegistry, RunResult, RunStatus};
pub use event::{EventLog, MemoryEventLog, StepEvent, StepKind};
pub use item::{Item, ItemSource};
pub use node::{
    CredentialError, CredentialResolver, ExecutionMode, NodeContext, NodeError, NodeExecutor,
};
