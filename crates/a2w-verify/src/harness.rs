//! The verification harness: a thin, deterministic, zero-token runner.
//!
//! Correctness testing leans on the engine's core guarantee — runs are
//! reproducible and never touch an LLM or (in [`ExecutionMode::DryRun`]) the
//! network — so a relation can be checked many times almost for free. The
//! harness defaults to `DryRun` for exactly this reason: pure data nodes
//! (Transform / Branch / Switch / Loop / Merge) execute identically in `Run`
//! and `DryRun`, while side-effecting nodes are mocked rather than dialed out.

use a2w_engine::{Engine, ExecutionMode, MemoryEventLog, RunResult};
use a2w_ir::Workflow;
use serde_json::Value;

use crate::VerifyError;

/// A reusable runner that executes workflows for verification.
pub struct VerificationHarness {
    engine: Engine,
    mode: ExecutionMode,
}

impl Default for VerificationHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl VerificationHarness {
    /// A harness over the default node registry in `DryRun` mode — zero-token
    /// and network-free, the right default for correctness verification.
    #[must_use]
    pub fn new() -> Self {
        Self {
            engine: Engine::new(a2w_nodes::default_registry()),
            mode: ExecutionMode::DryRun,
        }
    }

    /// Override the execution mode (e.g. `Run` for a fully-pure workflow whose
    /// real and dry outputs are identical).
    #[must_use]
    pub fn with_mode(mut self, mode: ExecutionMode) -> Self {
        self.mode = mode;
        self
    }

    /// Use a caller-provided engine (e.g. with injected deterministic mocks for
    /// side-effecting nodes) instead of the default registry.
    #[must_use]
    pub fn with_engine(mut self, engine: Engine, mode: ExecutionMode) -> Self {
        self.engine = engine;
        self.mode = mode;
        self
    }

    /// The execution mode this harness uses.
    #[must_use]
    pub fn mode(&self) -> ExecutionMode {
        self.mode
    }

    /// Run `wf` with `input` seeded into the trigger, returning the full run
    /// result.
    ///
    /// # Errors
    /// [`VerifyError::Engine`] if validation or execution fails.
    pub async fn run(&self, wf: &Workflow, input: Vec<Value>) -> Result<RunResult, VerifyError> {
        let log = MemoryEventLog::new();
        self.engine
            .run(wf, input, self.mode, &log)
            .await
            .map_err(|e| VerifyError::Engine(e.to_string()))
    }

    /// Run `wf` and return the `.json` payloads of `observe_node`'s output
    /// items, in order. A node that produced no output (or did not execute)
    /// yields an empty vec — that is a legitimate observation, not an error.
    ///
    /// # Errors
    /// - [`VerifyError::Engine`] if the run fails.
    /// - [`VerifyError::UnknownNode`] if `observe_node` is not a node in `wf`.
    pub async fn observe(
        &self,
        wf: &Workflow,
        observe_node: &str,
        input: Vec<Value>,
    ) -> Result<Vec<Value>, VerifyError> {
        if !wf.nodes.iter().any(|n| n.id == observe_node) {
            return Err(VerifyError::UnknownNode(observe_node.to_string()));
        }
        let result = self.run(wf, input).await?;
        Ok(result
            .node_outputs
            .get(observe_node)
            .map(|items| items.iter().map(|i| i.json.clone()).collect())
            .unwrap_or_default())
    }
}
