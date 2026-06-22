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
// MCP stdio command allowlist.
// ---------------------------------------------------------------------------

/// Check `command` against a parsed allowlist string (comma-separated).
///
/// This is the pure inner logic, separated from env-var reading so it can be
/// unit-tested without mutating the process environment.
///
/// # Fail-closed policy
/// If `allowlist_raw` is empty (representing an unset or empty env var), ALL
/// commands are rejected.
///
/// # Errors
/// Returns [`McpError::BadSpec`] when the command is not allowed.
pub fn check_mcp_command_allowed_with_list(
    command: &str,
    allowlist_raw: &str,
) -> Result<(), McpError> {
    let allowed: Vec<&str> = allowlist_raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if allowed.is_empty() {
        return Err(McpError::BadSpec(format!(
            "stdio MCP spawn of '{command}' rejected: A2W_MCP_ALLOWED_COMMANDS is not set. \
             Set it to a comma-separated list of permitted commands, e.g. \
             A2W_MCP_ALLOWED_COMMANDS=a2w-mcp,my-mcp-server"
        )));
    }

    if allowed.contains(&command) {
        Ok(())
    } else {
        Err(McpError::BadSpec(format!(
            "stdio MCP spawn of '{command}' rejected: not in the allowlist \
             (A2W_MCP_ALLOWED_COMMANDS={allowlist_raw})"
        )))
    }
}

/// Env keys an MCP child must not be allowed to receive: dynamic-loader
/// overrides, interpreter-hijack variables, TLS-trust overrides, and basic
/// process-environment knobs (PATH/HOME) that would let workflow-supplied env
/// turn an allowlisted command into an arbitrary code loader or CA-trust
/// bypass. Match is case-insensitive — macOS DYLD_* are case-sensitive but
/// the safer default is to reject any case variant.
///
/// Audit-2 expansion: added JVM (JAVA_TOOL_OPTIONS et al.), Python (PYTHONHOME,
/// PYTHONBREAKPOINT, PYTHONINSPECT), Node (NODE_PATH), glibc loader-adjacent
/// (GCONV_PATH, LOCPATH, NLSPATH, RESOLV_HOST_CONF, HOSTALIASES), TLS trust
/// (SSL_CERT_FILE, SSL_CERT_DIR, CURL_CA_BUNDLE, REQUESTS_CA_BUNDLE,
/// GIT_SSL_CAINFO), GTK/Qt plugin paths, shell sidecars, and the basic
/// PATH/HOME/XDG keys.
///
/// Also rejects any key starting with `BASH_FUNC_` (function-export
/// shellshock pattern) or `DYLD_` (any macOS dyld variant).
fn is_dangerous_env_key(k: &str) -> bool {
    const DANGEROUS: &[&str] = &[
        // Dynamic loader (Linux).
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "LD_AUDIT",
        "LD_BIND_NOW",
        "LD_DEBUG",
        "LD_PROFILE",
        // Dynamic loader (macOS) — additional DYLD_* caught by prefix below.
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
        "DYLD_FORCE_FLAT_NAMESPACE",
        // glibc loader-adjacent.
        "GCONV_PATH",
        "LOCPATH",
        "NLSPATH",
        "RESOLV_HOST_CONF",
        "HOSTALIASES",
        "GLIBC_TUNABLES",
        "MALLOC_CHECK_",
        // Python.
        "PYTHONPATH",
        "PYTHONHOME",
        "PYTHONBREAKPOINT",
        "PYTHONINSPECT",
        "PYTHONSTARTUP",
        "PYTHONIOENCODING",
        // Node.
        "NODE_OPTIONS",
        "NODE_PATH",
        // Perl / Ruby.
        "PERL5LIB",
        "PERL5OPT",
        "RUBYLIB",
        "RUBYOPT",
        // JVM.
        "JAVA_TOOL_OPTIONS",
        "_JAVA_OPTIONS",
        "JDK_JAVA_OPTIONS",
        "CLASSPATH",
        "ANT_OPTS",
        // Shell sidecars.
        "BASH_ENV",
        "ENV",
        "PROMPT_COMMAND",
        "SHELLOPTS",
        "ZDOTDIR",
        // Process basics — letting a workflow override PATH means the child
        // can re-exec to attacker-controlled binaries on any sub-exec.
        "PATH",
        "HOME",
        "TMPDIR",
        // GUI plugin paths (any allowlisted command that loads GTK/Qt
        // becomes a plugin loader otherwise).
        "GTK_PATH",
        "QT_PLUGIN_PATH",
        "GIO_EXTRA_MODULES",
        // TLS trust — overriding any of these lets a workflow MITM the
        // child's outbound TLS.
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "CURL_CA_BUNDLE",
        "REQUESTS_CA_BUNDLE",
        "GIT_SSL_CAINFO",
        "NODE_EXTRA_CA_CERTS",
    ];
    if DANGEROUS.iter().any(|d| d.eq_ignore_ascii_case(k)) {
        return true;
    }
    // Catch-all prefixes for variant-rich families.
    let upper = k.to_ascii_uppercase();
    upper.starts_with("DYLD_") || upper.starts_with("BASH_FUNC_") || upper.starts_with("XDG_")
}

/// Check that `command` appears in the `A2W_MCP_ALLOWED_COMMANDS` allowlist.
///
/// Reads `A2W_MCP_ALLOWED_COMMANDS` from the environment at call time and
/// delegates to [`check_mcp_command_allowed_with_list`].
///
/// # Fail-closed policy
/// If the env var is **unset or empty**, ALL stdio MCP spawns are rejected.
/// The operator must explicitly opt in by naming each permitted command.
///
/// # Errors
/// Returns [`McpError::BadSpec`] when the command is not allowed.
pub fn check_mcp_command_allowed(command: &str) -> Result<(), McpError> {
    let raw = std::env::var("A2W_MCP_ALLOWED_COMMANDS").unwrap_or_default();
    check_mcp_command_allowed_with_list(command, &raw)
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

        // Security: check `command` against the process-wide allowlist before
        // spawning. The allowlist is read from A2W_MCP_ALLOWED_COMMANDS (a
        // comma-separated list). When the env var is UNSET or EMPTY we fail
        // closed: no stdio MCP commands may spawn without an explicit allowlist.
        check_mcp_command_allowed(command)?;

        // Build the child command. stderr is inherited (the server logs there);
        // stdin/stdout are wired to the MCP transport by `TokioChildProcess`.
        //
        // Security: env_clear() strips the parent environment so the child
        // cannot read A2W_MASTER_KEY, ANTHROPIC_API_KEY, or other host secrets.
        // Only the explicit `env` entries from the workflow spec are forwarded.
        //
        // Audit-fix: refuse to forward env keys that influence dynamic loader
        // behaviour. A workflow that sets LD_PRELOAD or DYLD_INSERT_LIBRARIES
        // could turn any allowlisted command into a shellcode loader; an
        // attacker who could write workflows shouldn't get that capability.
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args);
        cmd.env_clear();
        for (k, v) in env {
            if is_dangerous_env_key(k) {
                return Err(McpError::BadSpec(format!(
                    "env key '{k}' is rejected: dynamic-loader / library-injection \
                     variables are not permitted in MCP spawn env"
                )));
            }
            cmd.env(k, v);
        }

        let transport = TokioChildProcess::new(cmd).map_err(|e| {
            McpError::Connect(format!("failed to spawn MCP server '{command}': {e}"))
        })?;

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
            return Err(McpError::Call(format!(
                "tool '{tool}' returned an error: {detail}"
            )));
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
            .ok_or_else(|| {
                NodeError::BadParams("McpToolCall requires a non-empty string `tool`".into())
            })
    }

    /// Parse the `server` spec from params, mapping a bad spec to
    /// [`NodeError::BadParams`].
    fn server(params: &serde_json::Value) -> Result<McpServerSpec, NodeError> {
        let raw = params
            .get("server")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
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

        // Produce one output item per input item. Audit-2 fix (CRITICAL —
        // unselected-branch side effect): when input is empty AND the node
        // has any incoming connection, we MUST NOT fire the tool — the empty
        // input means a port-routing producer routed elsewhere. The previous
        // `.max(1)` semantics (always fire once) reintroduced side effects on
        // the unselected branch arm. A trigger-rooted MCP node (no incoming)
        // still gets one synthetic call via the engine seeding it with a
        // single trigger item, so observability of "tool called with no data"
        // is preserved at the trigger layer.
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let count = input.len();
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let result = self
                .invoker
                .call_tool(&server, &tool, arguments.clone())
                .await
                .map_err(|e| NodeError::Runtime(e.to_string()))?;
            // A real MCP tool invocation completed — report it to the engine.
            ctx.record_external_call();
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

        // Symmetry with `execute`: no input → no mock items, so unselected
        // branch arms produce zero events in dry-run too.
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let count = input.len();
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
            credentials: None,
            sub_workflows: None,
            sub_workflow_depth: 0,
            workflow_id: None,
            approvals: None,
            metrics: None,
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
        let input = vec![
            Item::root(serde_json::json!({})),
            Item::root(serde_json::json!({})),
        ];
        let out = node.execute(&ctx, input).await.expect("execute ok");
        assert_eq!(out.len(), 2, "one item per input item");
        for item in &out {
            assert_eq!(item.json["tool"], serde_json::json!("do"));
            assert_eq!(item.json["result"]["echoed_tool"], serde_json::json!("do"));
            assert_eq!(
                item.json["result"]["echoed_args"],
                serde_json::json!({ "a": 1 })
            );
            assert_eq!(
                item.json["result"]["canned"],
                serde_json::json!({ "ok": 1 })
            );
        }
    }

    #[tokio::test]
    async fn execute_with_no_input_emits_zero_items_audit2() {
        // Audit-2: empty input → no tool calls. This was previously
        // `.max(1)` which would silently fire the tool on the unselected
        // branch arm of a Branch/Switch.
        let node = McpToolCall::new(Arc::new(MockInvoker::default()));
        let ctx = ctx(
            serde_json::json!({
                "server": { "transport": "stdio", "command": "x" },
                "tool": "do",
            }),
            ExecutionMode::Run,
        );
        let out = node.execute(&ctx, vec![]).await.expect("execute ok");
        assert!(out.is_empty(), "no input → no side-effect, no items");
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
        assert_eq!(
            out[0].json["server"]["transport"],
            serde_json::json!("stdio")
        );
        assert_eq!(
            out[0].json["server"]["command"],
            serde_json::json!("a2w-mcp")
        );
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

    // -----------------------------------------------------------------------
    // Command allowlist tests (pure — no env mutation needed).
    // -----------------------------------------------------------------------

    #[test]
    fn allowlist_unset_fails_closed() {
        // Empty string simulates an unset / empty A2W_MCP_ALLOWED_COMMANDS.
        let err = check_mcp_command_allowed_with_list("sh", "")
            .expect_err("must reject when allowlist is empty");
        assert!(matches!(err, McpError::BadSpec(_)), "got {err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("A2W_MCP_ALLOWED_COMMANDS"),
            "error should name the env var; got: {msg}"
        );
    }

    #[test]
    fn allowlist_command_not_listed_is_rejected() {
        let err = check_mcp_command_allowed_with_list("sh", "a2w-mcp,my-server")
            .expect_err("sh not in allowlist");
        assert!(matches!(err, McpError::BadSpec(_)), "got {err:?}");
    }

    #[test]
    fn allowlist_command_listed_is_allowed() {
        check_mcp_command_allowed_with_list("a2w-mcp", "a2w-mcp,my-server")
            .expect("a2w-mcp is in the allowlist");
        check_mcp_command_allowed_with_list("my-server", "a2w-mcp,my-server")
            .expect("my-server is in the allowlist");
    }

    #[test]
    fn allowlist_whitespace_trimmed() {
        check_mcp_command_allowed_with_list("a2w-mcp", " a2w-mcp , my-server ")
            .expect("whitespace around entries must be trimmed");
    }

    #[test]
    fn allowlist_case_sensitive() {
        // The match is case-sensitive; "SH" != "sh".
        let err = check_mcp_command_allowed_with_list("SH", "sh,a2w-mcp")
            .expect_err("case mismatch must be rejected");
        assert!(matches!(err, McpError::BadSpec(_)), "got {err:?}");
    }
}
