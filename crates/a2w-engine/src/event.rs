//! Per-step execution events and the event-log abstraction.
//!
//! Each node execution emits a [`StepEvent`] at the start and end of its run,
//! carrying timing and item-count metrics. Events are recorded through the
//! [`EventLog`] trait; an in-memory [`MemoryEventLog`] is provided here. SQLite
//! persistence is deferred to M4 and would simply be another `EventLog` impl —
//! the engine only ever talks to the trait.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The phase of a step that a [`StepEvent`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// Emitted immediately before a node's executor is invoked.
    Started,
    /// Emitted after a node's executor returns successfully.
    Finished,
    /// Emitted when a node's executor returns an error.
    Failed,
}

/// A single observability event for one node execution phase.
///
/// `latency_ms` is measured with [`std::time::Instant`] (permitted here: this is
/// the engine, not a sandboxed user script). `Started` events carry a latency of
/// `0` and zero output counts; `Finished`/`Failed` carry the measured values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepEvent {
    /// The run this event belongs to.
    pub run_id: String,
    /// The node that produced this event.
    pub node_id: String,
    /// Which phase of the step this is.
    pub kind: StepKind,
    /// Wall-clock latency of the step in milliseconds (0 for `Started`).
    pub latency_ms: u64,
    /// Number of items handed to the node as input.
    pub input_items: usize,
    /// Number of items the node produced (0 for `Started`/`Failed`).
    pub output_items: usize,
    /// Count of external calls (e.g. HTTP/MCP) the node made. Reserved for
    /// node-reported metrics; the engine records 0 by default.
    pub external_calls: u32,
    /// Tokens consumed (e.g. by an LLM call). Reserved; 0 by default.
    pub tokens: u64,
    /// Error message when `kind == Failed`, otherwise `None`.
    pub error: Option<String>,
}

/// A sink for [`StepEvent`]s. Implementations must be cheaply shareable across
/// concurrently-executing node tasks (`Send + Sync`).
pub trait EventLog: Send + Sync {
    /// Record a single event.
    fn record(&self, ev: StepEvent);
    /// Snapshot all events recorded so far, in insertion order.
    fn events(&self) -> Vec<StepEvent>;
}

/// An in-memory [`EventLog`] backed by a `Mutex<Vec<StepEvent>>`.
#[derive(Debug, Default)]
pub struct MemoryEventLog {
    events: Mutex<Vec<StepEvent>>,
}

impl MemoryEventLog {
    /// Create an empty in-memory event log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl EventLog for MemoryEventLog {
    fn record(&self, ev: StepEvent) {
        // A poisoned lock means another thread panicked while holding it. We
        // recover the guard rather than propagating, since dropping events on a
        // poisoned observability sink is worse than continuing.
        match self.events.lock() {
            Ok(mut guard) => guard.push(ev),
            Err(poisoned) => poisoned.into_inner().push(ev),
        }
    }

    fn events(&self) -> Vec<StepEvent> {
        match self.events.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}
