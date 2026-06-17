//! The execution engine: models a workflow as a DAG and runs it as async tasks,
//! executing independent branches **concurrently**.
//!
//! ## Scheduling model
//! A node becomes *runnable* once every upstream producer feeding one of its
//! incoming connections has finished. The scheduler repeatedly collects the full
//! set of currently-runnable nodes and drives them **all at once** with
//! [`futures::future::join_all`], looping until every node is done. Independent
//! branches therefore execute in parallel rather than being serialized.
//!
//! ## Item lineage
//! A node's input is the concatenation, in deterministic order, of the output
//! items of all its incoming edges (fan-in / merge). Each incoming item keeps
//! the `source` stamped by *its* producer. After a node executes, the engine
//! **re-stamps** every output item with `ItemSource::Produced { node_id, index }`
//! identifying *this* node — so lineage always points one hop upstream and is
//! never something a node can forge.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use a2w_ir::{ErrorPolicy, NodeKind, Workflow};
use a2w_validator::ValidationReport;
use serde::Serialize;
use thiserror::Error;

use crate::event::{EventLog, StepEvent, StepKind};
use crate::item::{Item, ItemSource};
use crate::node::{CredentialResolver, ExecutionMode, NodeContext, NodeExecutor};

/// Maps a [`NodeKind`] to the executor that implements its behaviour.
///
/// Behaviour is per-kind; per-node configuration arrives via [`NodeContext`].
#[derive(Clone, Default)]
pub struct NodeRegistry {
    executors: HashMap<NodeKind, Arc<dyn NodeExecutor>>,
}

impl NodeRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `executor` as the implementation for `kind`, replacing any
    /// previous registration. Returns `self` for chaining.
    #[must_use]
    pub fn with(mut self, kind: NodeKind, executor: Arc<dyn NodeExecutor>) -> Self {
        self.executors.insert(kind, executor);
        self
    }

    /// Insert or replace the executor for `kind`.
    pub fn register(&mut self, kind: NodeKind, executor: Arc<dyn NodeExecutor>) {
        self.executors.insert(kind, executor);
    }

    /// Look up the executor for `kind`, if any.
    #[must_use]
    pub fn get(&self, kind: NodeKind) -> Option<Arc<dyn NodeExecutor>> {
        self.executors.get(&kind).cloned()
    }
}

/// Terminal status of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Every scheduled node finished (a node may have been skipped/zeroed via
    /// an `on_error: continue` policy, but the run reached the end).
    Completed,
    /// The run aborted because a node failed under a `Stop` error policy.
    Failed,
}

/// The result of a completed (or failed) run.
#[derive(Debug, Clone, Serialize)]
pub struct RunResult {
    /// The id assigned to this run.
    pub run_id: String,
    /// Whether the run completed or failed.
    pub status: RunStatus,
    /// Final output items of every node that executed, keyed by node id.
    pub node_outputs: HashMap<String, Vec<Item>>,
    /// All step events recorded during the run, in insertion order.
    pub events: Vec<StepEvent>,
}

/// Errors that abort a run before or during execution.
#[derive(Debug, Error)]
pub enum EngineError {
    /// The workflow failed validation; it was not executed.
    #[error("workflow is invalid: {} finding(s)", .0.findings.len())]
    Invalid(ValidationReport),
    /// A node failed under a `Stop` (default) error policy.
    #[error("node '{node_id}' failed: {error}")]
    NodeFailed {
        /// The node that failed.
        node_id: String,
        /// The underlying error message.
        error: String,
    },
    /// No executor is registered for a node kind present in the workflow.
    #[error("no executor registered for node kind {0:?}")]
    NoExecutorForKind(NodeKind),
    /// An internal invariant was violated (should not happen for valid input).
    #[error("internal engine error: {0}")]
    Internal(String),
}

/// Global monotonic counter used to mint dependency-light run ids.
static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The execution engine. Holds the registry of node behaviours and an optional
/// run-time credential resolver.
pub struct Engine {
    registry: NodeRegistry,
    credentials: Option<Arc<dyn CredentialResolver>>,
}

/// Outcome of executing a single node, used internally by the scheduler.
enum NodeOutcome {
    /// The node produced these (already lineage-stamped) items.
    Produced(Vec<Item>),
    /// The node failed and its `Stop` policy aborts the run.
    Fail(String),
}

impl Engine {
    /// Construct an engine over a node registry.
    #[must_use]
    pub fn new(registry: NodeRegistry) -> Self {
        Self {
            registry,
            credentials: None,
        }
    }

    /// Attach a run-time credential resolver (vault-backed) so nodes can resolve
    /// `credential_ref`s at execution time without plaintext secrets ever
    /// appearing in the workflow IR or persisted run records.
    #[must_use]
    pub fn with_credentials(mut self, resolver: Arc<dyn CredentialResolver>) -> Self {
        self.credentials = Some(resolver);
        self
    }

    /// Run `wf` to completion.
    ///
    /// `trigger_input` seeds the trigger node: each JSON value becomes a root
    /// [`Item`]. `mode` selects real vs. dry execution. Step events are written
    /// to `log` as the run proceeds.
    ///
    /// The workflow is validated first via [`a2w_validator::validate`]; an
    /// invalid workflow returns [`EngineError::Invalid`] **without executing**
    /// (and without recording any step event).
    ///
    /// # Errors
    /// - [`EngineError::Invalid`] if validation fails.
    /// - [`EngineError::NoExecutorForKind`] if a node's kind has no executor.
    /// - [`EngineError::NodeFailed`] if a node fails under a `Stop` policy.
    /// - [`EngineError::Internal`] on a violated invariant.
    pub async fn run(
        &self,
        wf: &Workflow,
        trigger_input: Vec<serde_json::Value>,
        mode: ExecutionMode,
        log: &dyn EventLog,
    ) -> Result<RunResult, EngineError> {
        // --- Validate first; do not execute (or log) an invalid workflow. ----
        let report = a2w_validator::validate(wf);
        if !report.is_valid {
            return Err(EngineError::Invalid(report));
        }

        let run_id = mint_run_id(&wf.id);

        // --- Index nodes and pre-flight the registry. ------------------------
        // A valid workflow has unique ids, so first-wins indexing is exact.
        let mut node_by_id: HashMap<&str, &a2w_ir::Node> = HashMap::new();
        for node in &wf.nodes {
            node_by_id.entry(node.id.as_str()).or_insert(node);
        }
        for node in &wf.nodes {
            if self.registry.get(node.kind).is_none() {
                return Err(EngineError::NoExecutorForKind(node.kind));
            }
        }

        // --- Build dependency structure from connections. --------------------
        // For each node: its incoming edges (producers) and outgoing targets.
        // `incoming` drives readiness; we collect edges as (from_node, from_port)
        // so input gathering is deterministic.
        let mut incoming: HashMap<&str, Vec<IncomingEdge>> = HashMap::new();
        let mut in_degree: HashMap<&str, usize> = HashMap::new();
        for node in &wf.nodes {
            in_degree.entry(node.id.as_str()).or_insert(0);
            incoming.entry(node.id.as_str()).or_default();
        }
        for conn in &wf.connections {
            incoming
                .entry(conn.to_node.as_str())
                .or_default()
                .push(IncomingEdge {
                    from_node: conn.from_node.as_str(),
                    from_port: conn.from_port,
                });
            *in_degree.entry(conn.to_node.as_str()).or_insert(0) += 1;
        }

        // The single trigger seeds root items.
        let trigger_id = wf
            .nodes
            .iter()
            .find(|n| n.kind.is_trigger())
            .map(|n| n.id.as_str())
            .ok_or_else(|| EngineError::Internal("validated workflow has no trigger".into()))?;

        // Pre-seeded trigger items keyed by node id (only the trigger gets them).
        let trigger_items: Vec<Item> = trigger_input.into_iter().map(Item::root).collect();

        // --- Concurrent scheduling state. ------------------------------------
        // `outputs` holds each finished node's (re-stamped) output items.
        let mut outputs: HashMap<String, Vec<Item>> = HashMap::new();
        let mut done: HashSet<&str> = HashSet::new();
        let mut remaining: usize = wf.nodes.len();

        // Loop: each pass runs ALL currently-runnable nodes concurrently.
        while remaining > 0 {
            // A node is runnable iff it is not done and every incoming producer
            // is done.
            let runnable: Vec<&a2w_ir::Node> = wf
                .nodes
                .iter()
                .filter(|n| !done.contains(n.id.as_str()))
                .filter(|n| {
                    incoming
                        .get(n.id.as_str())
                        .map(|edges| edges.iter().all(|e| done.contains(e.from_node)))
                        .unwrap_or(true)
                })
                .collect();

            if runnable.is_empty() {
                // Should be impossible for a validated (acyclic, connected)
                // workflow; guard rather than spin forever.
                return Err(EngineError::Internal(
                    "no runnable nodes but work remains (possible cycle)".into(),
                ));
            }

            // Build each runnable node's input by gathering incoming items in a
            // deterministic order, then prepare its future.
            let mut futures_vec = Vec::with_capacity(runnable.len());
            for node in &runnable {
                let input = self.gather_input(
                    node,
                    trigger_id,
                    &trigger_items,
                    incoming.get(node.id.as_str()).map(Vec::as_slice).unwrap_or(&[]),
                    &outputs,
                );
                futures_vec.push(self.run_one_node(node, input, &run_id, mode, log));
            }

            // Drive the whole wave concurrently.
            let results = futures::future::join_all(futures_vec).await;

            // Commit results. Honor Stop policy by aborting the whole run.
            for (node, outcome) in runnable.iter().zip(results) {
                match outcome {
                    NodeOutcome::Produced(items) => {
                        outputs.insert(node.id.clone(), items);
                    }
                    NodeOutcome::Fail(err) => {
                        // Stop policy: abort. Events already recorded the failure.
                        return Err(EngineError::NodeFailed {
                            node_id: node.id.clone(),
                            error: err,
                        });
                    }
                }
                done.insert(node.id.as_str());
                remaining -= 1;
            }
        }

        Ok(RunResult {
            run_id,
            status: RunStatus::Completed,
            node_outputs: outputs,
            events: log.events(),
        })
    }

    /// Gather a node's input items from its incoming edges in deterministic
    /// order (sorted by `from_node` id, then `from_port`). The trigger node's
    /// input is the pre-seeded root items.
    fn gather_input(
        &self,
        node: &a2w_ir::Node,
        trigger_id: &str,
        trigger_items: &[Item],
        edges: &[IncomingEdge],
        outputs: &HashMap<String, Vec<Item>>,
    ) -> Vec<Item> {
        // The trigger receives the seeded root items (it has no producers).
        if node.id == trigger_id {
            return trigger_items.to_vec();
        }

        // Deterministic fan-in: sort edges by (from_node, from_port).
        let mut ordered: Vec<&IncomingEdge> = edges.iter().collect();
        ordered.sort_by(|a, b| {
            a.from_node
                .cmp(b.from_node)
                .then_with(|| a.from_port.cmp(&b.from_port))
        });

        let mut gathered = Vec::new();
        for edge in ordered {
            if let Some(items) = outputs.get(edge.from_node) {
                // Each item keeps the lineage stamped by ITS producer; fan-in
                // simply concatenates. Port-level routing (Branch/Switch) is a
                // later milestone — for M2 all output ports carry the node's
                // full output.
                gathered.extend(items.iter().cloned());
            }
        }
        gathered
    }

    /// Execute a single node: record Started, time it, run execute/dry_run,
    /// re-stamp lineage, record Finished/Failed, and apply the `on_error` policy
    /// to decide between producing items, zeroing out (Continue), or failing.
    async fn run_one_node(
        &self,
        node: &a2w_ir::Node,
        input: Vec<Item>,
        run_id: &str,
        mode: ExecutionMode,
        log: &dyn EventLog,
    ) -> NodeOutcome {
        let input_count = input.len();

        // Registry membership was verified up front; treat absence as internal.
        let executor = match self.registry.get(node.kind) {
            Some(e) => e,
            None => {
                // Defensive: should be unreachable after the pre-flight check.
                let msg = format!("no executor for kind {:?}", node.kind);
                log.record(failed_event(run_id, &node.id, input_count, &msg));
                return self.apply_error_policy(node, msg);
            }
        };

        let ctx = NodeContext {
            run_id: run_id.to_string(),
            node_id: node.id.clone(),
            kind: node.kind,
            params: node.params.clone(),
            mode,
            credentials: self.credentials.clone(),
        };

        // Started event.
        log.record(StepEvent {
            run_id: run_id.to_string(),
            node_id: node.id.clone(),
            kind: StepKind::Started,
            latency_ms: 0,
            input_items: input_count,
            output_items: 0,
            external_calls: 0,
            tokens: 0,
            error: None,
        });

        let start = Instant::now();
        let result = match mode {
            ExecutionMode::Run => executor.execute(&ctx, input).await,
            ExecutionMode::DryRun => executor.dry_run(&ctx, input).await,
        };
        let latency_ms = elapsed_ms(start);

        match result {
            Ok(items) => {
                // Re-stamp lineage: the engine owns the source field. Whatever
                // a node set is overwritten with this node's identity + index.
                let stamped: Vec<Item> = items
                    .into_iter()
                    .enumerate()
                    .map(|(index, mut item)| {
                        item.source = ItemSource::Produced {
                            node_id: node.id.clone(),
                            item_index: index,
                        };
                        item
                    })
                    .collect();

                log.record(StepEvent {
                    run_id: run_id.to_string(),
                    node_id: node.id.clone(),
                    kind: StepKind::Finished,
                    latency_ms,
                    input_items: input_count,
                    output_items: stamped.len(),
                    external_calls: 0,
                    tokens: 0,
                    error: None,
                });
                NodeOutcome::Produced(stamped)
            }
            Err(err) => {
                let msg = err.to_string();
                log.record(StepEvent {
                    run_id: run_id.to_string(),
                    node_id: node.id.clone(),
                    kind: StepKind::Failed,
                    latency_ms,
                    input_items: input_count,
                    output_items: 0,
                    external_calls: 0,
                    tokens: 0,
                    error: Some(msg.clone()),
                });
                self.apply_error_policy(node, msg)
            }
        }
    }

    /// Map a node failure onto its `on_error` policy. `Stop` (the default)
    /// aborts; `Continue` treats the node as producing zero items; `Route` is
    /// not implemented for M2 and is treated as `Stop`.
    fn apply_error_policy(&self, node: &a2w_ir::Node, error: String) -> NodeOutcome {
        match node.on_error {
            Some(ErrorPolicy::Continue) => NodeOutcome::Produced(Vec::new()),
            // Route is documented as NotImplemented for M2; fall back to Stop so
            // the failure is not silently swallowed.
            Some(ErrorPolicy::Route) | Some(ErrorPolicy::Stop) | None => NodeOutcome::Fail(error),
        }
    }
}

/// An incoming connection edge into a node.
struct IncomingEdge<'a> {
    from_node: &'a str,
    from_port: usize,
}

/// Mint a dependency-light run id from the workflow id plus a process-global
/// monotonic counter. No uuid crate required.
fn mint_run_id(wf_id: &str) -> String {
    let n = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("run_{wf_id}_{n}")
}

/// Saturating millisecond latency from an `Instant`.
fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Build a `Failed` step event (used for the defensive no-executor path).
fn failed_event(run_id: &str, node_id: &str, input_items: usize, msg: &str) -> StepEvent {
    StepEvent {
        run_id: run_id.to_string(),
        node_id: node_id.to_string(),
        kind: StepKind::Failed,
        latency_ms: 0,
        input_items,
        output_items: 0,
        external_calls: 0,
        tokens: 0,
        error: Some(msg.to_string()),
    }
}
