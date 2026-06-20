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
use std::time::{Duration, Instant};

use a2w_ir::{ErrorPolicy, NodeKind, RetryPolicy, Workflow};
use a2w_validator::ValidationReport;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::Semaphore;

/// Default maximum number of nodes the engine will run concurrently in a single
/// wave. Tunable per-engine via [`Engine::with_max_concurrency`]; clamped to
/// >=1.
pub const DEFAULT_MAX_CONCURRENCY: usize = 64;

use crate::event::{EventLog, StepEvent, StepKind};
use crate::item::{Item, ItemSource};
use crate::node::{
    ApprovalGate, CredentialResolver, ExecutionMode, NodeContext, NodeExecutor,
    SubWorkflowResolver,
};

/// Maximum recursion depth for SubWorkflow invocation. Beyond this, the
/// executor refuses to descend (defense against an accidental
/// `workflow A includes A` infinite loop).
pub const DEFAULT_MAX_SUB_WORKFLOW_DEPTH: u8 = 5;

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

/// The execution engine. Holds the registry of node behaviours, an optional
/// run-time credential resolver, and the per-wave concurrency cap (number of
/// nodes the scheduler will drive in parallel before back-pressuring the
/// remainder of the wave to the next slot).
pub struct Engine {
    registry: NodeRegistry,
    credentials: Option<Arc<dyn CredentialResolver>>,
    sub_workflows: Option<Arc<dyn SubWorkflowResolver>>,
    approvals: Option<Arc<dyn ApprovalGate>>,
    /// Initial sub-workflow recursion depth seeded into every NodeContext this
    /// engine constructs. The top-level engine uses 0; SubWorkflow's sub-engine
    /// uses `parent_depth + 1` so the recursion cap actually fires.
    initial_sub_workflow_depth: u8,
    /// Concurrency permit. `Arc<Semaphore>` so it can be cheaply cloned into
    /// each spawned node task within a wave.
    semaphore: Arc<Semaphore>,
}

/// Outcome of executing a single node, used internally by the scheduler.
enum NodeOutcome {
    /// The node produced these (already lineage-stamped) items.
    Produced(Vec<Item>),
    /// The node failed and its `Stop` policy aborts the run.
    Fail(String),
}

impl Engine {
    /// Construct an engine over a node registry with the default per-wave
    /// concurrency cap ([`DEFAULT_MAX_CONCURRENCY`]).
    #[must_use]
    pub fn new(registry: NodeRegistry) -> Self {
        Self {
            registry,
            credentials: None,
            sub_workflows: None,
            approvals: None,
            initial_sub_workflow_depth: 0,
            semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENCY)),
        }
    }

    /// Pre-load the sub-workflow recursion depth that every `NodeContext`
    /// this engine constructs will start at. Used by SubWorkflow to mark its
    /// sub-engine as one level deeper than itself so the recursion cap works.
    #[must_use]
    pub fn with_initial_sub_workflow_depth(mut self, depth: u8) -> Self {
        self.initial_sub_workflow_depth = depth;
        self
    }

    /// Attach an approval gate so the `Approval` executor can record pending
    /// approvals and poll for decisions. Without one, `Approval` returns a
    /// clear "approvals not configured" runtime error.
    #[must_use]
    pub fn with_approvals(mut self, gate: Arc<dyn ApprovalGate>) -> Self {
        self.approvals = Some(gate);
        self
    }

    /// Attach a run-time credential resolver (vault-backed) so nodes can resolve
    /// `credential_ref`s at execution time without plaintext secrets ever
    /// appearing in the workflow IR or persisted run records.
    #[must_use]
    pub fn with_credentials(mut self, resolver: Arc<dyn CredentialResolver>) -> Self {
        self.credentials = Some(resolver);
        self
    }

    /// Attach a sub-workflow resolver so the SubWorkflow executor can look up
    /// stored workflows by id.
    #[must_use]
    pub fn with_sub_workflows(mut self, resolver: Arc<dyn SubWorkflowResolver>) -> Self {
        self.sub_workflows = Some(resolver);
        self
    }

    /// Override the per-wave concurrency cap. Clamped to >=1 so the scheduler
    /// can always make forward progress. A small cap (e.g. 1) sequentializes
    /// the engine — useful for tests; the production default is 64.
    #[must_use]
    pub fn with_max_concurrency(mut self, max: usize) -> Self {
        let max = max.max(1);
        self.semaphore = Arc::new(Semaphore::new(max));
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
        self.run_with_id(wf, mint_run_id(&wf.id), trigger_input, mode, log, None)
            .await
    }

    /// Like [`Engine::run`] but the caller pre-mints (or pre-claims) the
    /// `run_id`. Required for idempotency 2-phase claim (handler reserves
    /// the id, then runs) and for SubWorkflow (parent invokes child under a
    /// derived id).
    ///
    /// `resume_from` (optional) hydrates outputs from a previous, crashed
    /// run with the same `run_id`: nodes that have a `Finished` step record
    /// are skipped and their outputs are reused. Side-effecting nodes that
    /// were `Finished` do NOT re-fire. This is the P2 resume-from-step path.
    ///
    /// # Errors
    /// Same as [`Engine::run`].
    pub async fn run_with_id(
        &self,
        wf: &Workflow,
        run_id: String,
        trigger_input: Vec<serde_json::Value>,
        mode: ExecutionMode,
        log: &dyn EventLog,
        resume_from: Option<&dyn ResumeSource>,
    ) -> Result<RunResult, EngineError> {
        // --- Validate first; do not execute (or log) an invalid workflow. ----
        let report = a2w_validator::validate(wf);
        if !report.is_valid {
            return Err(EngineError::Invalid(report));
        }

        // R4 audit-fix: resume + non-empty trigger_input is incoherent.
        // Hydrated downstream nodes carry lineage referring to the OLD
        // trigger; injecting a new trigger_input would make downstream items
        // inconsistent with the trigger's seed. Either resume (empty
        // trigger_input) or run fresh (no resume). Both is rejected.
        if resume_from.is_some() && !trigger_input.is_empty() {
            return Err(EngineError::Internal(
                "resume with a non-empty trigger_input is not supported: \
                 either resume (trigger_input must be empty so downstream \
                 hydrated outputs stay consistent with the original trigger) \
                 or start a fresh run with a new run_id"
                    .to_string(),
            ));
        }

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
            let edge = IncomingEdge {
                from_node: conn.from_node.as_str(),
                from_port: conn.from_port,
            };
            // Audit-2 dedup: duplicate connections (same triple) would
            // otherwise cause input duplication via `gather_input`. We dedupe
            // at the edge-list level so each (from_node, from_port) is only
            // gathered once per target.
            let bucket = incoming.entry(conn.to_node.as_str()).or_default();
            if !bucket
                .iter()
                .any(|e| e.from_node == edge.from_node && e.from_port == edge.from_port)
            {
                bucket.push(edge);
                *in_degree.entry(conn.to_node.as_str()).or_insert(0) += 1;
            }
        }
        // Suppress dead-code lint for the in-degree map: kept for future
        // schedulers / debug invariants (e.g. cycle detection).
        let _ = in_degree;

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

        // P2 resume-from-step: hydrate outputs from a previous run's step
        // records. Any node that was `Finished` is treated as already-done,
        // its outputs reused, and its executor is NOT invoked again — so
        // side-effecting nodes won't re-fire on a resume.
        //
        // R3 + R4 audit-fixes applied here:
        //   * The trigger node IS hydrated (R4 revision): with
        //     `trigger_input` required to be empty on resume (rejected at
        //     the top of this function), the trigger's only correct
        //     source-of-truth is its prior persisted output. Skipping it
        //     would re-execute the trigger with empty input and leave
        //     downstream-hydrated lineage inconsistent.
        //   * `HydrateResult::Corrupt` aborts the resume with `Internal`
        //     rather than silently re-executing the side-effecting node.
        //   * Hydrated items have their `ItemSource` re-stamped so a
        //     tampered `output_json` row cannot forge lineage.
        if let Some(source) = resume_from {
            for node in &wf.nodes {
                let expected_kind = node_kind_wire_name(node.kind);
                match source.hydrate(&run_id, &node.id, expected_kind).await {
                    HydrateResult::Missing => {}
                    HydrateResult::Found(items) => {
                        // Re-stamp lineage to (this node, index) so a
                        // tampered output_json cannot inject a forged
                        // `source` field downstream.
                        let stamped: Vec<Item> = items
                            .into_iter()
                            .enumerate()
                            .map(|(idx, mut it)| {
                                it.source = ItemSource::Produced {
                                    node_id: node.id.clone(),
                                    item_index: idx,
                                };
                                it
                            })
                            .collect();
                        let n = stamped.len();
                        outputs.insert(node.id.clone(), stamped);
                        done.insert(node.id.as_str());
                        remaining = remaining.saturating_sub(1);
                        log.record(StepEvent {
                            run_id: run_id.clone(),
                            node_id: node.id.clone(),
                            kind: StepKind::Finished,
                            latency_ms: 0,
                            input_items: 0,
                            output_items: n,
                            external_calls: 0,
                            tokens: 0,
                            error: Some("resumed from prior run".to_string()),
                        });
                    }
                    HydrateResult::Corrupt(msg) => {
                        return Err(EngineError::Internal(format!(
                            "resume aborted: prior step record for node '{}' is \
                             corrupt — refusing to silently re-execute a \
                             side-effecting node ({msg})",
                            node.id
                        )));
                    }
                }
            }
        }

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

            // Build each runnable node's input by gathering incoming items in
            // a deterministic order, then wrap its future in a semaphore acquire
            // so at most `DEFAULT_MAX_CONCURRENCY` (or the per-engine override)
            // nodes execute simultaneously. The semaphore is shared across the
            // whole run — bounding fan-out within AND across waves prevents a
            // workflow with thousands of parallel branches from exhausting the
            // tokio runtime or the OS connection pool.
            //
            // Audit-2: nodes that have incoming connections AND collected zero
            // items (e.g. the unselected arm of a Branch/Switch) are
            // SHORT-CIRCUITED: their executor is NOT invoked, they produce no
            // output, and downstream nodes see empty input. This both avoids
            // the spurious side effect AND records the skip as a Finished
            // step with output_items=0 for observability.
            let mut futures_vec = Vec::with_capacity(runnable.len());
            let mut skipped: Vec<&a2w_ir::Node> = Vec::new();
            for node in &runnable {
                let edges = incoming
                    .get(node.id.as_str())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let input =
                    self.gather_input(node, trigger_id, &trigger_items, edges, &outputs);
                if input.is_empty() && !edges.is_empty() {
                    // Has upstream but received nothing — skip without firing.
                    log.record(StepEvent {
                        run_id: run_id.clone(),
                        node_id: node.id.clone(),
                        kind: StepKind::Finished,
                        latency_ms: 0,
                        input_items: 0,
                        output_items: 0,
                        external_calls: 0,
                        tokens: 0,
                        error: Some("skipped: no input from port-routing upstream".to_string()),
                    });
                    skipped.push(node);
                    continue;
                }
                let sem = Arc::clone(&self.semaphore);
                let run_id_ref = run_id.as_str();
                let wf_id_ref = wf.id.as_str();
                let fut = async move {
                    let _permit = sem.acquire().await;
                    self.run_one_node(node, input, run_id_ref, Some(wf_id_ref), mode, log)
                        .await
                };
                futures_vec.push(fut);
            }

            // Commit the skipped batch first (no futures to drive).
            for node in &skipped {
                outputs.insert(node.id.clone(), Vec::new());
                done.insert(node.id.as_str());
                remaining = remaining.saturating_sub(1);
            }

            // Drive the whole wave concurrently (semaphore back-pressures).
            let executed: Vec<&a2w_ir::Node> = runnable
                .iter()
                .filter(|n| !skipped.iter().any(|s| s.id == n.id))
                .copied()
                .collect();
            let results = futures::future::join_all(futures_vec).await;

            // Commit results. Honor Stop policy by aborting the whole run.
            for (node, outcome) in executed.iter().zip(results) {
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
                remaining = remaining.saturating_sub(1);
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
                // Port-routed fan-in: an edge `(from_node, from_port) -> to`
                // gathers only the items the producer emitted on
                // `from_port`. Default executors emit every item on port 0,
                // so this collapses to "everything" for the common case;
                // Branch/Switch/Loop use other ports to direct individual
                // items to specific downstream paths.
                for item in items {
                    if item.output_port == edge.from_port {
                        gathered.push(item.clone());
                    }
                }
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
        workflow_id: Option<&str>,
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
            sub_workflows: self.sub_workflows.clone(),
            sub_workflow_depth: self.initial_sub_workflow_depth,
            workflow_id: workflow_id.map(str::to_string),
            approvals: self.approvals.clone(),
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
        let result = self
            .execute_with_retry(executor.as_ref(), &ctx, input, mode, node.retry.as_ref(), run_id, &node.id, input_count, log)
            .await;
        let latency_ms = elapsed_ms(start);

        match result {
            Ok(items) => {
                // Re-stamp lineage: the engine owns the `source` field.
                // Whatever a node set there is overwritten with this node's
                // identity + index. The executor-supplied `output_port` IS
                // preserved — routing is the executor's prerogative.
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

    /// Drive a single node with optional retry. Each retry attempt records a
    /// `Started` event with the attempt index encoded into `external_calls` so
    /// observability can see the retry behaviour without a schema change. The
    /// final outcome is returned to the caller, which then emits the
    /// authoritative `Finished` / `Failed` event for the whole step.
    ///
    /// In `DryRun` mode retries are **not applied** (mocking is deterministic
    /// and the retry semantics are about real network/IO transients).
    #[allow(clippy::too_many_arguments)]
    async fn execute_with_retry(
        &self,
        executor: &dyn NodeExecutor,
        ctx: &NodeContext,
        input: Vec<crate::item::Item>,
        mode: ExecutionMode,
        retry: Option<&RetryPolicy>,
        run_id: &str,
        node_id: &str,
        input_count: usize,
        log: &dyn EventLog,
    ) -> Result<Vec<crate::item::Item>, crate::node::NodeError> {
        // DryRun: skip retry entirely.
        if mode == ExecutionMode::DryRun {
            return executor.dry_run(ctx, input).await;
        }

        // No retry policy or 0/1 attempt configured: one shot.
        let max_attempts = match retry {
            Some(p) if p.max_attempts > 1 => p.max_attempts,
            _ => return executor.execute(ctx, input).await,
        };
        let backoff = Duration::from_millis(retry.map(|p| p.backoff_ms).unwrap_or(0));

        let mut last_err: Option<crate::node::NodeError> = None;
        for attempt in 1..=max_attempts {
            // Per-attempt input must be re-cloneable since each attempt is a
            // fresh call to the executor. We accept the clone cost — retries
            // are rare and items are JSON.
            let try_input = input.clone();
            match executor.execute(ctx, try_input).await {
                Ok(out) => return Ok(out),
                Err(err) => {
                    // Record a Failed-attempt event so a retried run is visible
                    // in the event stream even when the final outcome succeeds.
                    log.record(StepEvent {
                        run_id: run_id.to_string(),
                        node_id: node_id.to_string(),
                        kind: StepKind::Failed,
                        latency_ms: 0,
                        input_items: input_count,
                        output_items: 0,
                        external_calls: attempt,
                        tokens: 0,
                        error: Some(format!("attempt {attempt}/{max_attempts}: {err}")),
                    });
                    last_err = Some(err);
                    if attempt < max_attempts && !backoff.is_zero() {
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            crate::node::NodeError::Runtime("retry exhausted with no error captured".into())
        }))
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

/// Stable snake_case wire name for a [`NodeKind`]. Used by the engine to
/// pass an `expected_kind` to [`ResumeSource::hydrate`] for the R5
/// node_kind-drift check.
fn node_kind_wire_name(k: NodeKind) -> &'static str {
    match k {
        NodeKind::WebhookTrigger => "webhook_trigger",
        NodeKind::ScheduleTrigger => "schedule_trigger",
        NodeKind::HttpRequest => "http_request",
        NodeKind::McpToolCall => "mcp_tool_call",
        NodeKind::Transform => "transform",
        NodeKind::Branch => "branch",
        NodeKind::Switch => "switch",
        NodeKind::Loop => "loop",
        NodeKind::Merge => "merge",
        NodeKind::Wait => "wait",
        NodeKind::SubWorkflow => "sub_workflow",
        NodeKind::LlmCall => "llm_call",
        NodeKind::CodeStep => "code_step",
        NodeKind::Approval => "approval",
    }
}

/// Outcome of a hydrate probe — distinguishes "no prior Finished row" from
/// "row present but the persisted JSON cannot be deserialized" (R3 audit-
/// fix: the latter case used to be silently treated as a re-execute, which
/// caused side-effecting nodes to fire twice on a corrupted DB row).
#[derive(Debug)]
pub enum HydrateResult {
    /// No prior Finished row for this node — schedule it normally.
    Missing,
    /// A prior Finished row was found and deserialized successfully.
    Found(Vec<crate::item::Item>),
    /// A row was found but its serialized payload could not be parsed (DB
    /// corruption, schema drift). The engine MUST abort the resume rather
    /// than silently re-execute the side-effecting node.
    Corrupt(String),
}

/// A source the engine can ask "do you have a prior `Finished` output for this
/// (run_id, node_id) that I can reuse?". Implemented by `a2w_store::Store` so
/// the server can resume a crashed run without re-firing side effects.
///
/// R5 audit-fix: `expected_kind` is the wire name of the node's `kind` in
/// the CURRENT workflow IR. Implementations compare it to the persisted
/// step's `node_kind` and return [`HydrateResult::Corrupt`] on mismatch —
/// defeats the "edit a node's kind, keep its id, reuse its outputs" attack.
#[async_trait::async_trait]
pub trait ResumeSource: Send + Sync {
    /// Probe for the previously-persisted output `Vec<Item>` for
    /// `(run_id, node_id)`. See [`HydrateResult`] for the cases.
    async fn hydrate(
        &self,
        run_id: &str,
        node_id: &str,
        expected_kind: &str,
    ) -> HydrateResult;
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use crate::event::MemoryEventLog;
    use crate::item::Item;
    use crate::node::{NodeContext, NodeError, NodeExecutor};
    use a2w_ir::{Connection, Node, NodeKind, RetryPolicy, Workflow, SCHEMA_VERSION};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// An executor that fails the first `fail_count` calls, then succeeds.
    struct FlakyExecutor {
        fail_count: u32,
        attempts: AtomicU32,
    }

    impl FlakyExecutor {
        fn new(fail_count: u32) -> Self {
            Self {
                fail_count,
                attempts: AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl NodeExecutor for FlakyExecutor {
        fn has_side_effects(&self) -> bool {
            true
        }
        async fn execute(&self, _ctx: &NodeContext, _input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
            let n = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= self.fail_count {
                Err(NodeError::Runtime(format!("flaky attempt {n}")))
            } else {
                Ok(vec![Item::root(serde_json::json!({ "attempt": n }))])
            }
        }
    }

    fn flaky_wf() -> Workflow {
        Workflow {
            schema_version: SCHEMA_VERSION,
            id: "wf_flaky".into(),
            name: "Flaky".into(),
            nodes: vec![
                Node::new("trigger", NodeKind::WebhookTrigger),
                Node {
                    id: "flaky".into(),
                    kind: NodeKind::HttpRequest, // co-opted: we'll register our own executor
                    params: serde_json::json!({}),
                    retry: Some(RetryPolicy {
                        max_attempts: 4,
                        backoff_ms: 1,
                    }),
                    on_error: None,
                },
            ],
            connections: vec![Connection::new("trigger", 0, "flaky")],
        }
    }

    #[tokio::test]
    async fn retry_succeeds_within_max_attempts() {
        let flaky = Arc::new(FlakyExecutor::new(2));
        let registry = NodeRegistry::new()
            .with(NodeKind::WebhookTrigger, Arc::new(super::__tests::PassThrough))
            .with(NodeKind::HttpRequest, flaky.clone());
        let engine = Engine::new(registry);
        let log = MemoryEventLog::new();
        let result = engine
            .run(
                &flaky_wf(),
                vec![serde_json::json!({})],
                ExecutionMode::Run,
                &log,
            )
            .await
            .expect("retry should bring this run home");
        assert_eq!(result.status, RunStatus::Completed);
        let out = &result.node_outputs["flaky"];
        assert_eq!(out.len(), 1);
        // 3rd attempt is the first success.
        assert_eq!(out[0].json["attempt"], serde_json::json!(3));
    }

    #[tokio::test]
    async fn retry_exhausted_is_node_failure() {
        // Permanently failing executor with retry=2 → 2 attempts, then fail.
        let flaky = Arc::new(FlakyExecutor::new(99));
        let registry = NodeRegistry::new()
            .with(NodeKind::WebhookTrigger, Arc::new(super::__tests::PassThrough))
            .with(NodeKind::HttpRequest, flaky.clone());
        let engine = Engine::new(registry);
        let mut wf = flaky_wf();
        wf.nodes[1].retry = Some(RetryPolicy {
            max_attempts: 2,
            backoff_ms: 0,
        });
        let log = MemoryEventLog::new();
        let err = engine
            .run(&wf, vec![serde_json::json!({})], ExecutionMode::Run, &log)
            .await
            .expect_err("should fail after exhausting retries");
        assert!(matches!(err, EngineError::NodeFailed { .. }), "got {err:?}");
        // We expect the executor to have been called exactly 2 times.
        assert_eq!(flaky.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn semaphore_bounds_parallel_execution() {
        use a2w_ir::{Connection, Node, NodeKind, Workflow, SCHEMA_VERSION};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        // Executor that records the running count when it enters and verifies
        // it never exceeds the engine's configured cap.
        struct CountingExecutor {
            running: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl NodeExecutor for CountingExecutor {
            fn has_side_effects(&self) -> bool {
                false
            }
            async fn execute(
                &self,
                _ctx: &NodeContext,
                _input: Vec<Item>,
            ) -> Result<Vec<Item>, NodeError> {
                let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                self.running.fetch_sub(1, Ordering::SeqCst);
                Ok(vec![])
            }
        }

        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let exec: Arc<dyn NodeExecutor> = Arc::new(CountingExecutor {
            running: Arc::clone(&running),
            peak: Arc::clone(&peak),
        });
        let registry = NodeRegistry::new()
            .with(NodeKind::WebhookTrigger, Arc::new(super::__tests::PassThrough))
            // Co-opt Transform for the counter; it's a pure kind.
            .with(NodeKind::Transform, Arc::clone(&exec));
        let engine = Engine::new(registry).with_max_concurrency(2);

        // 8 sibling Transform nodes — all runnable in one wave.
        let mut nodes = vec![Node::new("trigger", NodeKind::WebhookTrigger)];
        let mut conns = Vec::new();
        for i in 0..8 {
            let id = format!("t{i}");
            nodes.push(Node::new(&id, NodeKind::Transform));
            conns.push(Connection::new("trigger", 0, id));
        }
        let wf = Workflow {
            schema_version: SCHEMA_VERSION,
            id: "wf_concurrent".into(),
            name: "Concurrent siblings".into(),
            nodes,
            connections: conns,
        };
        let log = crate::event::MemoryEventLog::new();
        engine
            .run(&wf, vec![serde_json::json!({})], ExecutionMode::Run, &log)
            .await
            .expect("ok");
        let observed_peak = peak.load(Ordering::SeqCst);
        assert!(
            observed_peak <= 2,
            "engine peak concurrency must be <= cap (2); observed {observed_peak}"
        );
        assert!(observed_peak >= 1, "must have executed at least once");
    }

    #[tokio::test]
    async fn retry_not_applied_in_dry_run() {
        let flaky = Arc::new(FlakyExecutor::new(99));
        let registry = NodeRegistry::new()
            .with(NodeKind::WebhookTrigger, Arc::new(super::__tests::PassThrough))
            .with(NodeKind::HttpRequest, flaky.clone());
        let engine = Engine::new(registry);
        let log = MemoryEventLog::new();
        let _ = engine
            .run(
                &flaky_wf(),
                vec![serde_json::json!({})],
                ExecutionMode::DryRun,
                &log,
            )
            .await
            .expect("dry_run uses the default mock for side-effecting nodes");
        // dry_run goes through `dry_run` on the executor (the default impl
        // returns a mock without calling `execute`), so attempts should be 0.
        assert_eq!(
            flaky.attempts.load(Ordering::SeqCst),
            0,
            "dry_run must not invoke execute"
        );
    }
}

#[cfg(test)]
pub(crate) mod __tests {
    //! Tiny helpers shared by the engine's in-tree tests.
    use crate::item::Item;
    use crate::node::{NodeContext, NodeError, NodeExecutor};
    use async_trait::async_trait;

    /// Executor that passes its input through unchanged.
    pub struct PassThrough;

    #[async_trait]
    impl NodeExecutor for PassThrough {
        fn has_side_effects(&self) -> bool {
            false
        }
        async fn execute(&self, _ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
            Ok(input)
        }
    }
}

/// Mint a dependency-light run id from the workflow id plus a process-global
/// monotonic counter, plus a 64-bit hash of (PID, nanos-since-epoch, counter)
/// that scrambles to avoid cross-process collision. No uuid crate required —
/// we use the std `RandomState` hasher seed to get a process-unique salt.
///
/// R3 audit-fix: previously just `<wf>_<atomic>` which collided trivially
/// across replicas and let `save_run`'s ON CONFLICT silently overwrite peer
/// runs.
fn mint_run_id(wf_id: &str) -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;
    use std::time::{SystemTime, UNIX_EPOCH};

    let n = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let salt = RandomState::new().hash_one((pid, nanos, n, wf_id));
    format!("run_{wf_id}_{nanos:x}_{n}_{salt:x}")
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
