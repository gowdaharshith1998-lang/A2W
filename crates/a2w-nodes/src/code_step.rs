//! [`NodeKind::CodeStep`](a2w_ir::NodeKind::CodeStep) executor: run untrusted
//! user code as a sandboxed WebAssembly module via [extism](https://extism.org).
//!
//! Side-effecting. Params:
//! ```json
//! { "wasm": { "base64": "<wasm bytes, base64>" } | { "path": "<file.wasm>" },
//!   "function"?: "run",
//!   "config"?: { "k": "v", ... } }
//! ```
//!
//! For each input item the executor serialises the item's `json` to bytes, calls
//! the named export (default `"run"`) inside the sandbox, and parses the output
//! bytes back as JSON (falling back to `{ "output": "<utf8>" }` when the module
//! returns non-JSON text). One output item is emitted per input item.
//!
//! The actual WASM host is hidden behind the [`WasmRunner`] trait (mirroring the
//! [`McpInvoker`](crate::McpInvoker) pattern) so the node is fully testable
//! without compiling/running any WASM: tests inject a deterministic mock. The
//! default node uses [`ExtismRunner`], a thin wrapper over the extism 1.30 host
//! SDK (which bundles a wasmtime runtime).
//!
//! ## Sandbox
//! [`ExtismRunner`] builds a **locked-down** `Manifest`: no `allowed_hosts`
//! (the module cannot make HTTP requests — `disallow_all_hosts` makes this
//! explicit), no `allowed_paths` (no filesystem access), a bounded memory limit,
//! and a wall-clock `timeout`. Running arbitrary code is a side effect, so
//! `has_side_effects()` is `true` and a dry run mocks the node without loading
//! or executing any WASM.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use serde::Deserialize;
use thiserror::Error;

use a2w_engine::{Item, NodeContext, NodeError, NodeExecutor};

/// Default exported function name when `function` is omitted.
const DEFAULT_FUNCTION: &str = "run";

/// Wall-clock execution budget handed to the sandbox per call. Untrusted code
/// that spins forever is interrupted at this bound rather than wedging the host.
const EXEC_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum linear-memory pages the sandboxed module may grow to (64 KiB each).
/// 256 pages = 16 MiB, generous for data-shaping code while still bounded.
const MAX_MEMORY_PAGES: u32 = 256;

// ---------------------------------------------------------------------------
// Error.
// ---------------------------------------------------------------------------

/// Errors a [`WasmRunner`] (or the param parsing around it) can surface.
#[derive(Debug, Error)]
pub enum CodeError {
    /// The `wasm` param was missing, malformed, or its bytes could not be loaded
    /// (bad base64, unreadable file, etc.).
    #[error("bad code_step params: {0}")]
    BadParams(String),
    /// The WASM module failed to compile / instantiate in the sandbox.
    #[error("wasm load error: {0}")]
    Load(String),
    /// The exported function call itself failed (trap, timeout, missing export,
    /// non-zero exit).
    #[error("wasm call error: {0}")]
    Call(String),
}

/// Default cap on the per-item input payload handed to a WASM module.
const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;

/// Read `A2W_CODE_MAX_INPUT_BYTES` once and cache it; default 1 MiB.
fn max_input_bytes() -> usize {
    use std::sync::OnceLock;
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("A2W_CODE_MAX_INPUT_BYTES")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_MAX_INPUT_BYTES)
    })
}

// ---------------------------------------------------------------------------
// Wasm source.
// ---------------------------------------------------------------------------

/// Where the WASM module bytes come from, parsed from the node's `wasm` param.
///
/// Exactly one of `base64` (inline bytes) or `path` (a file on disk) must be
/// given. `path` is intended for trusted/operator-supplied modules; `base64`
/// lets a workflow carry the module inline.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum WasmSource {
    /// Inline module bytes, base64-encoded (standard alphabet, padded).
    Base64(String),
    /// A path to a `.wasm` file on disk.
    Path(PathBuf),
}

impl WasmSource {
    /// Parse the `wasm` param value into a [`WasmSource`].
    fn from_params(wasm: &serde_json::Value) -> Result<Self, CodeError> {
        if wasm.is_null() {
            return Err(CodeError::BadParams(
                "code_step requires a `wasm` source: { \"base64\": \"...\" } or { \"path\": \"...\" }"
                    .into(),
            ));
        }
        serde_json::from_value(wasm.clone()).map_err(|e| {
            CodeError::BadParams(format!(
                "`wasm` is not a valid source ({e}); expected exactly one of \
                 {{ \"base64\": \"<wasm bytes b64>\" }} or {{ \"path\": \"<file.wasm>\" }}"
            ))
        })
    }

    /// Resolve this source to the raw WASM bytes.
    ///
    /// Audit-fix: `WasmSource::Path` is now confined to a single directory
    /// rooted at `A2W_CODE_WASM_DIR` (canonicalized). Without that env var set
    /// the path source is rejected outright — protects against a workflow that
    /// names `/etc/passwd` or `/proc/self/environ`.
    fn load(&self) -> Result<Vec<u8>, CodeError> {
        match self {
            WasmSource::Base64(b64) => base64::engine::general_purpose::STANDARD
                .decode(b64.as_bytes())
                .map_err(|e| {
                    CodeError::BadParams(format!("`wasm.base64` is not valid base64: {e}"))
                }),
            WasmSource::Path(path) => {
                let root = std::env::var("A2W_CODE_WASM_DIR").map_err(|_| {
                    CodeError::BadParams(
                        "`wasm.path` is disabled: set A2W_CODE_WASM_DIR to a directory \
                         containing the trusted .wasm modules"
                            .to_string(),
                    )
                })?;
                let root = std::fs::canonicalize(&root).map_err(|e| {
                    CodeError::BadParams(format!(
                        "A2W_CODE_WASM_DIR ('{root}') is not a usable directory: {e}"
                    ))
                })?;
                let abs = std::fs::canonicalize(path).map_err(|e| {
                    CodeError::BadParams(format!(
                        "`wasm.path` could not be resolved ('{}'): {e}",
                        path.display()
                    ))
                })?;
                if !abs.starts_with(&root) {
                    return Err(CodeError::BadParams(format!(
                        "`wasm.path` ('{}') is outside the permitted A2W_CODE_WASM_DIR ('{}')",
                        abs.display(),
                        root.display()
                    )));
                }
                std::fs::read(&abs).map_err(|e| {
                    CodeError::BadParams(format!(
                        "`wasm.path` could not be read ('{}'): {e}",
                        abs.display()
                    ))
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Runner trait.
// ---------------------------------------------------------------------------

/// A pluggable WASM host: loads `wasm`, invokes the export named `function` with
/// `input` bytes, and returns the raw output bytes.
///
/// Kept **synchronous** because the extism `Plugin::call` is blocking; the
/// executor wraps real runs in [`tokio::task::spawn_blocking`] so the async
/// runtime is never stalled. Implementations must be `Send + Sync` so the
/// executor can hold one behind a shared reference. The default node uses
/// [`ExtismRunner`]; tests inject a deterministic mock.
pub trait WasmRunner: Send + Sync {
    /// Run `function` from the `wasm` module against `input`, returning the
    /// module's raw output bytes.
    ///
    /// # Errors
    /// Returns [`CodeError`] on a module that fails to load ([`CodeError::Load`])
    /// or a call that fails / traps / times out ([`CodeError::Call`]).
    fn run(&self, wasm: &[u8], function: &str, input: &[u8]) -> Result<Vec<u8>, CodeError>;
}

// ---------------------------------------------------------------------------
// Real implementation over the extism host SDK.
// ---------------------------------------------------------------------------

/// A [`WasmRunner`] backed by the extism 1.30 host SDK (bundled wasmtime).
///
/// Each call builds a fresh, **locked-down** `Manifest` from the module bytes,
/// instantiates a `Plugin`, and invokes the export. The sandbox grants no
/// network ([`Manifest::disallow_all_hosts`]) and no filesystem (no
/// `allowed_paths`), caps linear memory, and enforces a wall-clock
/// [`EXEC_TIMEOUT`]. WASI is enabled (`with_wasi: true`) because the canonical
/// PDK-built plugins import the WASI ABI for memory setup, but with no preopened
/// dirs or hosts the module still cannot touch the host environment.
///
/// Per-call (re)instantiation keeps the runner stateless and `Send + Sync`;
/// config injected via the manifest (see [`CodeStep`] `config` param) is plumbed
/// through to the module's `extism_config_get`.
#[derive(Debug, Default, Clone)]
pub struct ExtismRunner {
    /// Optional `config` key/value pairs exposed to the module via extism config.
    config: std::collections::BTreeMap<String, String>,
}

impl ExtismRunner {
    /// Construct a runner with no extra config.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a runner that exposes `config` to the module via extism's
    /// `extism_config_get`.
    #[must_use]
    pub fn with_config(config: std::collections::BTreeMap<String, String>) -> Self {
        Self { config }
    }
}

impl WasmRunner for ExtismRunner {
    fn run(&self, wasm: &[u8], function: &str, input: &[u8]) -> Result<Vec<u8>, CodeError> {
        use extism::{Manifest, Plugin, Wasm};

        // Locked-down manifest: inline bytes only, no network, no filesystem,
        // bounded memory, and a hard wall-clock timeout. `disallow_all_hosts`
        // makes the "no HTTP" stance explicit (the default `None` would also
        // deny, but being explicit documents intent and is robust to defaults
        // changing).
        let mut manifest = Manifest::new([Wasm::data(wasm.to_vec())])
            .disallow_all_hosts()
            .with_memory_max(MAX_MEMORY_PAGES)
            .with_timeout(EXEC_TIMEOUT);
        if !self.config.is_empty() {
            manifest = manifest.with_config(self.config.iter());
        }

        // No host functions are exported to the module ([]). WASI is enabled
        // for ABI/memory setup only; with no allowed hosts/paths it grants no
        // real capabilities.
        let mut plugin = Plugin::new(&manifest, [], true)
            .map_err(|e| CodeError::Load(format!("failed to instantiate wasm module: {e}")))?;

        let out: &[u8] = plugin
            .call(function, input)
            .map_err(|e| CodeError::Call(format!("call to '{function}' failed: {e}")))?;
        Ok(out.to_vec())
    }
}

// ---------------------------------------------------------------------------
// Node executor.
// ---------------------------------------------------------------------------

/// Executor for [`a2w_ir::NodeKind::CodeStep`].
///
/// Holds a [`WasmRunner`] behind an [`Arc`] so the host is injectable: the
/// default uses [`ExtismRunner`] (a real extism/wasmtime sandbox), while tests
/// inject a deterministic mock.
#[derive(Clone)]
pub struct CodeStep {
    runner: Arc<dyn WasmRunner>,
    /// `true` when `runner` is the default [`ExtismRunner`] (built by
    /// [`CodeStep::default`]). A `config` param is plumbed into the sandbox only
    /// for the default runner: a caller-injected runner (test mock or a
    /// pre-configured `ExtismRunner`) is honoured exactly as given.
    default_extism: bool,
}

impl std::fmt::Debug for CodeStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeStep").finish_non_exhaustive()
    }
}

impl Default for CodeStep {
    fn default() -> Self {
        Self {
            runner: Arc::new(ExtismRunner::new()),
            default_extism: true,
        }
    }
}

impl CodeStep {
    /// Construct with an explicit [`WasmRunner`] (e.g. a mock in tests).
    ///
    /// The injected runner is used exactly as given; the `config` param is not
    /// re-plumbed for a custom runner (only the default [`ExtismRunner`] picks it
    /// up).
    #[must_use]
    pub fn new(runner: Arc<dyn WasmRunner>) -> Self {
        Self {
            runner,
            default_extism: false,
        }
    }

    /// Read `function` from params (defaults to `"run"`; must be a non-empty
    /// string when present).
    fn function(params: &serde_json::Value) -> Result<String, NodeError> {
        match params.get("function") {
            None | Some(serde_json::Value::Null) => Ok(DEFAULT_FUNCTION.to_string()),
            Some(serde_json::Value::String(s)) if !s.is_empty() => Ok(s.clone()),
            Some(_) => Err(NodeError::BadParams(
                "code_step `function` must be a non-empty string".into(),
            )),
        }
    }

    /// Parse the `wasm` source from params, mapping a bad source to
    /// [`NodeError::BadParams`].
    fn wasm_source(params: &serde_json::Value) -> Result<WasmSource, NodeError> {
        let raw = params
            .get("wasm")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        WasmSource::from_params(&raw).map_err(|e| NodeError::BadParams(e.to_string()))
    }

    /// Parse the optional `config` map (string -> string) from params.
    fn config(
        params: &serde_json::Value,
    ) -> Result<std::collections::BTreeMap<String, String>, NodeError> {
        match params.get("config") {
            None | Some(serde_json::Value::Null) => Ok(Default::default()),
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                NodeError::BadParams(format!("code_step `config` must be a string map: {e}"))
            }),
        }
    }

    /// Interpret raw module output bytes as JSON, falling back to a UTF-8 string
    /// wrapper when the bytes are not valid JSON.
    fn parse_output(bytes: Vec<u8>) -> serde_json::Value {
        match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(value) => value,
            Err(_) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                serde_json::json!({ "output": text })
            }
        }
    }
}

#[async_trait]
impl NodeExecutor for CodeStep {
    fn has_side_effects(&self) -> bool {
        // Running arbitrary (untrusted) WASM is treated as a side effect: a dry
        // run must NOT load or execute the module, so it is mocked instead.
        true
    }

    async fn execute(&self, ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        // Validate + load params once, up front, before any execution.
        let source = Self::wasm_source(&ctx.params)?;
        let function = Self::function(&ctx.params)?;
        let config = Self::config(&ctx.params)?;
        let wasm_bytes = source
            .load()
            .map_err(|e| NodeError::BadParams(e.to_string()))?;

        // The default ExtismRunner carries no config; when `config` is present
        // build a config-aware runner for this execution. A non-default
        // (injected) runner is used as-is.
        let runner: Arc<dyn WasmRunner> = if !config.is_empty() && self.default_extism {
            Arc::new(ExtismRunner::with_config(config))
        } else {
            Arc::clone(&self.runner)
        };

        // One output item per input item. Audit-2 fix (CRITICAL —
        // unselected-branch side effect): no input → no work, mirroring
        // mcp_tool_call. WASM execution is expensive; running it on an empty
        // unselected branch arm is both incorrect and a DoS vector.
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let count = input.len();
        let mut out = Vec::with_capacity(count);
        for item in input.iter().map(Some) {
            // Serialise the item's json to bytes (empty object when there is no
            // input item).
            let payload = item.map_or_else(
                || serde_json::Value::Object(Default::default()),
                |it| it.json.clone(),
            );
            let input_bytes = serde_json::to_vec(&payload)
                .map_err(|e| NodeError::Runtime(format!("failed to serialise input item: {e}")))?;
            // Audit-fix: cap the per-item input payload so a malicious upstream
            // node can't OOM the WASM sandbox by handing it an enormous JSON
            // blob. Default 1 MiB; tunable via `A2W_CODE_MAX_INPUT_BYTES`.
            let max_input = max_input_bytes();
            if input_bytes.len() > max_input {
                return Err(NodeError::Runtime(format!(
                    "code_step input ({} bytes) exceeds the maximum allowed \
                     ({} bytes, set A2W_CODE_MAX_INPUT_BYTES to change)",
                    input_bytes.len(),
                    max_input
                )));
            }

            // extism's call is blocking: run it off the async runtime.
            let runner = Arc::clone(&runner);
            let function = function.clone();
            let wasm_bytes = wasm_bytes.clone();
            let output_bytes = tokio::task::spawn_blocking(move || {
                runner.run(&wasm_bytes, &function, &input_bytes)
            })
            .await
            .map_err(|e| {
                NodeError::Runtime(format!("code_step task panicked or was cancelled: {e}"))
            })?
            .map_err(|e| NodeError::Runtime(e.to_string()))?;

            let value = Self::parse_output(output_bytes);
            out.push(Item::produced(value, ctx.node_id.clone(), 0));
        }
        Ok(out)
    }

    async fn dry_run(&self, ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        // Validate the params shape (without loading/running any wasm), then mock
        // one item per input item.
        let _source = Self::wasm_source(&ctx.params)?;
        let function = Self::function(&ctx.params)?;
        let _config = Self::config(&ctx.params)?;

        if input.is_empty() {
            return Ok(Vec::new());
        }
        let count = input.len();
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(Item::produced(
                serde_json::json!({ "_mock": true, "code_step": true, "function": function }),
                ctx.node_id.clone(),
                0,
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use a2w_engine::ExecutionMode;
    use a2w_ir::NodeKind;

    /// A deterministic, WASM-free runner for tests: records the function name and
    /// input bytes it last saw and returns a canned byte output (or a forced
    /// error). `byte`-level recording proves the executor serialises the item
    /// json and routes it to the runner unchanged.
    #[derive(Debug, Default)]
    struct MockRunner {
        /// Bytes to return from `run` (e.g. a JSON document or raw text).
        canned: Vec<u8>,
        /// When true, `run` returns a [`CodeError::Call`].
        fail: bool,
        /// Records `(function, input_bytes)` of every call.
        seen: Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl MockRunner {
        fn returning(bytes: impl Into<Vec<u8>>) -> Self {
            Self {
                canned: bytes.into(),
                ..Default::default()
            }
        }
        fn failing() -> Self {
            Self {
                fail: true,
                ..Default::default()
            }
        }
    }

    impl WasmRunner for MockRunner {
        fn run(&self, _wasm: &[u8], function: &str, input: &[u8]) -> Result<Vec<u8>, CodeError> {
            self.seen
                .lock()
                .unwrap()
                .push((function.to_string(), input.to_vec()));
            if self.fail {
                return Err(CodeError::Call("mock wasm failure".into()));
            }
            Ok(self.canned.clone())
        }
    }

    /// A tiny, definitely-not-a-wasm base64 blob; valid base64, so it loads as
    /// bytes fine, and the MockRunner never actually parses it as wasm.
    fn dummy_wasm_b64() -> String {
        base64::engine::general_purpose::STANDARD.encode(b"\0asm-not-real")
    }

    fn ctx(params: serde_json::Value, mode: ExecutionMode) -> NodeContext {
        NodeContext {
            run_id: "run".into(),
            node_id: "code".into(),
            kind: NodeKind::CodeStep,
            params,
            mode,
            credentials: None,
            sub_workflows: None,
            sub_workflow_depth: 0,
            workflow_id: None,
            approvals: None,
        }
    }

    // --- WasmSource parsing ----------------------------------------------------

    #[test]
    fn wasm_source_parses_base64_and_path() {
        let b64 = WasmSource::from_params(&serde_json::json!({ "base64": "AAA=" }))
            .expect("valid base64 source");
        assert_eq!(b64, WasmSource::Base64("AAA=".into()));

        let path = WasmSource::from_params(&serde_json::json!({ "path": "x.wasm" }))
            .expect("valid path source");
        assert_eq!(path, WasmSource::Path(PathBuf::from("x.wasm")));
    }

    #[test]
    fn wasm_source_rejects_null_and_unknown_shape() {
        assert!(matches!(
            WasmSource::from_params(&serde_json::Value::Null),
            Err(CodeError::BadParams(_))
        ));
        assert!(matches!(
            WasmSource::from_params(&serde_json::json!({ "url": "http://x" })),
            Err(CodeError::BadParams(_))
        ));
        // Both keys at once is rejected (untagged enum + deny_unknown_fields).
        assert!(matches!(
            WasmSource::from_params(&serde_json::json!({ "base64": "AAA=", "path": "x" })),
            Err(CodeError::BadParams(_))
        ));
    }

    #[test]
    fn wasm_source_load_decodes_base64_and_rejects_bad_base64() {
        let src = WasmSource::Base64(base64::engine::general_purpose::STANDARD.encode(b"hello"));
        assert_eq!(src.load().expect("decodes"), b"hello");

        let bad = WasmSource::Base64("not!!base64".into());
        assert!(matches!(bad.load(), Err(CodeError::BadParams(_))));
    }

    // --- parse_output ----------------------------------------------------------

    #[test]
    fn parse_output_json_passthrough_and_text_fallback() {
        let json = CodeStep::parse_output(br#"{"count":3}"#.to_vec());
        assert_eq!(json, serde_json::json!({ "count": 3 }));

        let text = CodeStep::parse_output(b"plain text".to_vec());
        assert_eq!(text, serde_json::json!({ "output": "plain text" }));
    }

    // --- execute (Run) ---------------------------------------------------------

    #[tokio::test]
    async fn execute_runs_function_and_parses_json_output() {
        let runner = Arc::new(MockRunner::returning(br#"{"ok":true,"n":7}"#.to_vec()));
        let node = CodeStep::new(runner.clone());
        let ctx = ctx(
            serde_json::json!({ "wasm": { "base64": dummy_wasm_b64() }, "function": "go" }),
            ExecutionMode::Run,
        );
        let input = vec![
            Item::root(serde_json::json!({ "a": 1 })),
            Item::root(serde_json::json!({ "b": 2 })),
        ];
        let out = node.execute(&ctx, input).await.expect("execute ok");
        assert_eq!(out.len(), 2, "one item per input item");
        for item in &out {
            assert_eq!(item.json, serde_json::json!({ "ok": true, "n": 7 }));
        }

        // The runner saw the right function and each item's serialised json.
        let seen = runner.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(|(f, _)| f == "go"));
        assert_eq!(
            seen[0].1,
            serde_json::to_vec(&serde_json::json!({ "a": 1 })).unwrap()
        );
        assert_eq!(
            seen[1].1,
            serde_json::to_vec(&serde_json::json!({ "b": 2 })).unwrap()
        );
    }

    #[tokio::test]
    async fn execute_defaults_function_to_run_and_wraps_non_json() {
        let runner = Arc::new(MockRunner::returning(b"hi there".to_vec()));
        let node = CodeStep::new(runner.clone());
        let ctx = ctx(
            serde_json::json!({ "wasm": { "base64": dummy_wasm_b64() } }),
            ExecutionMode::Run,
        );
        let out = node
            .execute(&ctx, vec![Item::root(serde_json::json!({}))])
            .await
            .expect("execute ok");
        assert_eq!(out.len(), 1);
        // Non-JSON output is wrapped.
        assert_eq!(out[0].json, serde_json::json!({ "output": "hi there" }));
        // `function` defaulted to "run".
        assert_eq!(runner.seen.lock().unwrap()[0].0, "run");
    }

    #[tokio::test]
    async fn execute_with_no_input_emits_zero_items_audit2() {
        // Audit-2: empty input → no WASM invocation. The runner must never
        // have been called.
        let runner = Arc::new(MockRunner::returning(b"{}".to_vec()));
        let node = CodeStep::new(runner.clone());
        let ctx = ctx(
            serde_json::json!({ "wasm": { "base64": dummy_wasm_b64() } }),
            ExecutionMode::Run,
        );
        let out = node.execute(&ctx, vec![]).await.expect("execute ok");
        assert!(out.is_empty(), "no input → no side-effect, no items");
        assert!(
            runner.seen.lock().unwrap().is_empty(),
            "runner must not be invoked on empty input"
        );
    }

    #[tokio::test]
    async fn execute_runner_error_is_runtime() {
        let node = CodeStep::new(Arc::new(MockRunner::failing()));
        let ctx = ctx(
            serde_json::json!({ "wasm": { "base64": dummy_wasm_b64() } }),
            ExecutionMode::Run,
        );
        let err = node
            .execute(&ctx, vec![Item::root(serde_json::json!({}))])
            .await
            .expect_err("runner failed");
        assert!(matches!(err, NodeError::Runtime(_)), "got {err:?}");
    }

    // --- param validation ------------------------------------------------------

    #[tokio::test]
    async fn execute_missing_wasm_is_bad_params() {
        let node = CodeStep::new(Arc::new(MockRunner::default()));
        let ctx = ctx(serde_json::json!({ "function": "run" }), ExecutionMode::Run);
        let err = node.execute(&ctx, vec![]).await.expect_err("missing wasm");
        assert!(matches!(err, NodeError::BadParams(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn execute_bad_base64_is_bad_params() {
        let node = CodeStep::new(Arc::new(MockRunner::default()));
        let ctx = ctx(
            serde_json::json!({ "wasm": { "base64": "not!!base64" } }),
            ExecutionMode::Run,
        );
        let err = node.execute(&ctx, vec![]).await.expect_err("bad base64");
        assert!(matches!(err, NodeError::BadParams(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn execute_non_string_function_is_bad_params() {
        let node = CodeStep::new(Arc::new(MockRunner::default()));
        let ctx = ctx(
            serde_json::json!({ "wasm": { "base64": dummy_wasm_b64() }, "function": 42 }),
            ExecutionMode::Run,
        );
        let err = node.execute(&ctx, vec![]).await.expect_err("bad function");
        assert!(matches!(err, NodeError::BadParams(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn execute_missing_path_file_is_bad_params() {
        let node = CodeStep::new(Arc::new(MockRunner::default()));
        let ctx = ctx(
            serde_json::json!({ "wasm": { "path": "definitely/missing/file.wasm" } }),
            ExecutionMode::Run,
        );
        let err = node.execute(&ctx, vec![]).await.expect_err("missing file");
        assert!(matches!(err, NodeError::BadParams(_)), "got {err:?}");
    }

    // --- dry_run ---------------------------------------------------------------

    #[tokio::test]
    async fn dry_run_returns_mock_without_invoking_runner() {
        // Runner is set to fail; dry_run must NOT call it, so this succeeds.
        let node = CodeStep::new(Arc::new(MockRunner::failing()));
        let ctx = ctx(
            serde_json::json!({ "wasm": { "base64": dummy_wasm_b64() }, "function": "go" }),
            ExecutionMode::DryRun,
        );
        let out = node
            .dry_run(&ctx, vec![Item::root(serde_json::json!({}))])
            .await
            .expect("dry_run ok");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].json["_mock"], serde_json::json!(true));
        assert_eq!(out[0].json["code_step"], serde_json::json!(true));
        assert_eq!(out[0].json["function"], serde_json::json!("go"));
    }

    #[tokio::test]
    async fn dry_run_validates_params() {
        let node = CodeStep::default();
        let ctx = ctx(serde_json::json!({}), ExecutionMode::DryRun);
        let err = node.dry_run(&ctx, vec![]).await.expect_err("missing wasm");
        assert!(matches!(err, NodeError::BadParams(_)), "got {err:?}");
    }
}
