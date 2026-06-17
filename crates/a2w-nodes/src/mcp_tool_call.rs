//! [`NodeKind::McpToolCall`](a2w_ir::NodeKind::McpToolCall) executor.
//!
//! Side-effecting. Params:
//! ```json
//! { "server": { "transport": "stdio", "command": "a2w-mcp", "args": [..]?, "env": {..}? },
//!   "tool": "...", "arguments": {..}? }
//! ```
//!
//! In a real (`Run`) execution this spawns / connects to an external MCP server
//! and invokes a tool on it. The actual transport is hidden behind the
//! [`McpInvoker`] trait (mirroring the LLM-client pattern in `a2w-llm`) so the
//! node is fully testable without a live server: tests inject a mock invoker.
//! The default node uses [`RmcpInvoker`], a thin wrapper over the official
//! `rmcp` client that spawns a stdio child-process server.
//!
//! `dry_run` returns a mock (no server is contacted) so dry runs of
//! MCP-containing workflows still validate end-to-end.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;

use a2w_engine::{Item, NodeContext, NodeError, NodeExecutor};

// ---------------------------------------------------------------------------
// Server spec.
// ---------------------------------------------------------------------------

/// How to reach an MCP server, parsed from the node's `server` param.
///
/// Only the `stdio` child-process transport is wired for real; an `http`
/// variant is accepted by the parser but returns a clear "not yet supported"
/// error when invoked, so workflows that name it fail cleanly rather than
/// silently mis-behaving.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpServerSpec {
    /// A server started as a child process speaking MCP over stdio.
    Stdio {
        /// The executable to launch (looked up on `PATH`, or an absolute path).
        command: String,
        /// Arguments passed to the command.
        #[serde(default)]
        args: Vec<String>,
        /// Extra environment variables for the child process. Ordered (BTreeMap)
        /// so the spec has a deterministic representation.
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    /// An HTTP(S) MCP endpoint. Parsed but not yet supported at run time.
    Http {
        /// The base URL of the streamable-HTTP MCP endpoint.
        url: String,
    },
}

impl McpServerSpec {
    /// Parse a [`McpServerSpec`] from the node's `server` param value.
    ///
    /// # Errors
    /// Returns [`McpError::BadSpec`] when the value is missing or does not match
    /// a known transport shape.
    pub fn from_params(server: &serde_json::Value) -> Result<Self, McpError> {
        if server.is_null() {
            return Err(McpError::BadSpec(
                "McpToolCall requires an object `server` with a `transport` field".into(),
            ));
        }
        serde_json::from_value(server.clone()).map_err(|e| {
            McpError::BadSpec(format!(
                "`server` is not a valid MCP server spec ({e}); expected \
                 {{ \"transport\": \"stdio\", \"command\": \"...\", \"args\"?: [..], \"env\"?: {{..}} }} \
                 or {{ \"transport\": \"http\", \"url\": \"...\" }}"
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// Error.
// ---------------------------------------------------------------------------

/// Errors an [`McpInvoker`] can surface.
#[derive(Debug, Error)]
pub enum McpError {
    /// The `server` param was missing or malformed.
    #[error("bad MCP server spec: {0}")]
    BadSpec(String),
    /// Failed to connect to / start the MCP server.
    #[error("MCP connect error: {0}")]
    Connect(String),
    /// The tool call itself failed (transport error, or the server returned an
    /// error result).
    #[error("MCP call error: {0}")]
    Call(String),
}

// ---------------------------------------------------------------------------
// Invoker trait.
// ---------------------------------------------------------------------------

/// A pluggable MCP client: connects to the server described by `server` and
/// invokes `tool` with `arguments`, returning the tool's result as JSON.
///
/// Implementations must be `Send + Sync` so the executor can hold one behind a
/// shared reference across `await` points. The default node uses
/// [`RmcpInvoker`]; tests inject a deterministic mock.
#[async_trait]
pub trait McpInvoker: Send + Sync {
    /// Invoke `tool` on the server described by `server` with `arguments`.
    ///
    /// # Errors
    /// Returns [`McpError`] on a bad spec, a connection failure, or a call
    /// failure (including a tool that reports an error result).
    async fn call_tool(
        &self,
        server: &McpServerSpec,
        tool: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, McpError>;
}

// ---------------------------------------------------------------------------
// Real implementation over the official rmcp client.
// ---------------------------------------------------------------------------

/// An [`McpInvoker`] backed by the official `rmcp` client.
///
/// For a [`McpServerSpec::Stdio`] spec it spawns the command as a child process
/// and speaks MCP over its stdin/stdout via `rmcp`'s `TokioChildProcess`
/// transport, performs the initialize handshake, calls the tool, then shuts the
/// connection (and child) down. A fresh connection is made per call: tool calls
/// from a workflow are infrequent and this keeps the invoker stateless and
/// `Clone`-free of shared mutable state.
#[derive(Debug, Default, Clone)]
pub struct RmcpInvoker;

impl RmcpInvoker {
    /// Construct a new invoker.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl McpInvoker for RmcpInvoker {
    async fn call_tool(
        &self,
        server: &McpServerSpec,
        tool: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        use rmcp::model::CallToolRequestParams;
        use rmcp::transport::TokioChildProcess;
        use rmcp::ServiceExt;

        let (command, args, env) = match server {
            McpServerSpec::Stdio { command, args, env } => (command, args, env),
            McpServerSpec::Http { url } => {
                return Err(McpError::Connect(format!(
                    "http MCP transport is not yet supported (server url '{url}'); \
                     use {{ \"transport\": \"stdio\", ... }}"
                )));
            }
        };

        // Build the child command. stderr is inherited (the server logs there);
        // stdin/stdout are wired to the MCP transport by `TokioChildProcess`.
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }

        let transport = TokioChildProcess::new(cmd)
            .map_err(|e| McpError::Connect(format!("failed to spawn MCP server '{command}': {e}")))?;

        // `()` is the unit client handler; `serve` performs the initialize
        // handshake and yields a RunningService that derefs to the client peer.
        let service = ()
            .serve(transport)
            .await
            .map_err(|e| McpError::Connect(format!("MCP initialize handshake failed: {e}")))?;

        // Arguments must be a JSON object (or absent). Anything else is a bad
        // spec from the caller, not a server error.
        let arguments = match arguments {
            serde_json::Value::Object(map) => Some(map),
            serde_json::Value::Null => None,
            other => {
                // Best-effort shutdown before returning the error.
                let _ = service.cancel().await;
                return Err(McpError::Call(format!(
                    "`arguments` must be a JSON object, got {other}"
                )));
            }
        };

        let mut params = CallToolRequestParams::new(tool.to_string());
        params.arguments = arguments;

        let call_result = service.call_tool(params).await;

        // Always tear the connection (and child process) down, regardless of the
        // call outcome.
        let _ = service.cancel().await;

        let result =
            call_result.map_err(|e| McpError::Call(format!("tool '{tool}' call failed: {e}")))?;

        // A tool that reports an error result is surfaced as a call error.
        if result.is_error.unwrap_or(false) {
            let detail = result
                .structured_content
                .clone()
                .map(|v| v.to_string())
                .or_else(|| text_of(&result))
                .unwrap_or_else(|| "tool reported an error".to_string());
            return Err(McpError::Call(format!("tool '{tool}' returned an error: {detail}")));
        }

        // Prefer the structured content; fall back to concatenated text content;
        // finally to JSON null so callers always get a value.
        let value = result
            .structured_content
            .clone()
            .or_else(|| text_of(&result).map(serde_json::Value::String))
            .unwrap_or(serde_json::Value::Null);
        Ok(value)
    }
}

/// Concatenate the text of every text content block in a tool result, if any.
fn text_of(result: &rmcp::model::CallToolResult) -> Option<String> {
    let mut out = String::new();
    for content in &result.content {
        if let Some(text) = content.as_text() {
            out.push_str(&text.text);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// Node executor.
// ---------------------------------------------------------------------------

/// Executor for [`a2w_ir::NodeKind::McpToolCall`].
///
/// Holds an [`McpInvoker`] behind an [`Arc`] so the transport is injectable:
/// the default uses [`RmcpInvoker`] (a real `rmcp` client), while tests inject a
/// deterministic mock.
#[derive(Clone)]
pub struct McpToolCall {
    invoker: Arc<dyn McpInvoker>,
}

impl std::fmt::Debug for McpToolCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolCall").finish_non_exhaustive()
    }
}

impl Default for McpToolCall {
    fn default() -> Self {
        Self {
            invoker: Arc::new(RmcpInvoker::new()),
        }
    }
}

impl McpToolCall {
    /// Construct with an explicit [`McpInvoker`] (e.g. a mock in tests).
    #[must_use]
    pub fn new(invoker: Arc<dyn McpInvoker>) -> Self {
        Self { invoker }
    }

    /// Read `tool` from params (required, non-empty string).
    fn tool(params: &serde_json::Value) -> Result<&str, NodeError> {
        params
            .get("tool")
            .and_then(serde_json::Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| NodeError::BadParams("McpToolCall requires a non-empty string `tool`".into()))
    }

    /// Parse the `server` spec from params, mapping a bad spec to
    /// [`NodeError::BadParams`].
    fn server(params: &serde_json::Value) -> Result<McpServerSpec, NodeError> {
        let raw = params.get("server").cloned().unwrap_or(serde_json::Value::Null);
        McpServerSpec::from_params(&raw).map_err(|e| NodeError::BadParams(e.to_string()))
    }

    /// The `arguments` param (defaults to an empty object when absent).
    fn arguments(params: &serde_json::Value) -> serde_json::Value {
        params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}))
    }
}

#[async_trait]
impl NodeExecutor for McpToolCall {
    fn has_side_effects(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        // Parse + validate params once, up front, so bad params surface as
        // BadParams before any side effect.
        let server = Self::server(&ctx.params)?;
        let tool = Self::tool(&ctx.params)?.to_string();
        let arguments = Self::arguments(&ctx.params);

        // Produce one output item per input item. With no input, still emit one
        // item so a tool call with no upstream data is observable.
        let count = input.len().max(1);
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let result = self
                .invoker
                .call_tool(&server, &tool, arguments.clone())
                .await
                .map_err(|e| NodeError::Runtime(e.to_string()))?;
            out.push(Item::produced(
                serde_json::json!({ "tool": tool, "result": result }),
                ctx.node_id.clone(),
                0,
            ));
        }
        Ok(out)
    }

    async fn dry_run(&self, ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        // Validate the params shape even on a dry run, then mock per input item.
        let server = Self::server(&ctx.params)?;
        let tool = Self::tool(&ctx.params)?.to_string();

        // A small, inspectable summary of where the call would have gone.
        let server_summary = match &server {
            McpServerSpec::Stdio { command, .. } => {
                serde_json::json!({ "transport": "stdio", "command": command })
            }
            McpServerSpec::Http { url } => {
                serde_json::json!({ "transport": "http", "url": url })
            }
        };

        let count = input.len().max(1);
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(Item::produced(
                serde_json::json!({ "_mock": true, "tool": tool, "server": server_summary }),
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
    use a2w_engine::ExecutionMode;
    use a2w_ir::NodeKind;

    /// A deterministic, network-free invoker for tests: records its inputs and
    /// returns a canned value (or a forced error).
    #[derive(Debug, Default)]
    struct MockInvoker {
        canned: serde_json::Value,
        fail: bool,
    }

    #[async_trait]
    impl McpInvoker for MockInvoker {
        async fn call_tool(
            &self,
            _server: &McpServerSpec,
            tool: &str,
            arguments: serde_json::Value,
        ) -> Result<serde_json::Value, McpError> {
            if self.fail {
                return Err(McpError::Call("mock failure".into()));
            }
            // Echo back enough to prove the right tool/args reached the invoker.
            Ok(serde_json::json!({
                "echoed_tool": tool,
                "echoed_args": arguments,
                "canned": self.canned.clone(),
            }))
        }
    }

    fn ctx(params: serde_json::Value, mode: ExecutionMode) -> NodeContext {
        NodeContext {
            run_id: "run".into(),
            node_id: "mcp".into(),
            kind: NodeKind::McpToolCall,
            params,
            mode,
        }
    }

    #[test]
    fn spec_parses_stdio_with_args_and_env() {
        let spec = McpServerSpec::from_params(&serde_json::json!({
            "transport": "stdio",
            "command": "a2w-mcp",
            "args": ["--flag"],
            "env": { "K": "V" },
        }))
        .expect("valid stdio spec");
        match spec {
            McpServerSpec::Stdio { command, args, env } => {
                assert_eq!(command, "a2w-mcp");
                assert_eq!(args, vec!["--flag".to_string()]);
                assert_eq!(env.get("K").map(String::as_str), Some("V"));
            }
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn spec_defaults_args_and_env() {
        let spec = McpServerSpec::from_params(&serde_json::json!({
            "transport": "stdio",
            "command": "x",
        }))
        .expect("valid minimal stdio spec");
        assert_eq!(
            spec,
            McpServerSpec::Stdio {
                command: "x".into(),
                args: vec![],
                env: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn spec_rejects_unknown_transport_and_null() {
        assert!(matches!(
            McpServerSpec::from_params(&serde_json::json!({ "transport": "carrier-pigeon" })),
            Err(McpError::BadSpec(_))
        ));
        assert!(matches!(
            McpServerSpec::from_params(&serde_json::Value::Null),
            Err(McpError::BadSpec(_))
        ));
    }

    #[tokio::test]
    async fn execute_returns_one_item_per_input_with_result() {
        let node = McpToolCall::new(Arc::new(MockInvoker {
            canned: serde_json::json!({ "ok": 1 }),
            fail: false,
        }));
        let ctx = ctx(
            serde_json::json!({
                "server": { "transport": "stdio", "command": "x" },
                "tool": "do",
                "arguments": { "a": 1 },
            }),
            ExecutionMode::Run,
        );
        let input = vec![Item::root(serde_json::json!({})), Item::root(serde_json::json!({}))];
        let out = node.execute(&ctx, input).await.expect("execute ok");
        assert_eq!(out.len(), 2, "one item per input item");
        for item in &out {
            assert_eq!(item.json["tool"], serde_json::json!("do"));
            assert_eq!(item.json["result"]["echoed_tool"], serde_json::json!("do"));
            assert_eq!(item.json["result"]["echoed_args"], serde_json::json!({ "a": 1 }));
            assert_eq!(item.json["result"]["canned"], serde_json::json!({ "ok": 1 }));
        }
    }

    #[tokio::test]
    async fn execute_with_no_input_still_emits_one_item() {
        let node = McpToolCall::new(Arc::new(MockInvoker::default()));
        let ctx = ctx(
            serde_json::json!({
                "server": { "transport": "stdio", "command": "x" },
                "tool": "do",
            }),
            ExecutionMode::Run,
        );
        let out = node.execute(&ctx, vec![]).await.expect("execute ok");
        assert_eq!(out.len(), 1);
        // Missing `arguments` defaults to an empty object.
        assert_eq!(out[0].json["result"]["echoed_args"], serde_json::json!({}));
    }

    #[tokio::test]
    async fn execute_missing_tool_is_bad_params() {
        let node = McpToolCall::new(Arc::new(MockInvoker::default()));
        let ctx = ctx(
            serde_json::json!({ "server": { "transport": "stdio", "command": "x" } }),
            ExecutionMode::Run,
        );
        let err = node.execute(&ctx, vec![]).await.expect_err("missing tool");
        assert!(matches!(err, NodeError::BadParams(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn execute_bad_server_is_bad_params() {
        let node = McpToolCall::new(Arc::new(MockInvoker::default()));
        let ctx = ctx(
            serde_json::json!({ "server": { "transport": "nope" }, "tool": "do" }),
            ExecutionMode::Run,
        );
        let err = node.execute(&ctx, vec![]).await.expect_err("bad server");
        assert!(matches!(err, NodeError::BadParams(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn execute_invoker_error_is_runtime() {
        let node = McpToolCall::new(Arc::new(MockInvoker {
            canned: serde_json::Value::Null,
            fail: true,
        }));
        let ctx = ctx(
            serde_json::json!({
                "server": { "transport": "stdio", "command": "x" },
                "tool": "do",
            }),
            ExecutionMode::Run,
        );
        let err = node
            .execute(&ctx, vec![Item::root(serde_json::json!({}))])
            .await
            .expect_err("invoker failed");
        assert!(matches!(err, NodeError::Runtime(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn dry_run_returns_mock_without_invoking() {
        // The invoker is set to fail; dry_run must NOT call it, so this succeeds.
        let node = McpToolCall::new(Arc::new(MockInvoker {
            canned: serde_json::Value::Null,
            fail: true,
        }));
        let ctx = ctx(
            serde_json::json!({
                "server": { "transport": "stdio", "command": "a2w-mcp" },
                "tool": "do",
            }),
            ExecutionMode::DryRun,
        );
        let out = node
            .dry_run(&ctx, vec![Item::root(serde_json::json!({}))])
            .await
            .expect("dry_run ok");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].json["_mock"], serde_json::json!(true));
        assert_eq!(out[0].json["tool"], serde_json::json!("do"));
        assert_eq!(out[0].json["server"]["transport"], serde_json::json!("stdio"));
        assert_eq!(out[0].json["server"]["command"], serde_json::json!("a2w-mcp"));
    }

    #[tokio::test]
    async fn dry_run_validates_params() {
        let node = McpToolCall::default();
        let ctx = ctx(
            serde_json::json!({ "server": { "transport": "stdio", "command": "x" } }),
            ExecutionMode::DryRun,
        );
        let err = node.dry_run(&ctx, vec![]).await.expect_err("missing tool");
        assert!(matches!(err, NodeError::BadParams(_)), "got {err:?}");
    }
}
