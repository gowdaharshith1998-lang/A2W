//! # a2w-mcp
//!
//! An **MCP (Model Context Protocol) server** that exposes A2W's already-built,
//! already-tested workflow logic as agent-callable `wf_*` tools over the stdio
//! transport. It is the *wire surface* over the core crates — it adds no new
//! workflow semantics, only a protocol shell an AI agent can drive.
//!
//! ## Tools
//! | Tool               | Input                                   | Output (JSON) |
//! |--------------------|-----------------------------------------|---------------|
//! | `wf_get_schema`    | none                                    | the `Workflow` JSON Schema |
//! | `wf_describe_nodes`| none                                    | node taxonomy array |
//! | `wf_validate`      | `{ workflow }`                          | `ValidationReport` |
//! | `wf_dry_run`       | `{ workflow, trigger_input }`           | `RunResult` (mocked side effects) |
//! | `wf_run`           | `{ workflow, trigger_input }`           | `RunResult` (real side effects) |
//! | `wf_run_tests`     | `{ workflow, tests }`                   | `Vec<TestResult>` |
//! | `wf_profile`       | `{ workflow, trigger_input }`           | `RunProfile` |
//! | `wf_optimize`      | `{ workflow, with_profile?, trigger_input? }` | `Vec<Suggestion>` |
//! | `wf_apply_ops`     | `{ workflow, ops }`                     | the new `Workflow` |
//! | `wf_search_templates` | `{ query }`                          | template summaries |
//! | `wf_get_template`  | `{ id }`                                | a template `Workflow` |
//! | `generate_workflow_from_prompt` | `{ prompt, max_repairs? }`| `AuthorOutcome` (needs `ANTHROPIC_API_KEY`) |
//!
//! ## Untrusted input
//! Agent-supplied workflow JSON is treated as untrusted: it is parsed via serde
//! and any failure is mapped to a clean MCP tool error
//! ([`rmcp::ErrorData::invalid_params`]) — the server never panics on bad input.
//!
//! ## Testability
//! Every tool's behaviour lives in a plain `async fn` on [`A2wServer`] (the
//! `*_logic` methods), returning `Result<serde_json::Value, ErrorData>`. The
//! `#[tool]` methods are thin wrappers that turn the value into a
//! [`rmcp::model::CallToolResult`]. Tests call the `*_logic` functions directly,
//! exercising the real core logic without the transport.
//!
//! ## schemars versions
//! The A2W core crates derive their JSON Schema with **schemars 0.8**, while
//! rmcp 1.7's tool macros generate code against **schemars 1.x**. The two
//! coexist: tool *input* types here derive [`JsonSchema`](rmcp::schemars) from
//! rmcp's re-exported schemars 1.x, while tool *output* types are only
//! `Serialize` and are emitted via [`rmcp::model::CallToolResult::structured`],
//! so the 0.8 schema (returned verbatim by `wf_get_schema`) is just serialized
//! as data and never touches a type-level schemars boundary.

#![forbid(unsafe_code)]

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::schemars::{self, JsonSchema};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use serde::Deserialize;
use serde_json::{json, Value};

use a2w_author::{generate_workflow_from_prompt, AuthorConfig};
use a2w_engine::{Engine, ExecutionMode, MemoryEventLog, RunResult};
use a2w_ir::{NodeKind, Workflow};
use a2w_llm::{AnthropicClient, LlmClient};
use a2w_optimizer::{analyze, apply, profile, IrOp};
use a2w_skills::PersistentSkillLibrary;
use a2w_store::{Store, StoreCredentialResolver, Vault};
use a2w_testkit::{run_tests, TestCase};
use a2w_verify::{
    verify, GoldenFixture, MetamorphicSuite, SemanticRelation, SemanticSuite, VerificationHarness,
    VerificationPlan, WorkflowSpec,
};

// ---------------------------------------------------------------------------
// Tool input parameter types.
//
// These derive `JsonSchema` from rmcp's re-exported schemars (1.x) so the
// `#[tool]` macro can generate an input schema. They are deliberately small and
// forgiving: the `workflow` field is an untyped `serde_json::Value` that we
// parse into a `Workflow` inside the logic, so malformed workflow JSON yields a
// clean tool error rather than a hard deserialization failure on the whole
// request envelope.
// ---------------------------------------------------------------------------

/// Input carrying a single workflow document.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowInput {
    /// The workflow IR document (see `wf_get_schema` for its shape).
    pub workflow: Value,
}

/// Input carrying a workflow plus trigger seed items.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunInput {
    /// The workflow IR document.
    pub workflow: Value,
    /// Root items seeded into the trigger node (one [`Item`](a2w_engine::Item)
    /// per JSON value). May be empty.
    #[serde(default)]
    pub trigger_input: Vec<Value>,
}

/// Input carrying a workflow plus a list of declarative test cases.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunTestsInput {
    /// The workflow IR document.
    pub workflow: Value,
    /// Declarative test cases to evaluate (run in DryRun mode).
    #[serde(default)]
    pub tests: Vec<Value>,
}

/// Input for `wf_optimize`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct OptimizeInput {
    /// The workflow IR document.
    pub workflow: Value,
    /// When `true`, first DryRun + profile the workflow so suggestions carry
    /// `estimated_gain_ms` (and dead-node suggestions are included).
    #[serde(default)]
    pub with_profile: bool,
    /// Trigger seed items, used only when `with_profile` is set.
    #[serde(default)]
    pub trigger_input: Vec<Value>,
}

/// Input for `wf_apply_ops`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApplyOpsInput {
    /// The workflow IR document.
    pub workflow: Value,
    /// IR diff ops to apply (e.g. from `wf_optimize` suggestions).
    #[serde(default)]
    pub ops: Vec<Value>,
}

/// Input for `wf_search_templates`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchTemplatesInput {
    /// Free-text query; matched case-insensitively against template name,
    /// description, and tags (any query word matching any field is a hit).
    pub query: String,
}

/// Input for `wf_get_template`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTemplateInput {
    /// The template id to fetch (see `wf_search_templates`).
    pub id: String,
}

/// Input for `generate_workflow_from_prompt`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateInput {
    /// Plain-English description of the workflow to author.
    pub prompt: String,
    /// Maximum number of repair attempts after the initial generation. Defaults
    /// to 3 when omitted.
    #[serde(default)]
    pub max_repairs: Option<u32>,
}

/// Input for `wf_store_credential` — upsert one credential under its id.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct StoreCredentialInput {
    /// Stable identifier; matches the `credential_ref` used in workflow nodes.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Plaintext secret. Encrypted under the AES-256-GCM master key and never
    /// returned by any later tool call.
    pub secret: String,
}

/// Input for `wf_delete_credential` — delete one credential by id.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteCredentialInput {
    /// The credential id to delete (no-op when absent).
    pub id: String,
}

/// A verification plan, as untyped JSON parts (parsed into the typed
/// `a2w_verify` plan inside the logic). Engine-invariant relations
/// (`metamorphic`) verify the engine; everything else is OUTCOME evidence.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct VerifyInput {
    /// The workflow IR document.
    pub workflow: Value,
    /// The node whose output is treated as "the result".
    pub observe_node: String,
    /// Optional spec assertions `{ input, assertions }` (outcome evidence).
    #[serde(default)]
    pub spec: Option<Value>,
    /// Optional golden fixtures `[{ name, input, expected, match_mode }]`
    /// (outcome evidence).
    #[serde(default)]
    pub golden: Option<Value>,
    /// Optional spec-derived semantic relations (outcome evidence).
    #[serde(default)]
    pub semantic: Option<Value>,
    /// Optional engine-invariant metamorphic suite (NOT outcome evidence).
    #[serde(default)]
    pub metamorphic: Option<Value>,
}

/// Input for `wf_promote_skill`: verify on the supplied HOLDOUT plan, then
/// persist iff it clears the threshold.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PromoteSkillInput {
    /// The natural-language query this workflow solves.
    pub query: String,
    /// The workflow IR document.
    pub workflow: Value,
    /// The node whose output is "the result".
    pub observe_node: String,
    /// The holdout verification plan (same shape as `wf_verify`, minus
    /// `workflow`/`observe_node`): `{ spec?, golden?, semantic?, metamorphic? }`.
    /// Promotion is gated on the report this plan produces.
    pub holdout: Value,
}

/// Input for `wf_find_skill`: retrieve persisted skills by query signature.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindSkillInput {
    /// Free-text query; ranked by task-signature similarity.
    pub query: String,
    /// Max number of skills to return (defaults to 5).
    #[serde(default)]
    pub k: Option<usize>,
}

// ---------------------------------------------------------------------------
// Tool-allow policy.
//
// The MCP stdio transport is **local-trust**: anyone who can spawn this
// process has full access to its tools (there is no per-call authentication
// over stdio). We therefore expose a `McpPolicy` so the operator can
// explicitly opt in to:
//
//   * actually executing real side effects (`wf_run`)
//   * spending LLM budget (`generate_workflow_from_prompt`)
//   * writing or deleting credentials (`wf_store_credential` /
//     `wf_delete_credential`)
//
// The default policy is **read-only**: validate / dry-run / profile / optimize
// / list templates / list credentials succeed; the gated tools return a clean
// `invalid_params`-class error naming the env var the operator must set.
// ---------------------------------------------------------------------------

/// Per-tool allowlist enforced on top of the engine wiring.
#[derive(Debug, Clone)]
pub struct McpPolicy {
    /// Allow `wf_run` to execute real side effects (HTTP, MCP child processes,
    /// WASM code steps). Set via `A2W_MCP_ALLOW_RUN=true`.
    pub allow_run: bool,
    /// Allow `generate_workflow_from_prompt` (costs LLM budget). Set via
    /// `A2W_MCP_ALLOW_LLM=true`.
    pub allow_llm: bool,
    /// Allow `wf_store_credential` and `wf_delete_credential` to mutate the
    /// vault. Set via `A2W_MCP_ALLOW_CREDENTIAL_WRITES=true`. (Reading the
    /// credential listing — id + name + created_at only — is always allowed
    /// when the vault is configured.)
    pub allow_credential_writes: bool,
}

impl Default for McpPolicy {
    /// Fail closed: every destructive tool is disabled.
    fn default() -> Self {
        Self {
            allow_run: false,
            allow_llm: false,
            allow_credential_writes: false,
        }
    }
}

impl McpPolicy {
    /// Build a policy from `A2W_MCP_ALLOW_*` env vars (defaults to read-only).
    #[must_use]
    pub fn from_env() -> Self {
        fn bool_env(name: &str) -> bool {
            std::env::var(name)
                .ok()
                .map(|v| {
                    let v = v.trim();
                    v.eq_ignore_ascii_case("true") || v == "1"
                })
                .unwrap_or(false)
        }
        Self {
            allow_run: bool_env("A2W_MCP_ALLOW_RUN"),
            allow_llm: bool_env("A2W_MCP_ALLOW_LLM"),
            allow_credential_writes: bool_env("A2W_MCP_ALLOW_CREDENTIAL_WRITES"),
        }
    }

    /// Policy that allows every tool. Use in tests or controlled environments
    /// only.
    #[must_use]
    pub fn allow_all() -> Self {
        Self {
            allow_run: true,
            allow_llm: true,
            allow_credential_writes: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Server.
// ---------------------------------------------------------------------------

/// The A2W MCP server.
///
/// Holds one shared [`Engine`] (built once from `a2w_nodes::default_registry()`)
/// behind an [`Arc`] so every tool call reuses the same registry (and its
/// pooled HTTP client). The struct is [`Clone`] because rmcp's service layer may
/// clone the handler; cloning is cheap (an `Arc` bump plus the router).
///
/// When constructed via [`A2wServer::with_vault`], the engine is wired with a
/// [`StoreCredentialResolver`] and the `wf_*_credential` tools become live;
/// otherwise those tools return a `service unavailable`-class tool error.
#[derive(Clone)]
pub struct A2wServer {
    engine: Arc<Engine>,
    /// Persisted store, shared with the credential vault. `None` when the
    /// server runs in stateless mode (no vault, no run history persistence).
    store: Option<Arc<Store>>,
    /// AES-256-GCM credential vault. `None` when `A2W_MASTER_KEY` is unset.
    vault: Option<Arc<Vault>>,
    /// Per-tool allowlist; defaults to read-only, fail-closed.
    policy: McpPolicy,
    // Read by the `#[tool_handler]`-generated `ServerHandler` impl to dispatch
    // tool calls. The dead-code lint can't see that use because it lives in a
    // separate, macro-generated impl block, so it spuriously flags the field;
    // `allow` (not `expect`) keeps it quiet across rmcp versions that may or may
    // not make the use visible.
    #[allow(dead_code)]
    tool_router: ToolRouter<A2wServer>,
}

impl Default for A2wServer {
    fn default() -> Self {
        Self::new()
    }
}

/// One entry of the `wf_describe_nodes` taxonomy.
#[derive(Debug, serde::Serialize)]
struct NodeKindInfo {
    /// snake_case wire name (matches the IR's `kind` field).
    name: &'static str,
    /// Number of output ports, or `null` when the kind has dynamic ports
    /// (currently only `switch`).
    output_port_count: Option<usize>,
    /// Whether the port count is determined by params rather than the kind.
    dynamic_ports: bool,
    /// Whether this kind is a workflow entry point.
    is_trigger: bool,
}

/// Every [`NodeKind`] variant, in declaration order, for the taxonomy tool.
const ALL_NODE_KINDS: &[NodeKind] = &[
    NodeKind::WebhookTrigger,
    NodeKind::ScheduleTrigger,
    NodeKind::HttpRequest,
    NodeKind::McpToolCall,
    NodeKind::Transform,
    NodeKind::Branch,
    NodeKind::Switch,
    NodeKind::Loop,
    NodeKind::Merge,
    NodeKind::Wait,
    NodeKind::SubWorkflow,
    NodeKind::LlmCall,
    NodeKind::CodeStep,
    NodeKind::Approval,
];

/// snake_case wire name for a [`NodeKind`] (matches the IR serde rename).
fn node_kind_name(kind: NodeKind) -> &'static str {
    match kind {
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

impl A2wServer {
    /// Construct a server with a fresh engine, no credential vault, and a
    /// **read-only** policy. HTTP / MCP nodes that name a `credential_ref`
    /// will fail closed, and `wf_run` / `generate_workflow_from_prompt` /
    /// `wf_*_credential` writes will all be rejected.
    #[must_use]
    pub fn new() -> Self {
        Self::with_policy(McpPolicy::default())
    }

    /// Construct a stateless server with an explicit [`McpPolicy`].
    #[must_use]
    pub fn with_policy(policy: McpPolicy) -> Self {
        Self {
            engine: Arc::new(Engine::new(a2w_nodes::default_registry())),
            store: None,
            vault: None,
            policy,
            tool_router: Self::tool_router(),
        }
    }

    /// Construct a server with a shared [`Store`] + [`Vault`], wiring the
    /// vault-backed [`StoreCredentialResolver`] into the engine so the
    /// `credential_ref` machinery is live for `wf_run` and `wf_dry_run`. The
    /// policy defaults to read-only; use [`A2wServer::with_vault_and_policy`]
    /// to opt in to destructive tools.
    #[must_use]
    pub fn with_vault(store: Arc<Store>, vault: Arc<Vault>) -> Self {
        Self::with_vault_and_policy(store, vault, McpPolicy::default())
    }

    /// Construct a server with a vault **and** an explicit [`McpPolicy`].
    #[must_use]
    pub fn with_vault_and_policy(store: Arc<Store>, vault: Arc<Vault>, policy: McpPolicy) -> Self {
        let resolver = Arc::new(StoreCredentialResolver::new(
            Arc::clone(&store),
            Arc::clone(&vault),
        ));
        let engine = Engine::new(a2w_nodes::default_registry()).with_credentials(resolver);
        Self {
            engine: Arc::new(engine),
            store: Some(store),
            vault: Some(vault),
            policy,
            tool_router: Self::tool_router(),
        }
    }

    /// Read access to the server's policy (mostly for tests).
    #[must_use]
    pub fn policy(&self) -> &McpPolicy {
        &self.policy
    }

    /// Borrow `(store, vault)` if both are configured, else return an
    /// `invalid_params` tool error that names the missing env var.
    fn require_vault(&self) -> Result<(&Arc<Store>, &Arc<Vault>), ErrorData> {
        match (self.store.as_ref(), self.vault.as_ref()) {
            (Some(s), Some(v)) => Ok((s, v)),
            _ => Err(ErrorData::invalid_params(
                "credential tools disabled: server was started without \
                 A2W_MASTER_KEY (set A2W_MASTER_KEY to a base64 32-byte key \
                 and restart)",
                None,
            )),
        }
    }

    /// Reject the call when `flag` is false, naming the env var the operator
    /// must set to enable it.
    fn require_policy(&self, flag: bool, env_var: &str, tool: &str) -> Result<(), ErrorData> {
        if flag {
            Ok(())
        } else {
            Err(ErrorData::invalid_params(
                format!(
                    "tool '{tool}' disabled by policy: server was started without \
                     {env_var}=true. Restart with {env_var}=true to allow this tool. \
                     Note: the MCP stdio transport is local-trust; treat any process \
                     that can spawn this server as fully authorized."
                ),
                None,
            ))
        }
    }

    // ---- Tool logic (plain, testable, transport-free) --------------------

    /// `wf_get_schema` logic: return the `Workflow` JSON Schema as JSON.
    ///
    /// # Errors
    /// Returns an internal error only if the (statically valid) schema fails to
    /// serialize, which does not happen in practice.
    pub fn get_schema_logic() -> Result<Value, ErrorData> {
        let schema = a2w_ir::workflow_json_schema();
        serde_json::to_value(schema).map_err(internal)
    }

    /// `wf_describe_nodes` logic: the node taxonomy.
    ///
    /// # Errors
    /// Returns an internal error only on the (practically impossible)
    /// serialization failure of a static array.
    pub fn describe_nodes_logic() -> Result<Value, ErrorData> {
        let infos: Vec<NodeKindInfo> = ALL_NODE_KINDS
            .iter()
            .map(|&kind| {
                let dynamic = kind.has_dynamic_ports();
                NodeKindInfo {
                    name: node_kind_name(kind),
                    output_port_count: if dynamic {
                        None
                    } else {
                        Some(kind.output_port_count())
                    },
                    dynamic_ports: dynamic,
                    is_trigger: kind.is_trigger(),
                }
            })
            .collect();
        serde_json::to_value(infos).map_err(internal)
    }

    /// `wf_validate` logic: parse the workflow and return its report.
    ///
    /// # Errors
    /// [`ErrorData::invalid_params`] when `workflow` is not a valid IR document.
    pub fn validate_logic(&self, input: WorkflowInput) -> Result<Value, ErrorData> {
        let wf = parse_workflow(input.workflow)?;
        let report = a2w_validator::validate(&wf);
        serde_json::to_value(report).map_err(internal)
    }

    /// `wf_dry_run` logic: run the workflow in DryRun (side effects mocked).
    ///
    /// # Errors
    /// [`ErrorData::invalid_params`] on bad workflow JSON or when the engine
    /// rejects the workflow (e.g. validation failure, missing executor, a node
    /// failing under a `Stop` policy). The engine error is surfaced as the tool
    /// error message so the agent can repair the workflow.
    pub async fn dry_run_logic(&self, input: RunInput) -> Result<Value, ErrorData> {
        self.run_in_mode(input, ExecutionMode::DryRun).await
    }

    /// `wf_run` logic: run the workflow for real (HTTP nodes make real calls).
    ///
    /// **Policy-gated:** rejected unless the server was started with
    /// `A2W_MCP_ALLOW_RUN=true`.
    ///
    /// # Errors
    /// [`ErrorData::invalid_params`] when the policy disallows execution; otherwise
    /// as [`A2wServer::dry_run_logic`].
    pub async fn run_logic(&self, input: RunInput) -> Result<Value, ErrorData> {
        self.require_policy(self.policy.allow_run, "A2W_MCP_ALLOW_RUN", "wf_run")?;
        self.run_in_mode(input, ExecutionMode::Run).await
    }

    /// Shared body for `wf_dry_run` / `wf_run`. Persists the run when the
    /// server has a store wired (i.e. `with_vault`/`with_vault_and_policy`).
    async fn run_in_mode(&self, input: RunInput, mode: ExecutionMode) -> Result<Value, ErrorData> {
        let wf = parse_workflow(input.workflow)?;
        let result = self.execute(&wf, input.trigger_input, mode).await?;

        // Best-effort persistence when a store is configured. A persistence
        // failure is logged to stderr but does not poison the tool result —
        // the agent already received the work product and a re-run will save
        // again.
        if let Some(store) = self.store.as_ref() {
            if let Err(e) = store.save_run(&wf.id, &result).await {
                eprintln!("a2w-mcp: save_run failed for run {}: {e}", result.run_id);
            }
        }

        serde_json::to_value(result).map_err(internal)
    }

    /// Run `wf` and map any engine error to an `invalid_params` tool error whose
    /// `data` carries a structured engine error payload.
    async fn execute(
        &self,
        wf: &Workflow,
        trigger_input: Vec<Value>,
        mode: ExecutionMode,
    ) -> Result<RunResult, ErrorData> {
        let log = MemoryEventLog::new();
        self.engine
            .run(wf, trigger_input, mode, &log)
            .await
            .map_err(engine_error)
    }

    /// `wf_run_tests` logic: evaluate declarative test cases via DryRun.
    ///
    /// # Errors
    /// [`ErrorData::invalid_params`] on bad workflow JSON or a malformed test
    /// case.
    pub async fn run_tests_logic(&self, input: RunTestsInput) -> Result<Value, ErrorData> {
        let wf = parse_workflow(input.workflow)?;
        let cases = parse_tests(input.tests)?;
        let results = run_tests(&self.engine, &wf, &cases, ExecutionMode::DryRun).await;
        serde_json::to_value(results).map_err(internal)
    }

    /// `wf_profile` logic: DryRun then profile, returning the [`RunProfile`].
    ///
    /// # Errors
    /// [`ErrorData::invalid_params`] on bad workflow JSON or an engine error.
    pub async fn profile_logic(&self, input: RunInput) -> Result<Value, ErrorData> {
        let wf = parse_workflow(input.workflow)?;
        let result = self
            .execute(&wf, input.trigger_input, ExecutionMode::DryRun)
            .await?;
        let prof = profile(&wf, &result);
        serde_json::to_value(prof).map_err(internal)
    }

    /// `wf_optimize` logic: structural suggestions, optionally profile-informed.
    ///
    /// # Errors
    /// [`ErrorData::invalid_params`] on bad workflow JSON or an engine error
    /// while profiling (only when `with_profile` is set).
    pub async fn optimize_logic(&self, input: OptimizeInput) -> Result<Value, ErrorData> {
        let wf = parse_workflow(input.workflow)?;
        let suggestions = if input.with_profile {
            let result = self
                .execute(&wf, input.trigger_input, ExecutionMode::DryRun)
                .await?;
            let prof = profile(&wf, &result);
            analyze(&wf, Some(&prof))
        } else {
            analyze(&wf, None)
        };
        serde_json::to_value(suggestions).map_err(internal)
    }

    /// `wf_apply_ops` logic: apply IR diff ops, returning the new workflow.
    ///
    /// # Errors
    /// [`ErrorData::invalid_params`] on bad workflow JSON or a malformed op.
    pub fn apply_ops_logic(&self, input: ApplyOpsInput) -> Result<Value, ErrorData> {
        let wf = parse_workflow(input.workflow)?;
        let ops = parse_ops(input.ops)?;
        let new_wf = apply(&wf, &ops);
        serde_json::to_value(new_wf).map_err(internal)
    }

    /// `wf_search_templates` logic: keyword search over the golden corpus.
    ///
    /// Returns an array of `{ id, name, description, tags }` (the workflow body
    /// is intentionally omitted; fetch it with `wf_get_template`).
    ///
    /// # Errors
    /// Internal error only on the (practically impossible) serialization
    /// failure of the result array.
    pub fn search_templates_logic(&self, input: SearchTemplatesInput) -> Result<Value, ErrorData> {
        let hits: Vec<Value> = a2w_templates::search(&input.query)
            .into_iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "name": t.name,
                    "description": t.description,
                    "tags": t.tags,
                })
            })
            .collect();
        serde_json::to_value(hits).map_err(internal)
    }

    /// `wf_get_template` logic: fetch one template's workflow by id.
    ///
    /// # Errors
    /// [`ErrorData::invalid_params`] when no template has the given id.
    pub fn get_template_logic(&self, input: GetTemplateInput) -> Result<Value, ErrorData> {
        let tmpl = a2w_templates::get(&input.id).ok_or_else(|| {
            ErrorData::invalid_params(
                format!(
                    "no template with id '{}'; use wf_search_templates to discover ids",
                    input.id
                ),
                None,
            )
        })?;
        serde_json::to_value(tmpl.workflow).map_err(internal)
    }

    /// `wf_store_credential` logic: encrypt and upsert a secret under `id`.
    ///
    /// # Errors
    /// [`ErrorData::invalid_params`] when the vault is unconfigured, when any
    /// field is empty, or when the underlying store write fails.
    pub async fn store_credential_logic(
        &self,
        input: StoreCredentialInput,
    ) -> Result<Value, ErrorData> {
        self.require_policy(
            self.policy.allow_credential_writes,
            "A2W_MCP_ALLOW_CREDENTIAL_WRITES",
            "wf_store_credential",
        )?;
        let (store, vault) = self.require_vault()?;
        if input.id.trim().is_empty() {
            return Err(ErrorData::invalid_params("`id` must be non-empty", None));
        }
        if input.name.trim().is_empty() {
            return Err(ErrorData::invalid_params("`name` must be non-empty", None));
        }
        if input.secret.is_empty() {
            return Err(ErrorData::invalid_params(
                "`secret` must be non-empty",
                None,
            ));
        }
        vault
            .store_secret(store, &input.id, &input.name, &input.secret)
            .await
            .map_err(|e| {
                ErrorData::internal_error(format!("credential write failed: {e}"), None)
            })?;
        Ok(json!({ "saved": input.id }))
    }

    /// `wf_list_credentials` logic: `[{id, name, created_at}]`. **No secrets.**
    ///
    /// # Errors
    /// [`ErrorData::invalid_params`] when the vault is unconfigured, or
    /// [`ErrorData::internal_error`] on a store read failure.
    pub async fn list_credentials_logic(&self) -> Result<Value, ErrorData> {
        let (store, _vault) = self.require_vault()?;
        let rows = Vault::list_credentials(store)
            .await
            .map_err(|e| ErrorData::internal_error(format!("credential list failed: {e}"), None))?;
        let summaries: Vec<Value> = rows
            .into_iter()
            .map(|(id, name, created_at)| {
                json!({ "id": id, "name": name, "created_at": created_at })
            })
            .collect();
        Ok(Value::Array(summaries))
    }

    /// `wf_delete_credential` logic: delete one credential (idempotent).
    ///
    /// # Errors
    /// As [`A2wServer::store_credential_logic`].
    pub async fn delete_credential_logic(
        &self,
        input: DeleteCredentialInput,
    ) -> Result<Value, ErrorData> {
        self.require_policy(
            self.policy.allow_credential_writes,
            "A2W_MCP_ALLOW_CREDENTIAL_WRITES",
            "wf_delete_credential",
        )?;
        let (store, _vault) = self.require_vault()?;
        Vault::delete_credential(store, &input.id)
            .await
            .map_err(|e| {
                ErrorData::internal_error(format!("credential delete failed: {e}"), None)
            })?;
        Ok(json!({ "deleted": input.id }))
    }

    /// Core `generate_workflow_from_prompt` logic, parameterized over the LLM
    /// client so tests can inject a deterministic mock.
    ///
    /// Runs the author crate's Generate→Validate→Repair loop and returns the
    /// [`AuthorOutcome`](a2w_author::AuthorOutcome) as JSON.
    ///
    /// # Errors
    /// [`ErrorData::internal_error`] if the LLM transport fails, or if the
    /// outcome cannot be serialized. (Parse/validation/dry-run failures are part
    /// of a successful response: they live inside the returned JSON.)
    pub async fn generate_logic(
        &self,
        prompt: &str,
        max_repairs: u32,
        llm: &dyn LlmClient,
    ) -> Result<Value, ErrorData> {
        self.require_policy(
            self.policy.allow_llm,
            "A2W_MCP_ALLOW_LLM",
            "generate_workflow_from_prompt",
        )?;
        let cfg = AuthorConfig { max_repairs };
        let outcome = generate_workflow_from_prompt(prompt, llm, &cfg)
            .await
            .map_err(|e| ErrorData::internal_error(format!("LLM transport failed: {e}"), None))?;
        serde_json::to_value(outcome).map_err(internal)
    }

    // -- F4: verification + skill library over the served surface ----------

    /// `wf_verify` logic: run a verification plan (zero-token, DryRun) and
    /// return the calibrated [`ConfidenceReport`](a2w_verify::ConfidenceReport).
    /// Engine-invariants are reported separately from outcome evidence.
    ///
    /// # Errors
    /// [`ErrorData::invalid_params`] on bad workflow/plan JSON or an absent
    /// observe node.
    pub async fn verify_logic(&self, input: VerifyInput) -> Result<Value, ErrorData> {
        let wf = parse_workflow(input.workflow)?;
        let plan = build_plan(
            &input.observe_node,
            input.spec,
            input.golden,
            input.semantic,
            input.metamorphic,
        )?;
        let harness = VerificationHarness::new();
        let report = verify(&harness, &wf, &plan).await.map_err(verify_error)?;
        // Surface the calibrated headline alongside the structured report so the
        // agent can never mistake engine-verification for outcome-verification.
        let mut value = serde_json::to_value(&report).map_err(internal)?;
        if let Value::Object(map) = &mut value {
            map.insert("summary".to_string(), Value::String(report.summary()));
            map.insert("outcome_score".to_string(), json!(report.score()));
            map.insert(
                "outcome_verified".to_string(),
                json!(report.meets(&a2w_verify::Threshold::default())),
            );
        }
        Ok(value)
    }

    /// `wf_promote_skill` logic: verify the workflow against the supplied
    /// **holdout** plan and persist it as a skill iff the report clears the
    /// threshold. Promotion is gated on the holdout (certification) evidence,
    /// never on "it ran". Requires a configured store.
    ///
    /// # Errors
    /// [`ErrorData::invalid_params`] on bad input, an absent store, or when the
    /// report does not clear the threshold.
    pub async fn promote_skill_logic(&self, input: PromoteSkillInput) -> Result<Value, ErrorData> {
        let store = self.require_store()?;
        let wf = parse_workflow(input.workflow)?;
        // The holdout plan is the verify-shape minus workflow/observe_node.
        let holdout_parts: PlanParts = serde_json::from_value(input.holdout).map_err(|e| {
            ErrorData::invalid_params(format!("`holdout` is not a valid plan: {e}"), None)
        })?;
        let plan = build_plan(
            &input.observe_node,
            holdout_parts.spec,
            holdout_parts.golden,
            holdout_parts.semantic,
            holdout_parts.metamorphic,
        )?;
        let harness = VerificationHarness::new();
        let report = verify(&harness, &wf, &plan).await.map_err(verify_error)?;

        let lib = PersistentSkillLibrary::with_default_threshold(store);
        let id = lib
            .promote(&input.query, wf, &input.observe_node, &report)
            .await
            .map_err(skill_error)?;
        Ok(json!({
            "promoted": id,
            "holdout_score": report.score(),
            "summary": report.summary(),
        }))
    }

    /// `wf_find_skill` logic: retrieve persisted skills by query signature.
    /// Requires a configured store.
    ///
    /// # Errors
    /// [`ErrorData::invalid_params`] when no store is configured;
    /// [`ErrorData::internal_error`] on a storage failure.
    pub async fn find_skill_logic(&self, input: FindSkillInput) -> Result<Value, ErrorData> {
        let store = self.require_store()?;
        let lib = PersistentSkillLibrary::with_default_threshold(store);
        let k = input.k.unwrap_or(5);
        let matches = lib
            .retrieve(&input.query, k)
            .await
            .map_err(|e| ErrorData::internal_error(format!("skill retrieval failed: {e}"), None))?;
        let out: Vec<Value> = matches
            .into_iter()
            .map(|(skill, sim)| {
                json!({
                    "id": skill.id,
                    "query": skill.query,
                    "observe_node": skill.observe_node,
                    "holdout_score": skill.evidence.score,
                    "similarity": sim,
                    "workflow": skill.workflow,
                })
            })
            .collect();
        Ok(json!({ "skills": out }))
    }

    /// Borrow the store, or return an `invalid_params` error naming the fix.
    fn require_store(&self) -> Result<&Arc<Store>, ErrorData> {
        self.store.as_ref().ok_or_else(|| {
            ErrorData::invalid_params(
                "skill tools disabled: server was started without persistence \
                 (set A2W_MASTER_KEY / A2W_DB_URL and restart so a Store is configured)",
                None,
            )
        })
    }
}

/// The plan parts shared by `wf_verify` and the `holdout` of `wf_promote_skill`.
#[derive(Debug, Deserialize)]
struct PlanParts {
    #[serde(default)]
    spec: Option<Value>,
    #[serde(default)]
    golden: Option<Value>,
    #[serde(default)]
    semantic: Option<Value>,
    #[serde(default)]
    metamorphic: Option<Value>,
}

// ---------------------------------------------------------------------------
// Tool surface: thin `#[tool]` wrappers delegating to the logic above.
// ---------------------------------------------------------------------------

#[tool_router]
impl A2wServer {
    /// Return the `Workflow` JSON Schema so an agent can learn the IR it must
    /// emit against. No input.
    #[tool(
        name = "wf_get_schema",
        description = "Return the JSON Schema for the A2W Workflow IR. No input. \
                       The agent emits workflows that validate against this schema."
    )]
    pub async fn wf_get_schema(&self) -> Result<CallToolResult, ErrorData> {
        ok(Self::get_schema_logic()?)
    }

    /// Return the node taxonomy: each kind's snake_case name, output port count
    /// (null for dynamic kinds such as `switch`), and whether it is a trigger.
    #[tool(
        name = "wf_describe_nodes",
        description = "Return the A2W node taxonomy: for each node kind its \
                       snake_case name, output_port_count (null when dynamic, \
                       e.g. switch), dynamic_ports flag, and is_trigger. No input."
    )]
    pub async fn wf_describe_nodes(&self) -> Result<CallToolResult, ErrorData> {
        ok(Self::describe_nodes_logic()?)
    }

    /// Validate a workflow, returning a deterministic `ValidationReport`.
    #[tool(
        name = "wf_validate",
        description = "Validate a workflow IR document. Input { workflow }. \
                       Returns a ValidationReport (findings + is_valid)."
    )]
    pub async fn wf_validate(
        &self,
        Parameters(input): Parameters<WorkflowInput>,
    ) -> Result<CallToolResult, ErrorData> {
        ok(self.validate_logic(input)?)
    }

    /// Dry-run a workflow (side effects mocked), returning a `RunResult`.
    #[tool(
        name = "wf_dry_run",
        description = "Dry-run a workflow: side-effecting nodes (HTTP, MCP) are \
                       mocked. Input { workflow, trigger_input }. Returns a \
                       RunResult (status, node_outputs, events)."
    )]
    pub async fn wf_dry_run(
        &self,
        Parameters(input): Parameters<RunInput>,
    ) -> Result<CallToolResult, ErrorData> {
        ok(self.dry_run_logic(input).await?)
    }

    /// Run a workflow for real, returning a `RunResult`.
    #[tool(
        name = "wf_run",
        description = "Run a workflow for real: HTTP nodes make real calls; \
                       mcp_tool_call returns NotImplemented for now. Input \
                       { workflow, trigger_input }. Returns a RunResult."
    )]
    pub async fn wf_run(
        &self,
        Parameters(input): Parameters<RunInput>,
    ) -> Result<CallToolResult, ErrorData> {
        ok(self.run_logic(input).await?)
    }

    /// Run declarative tests against a workflow (DryRun), returning results.
    #[tool(
        name = "wf_run_tests",
        description = "Evaluate declarative test cases against a workflow (run \
                       in DryRun). Input { workflow, tests: [TestCase] }. \
                       Returns one TestResult per case."
    )]
    pub async fn wf_run_tests(
        &self,
        Parameters(input): Parameters<RunTestsInput>,
    ) -> Result<CallToolResult, ErrorData> {
        ok(self.run_tests_logic(input).await?)
    }

    /// Profile a workflow (DryRun then profile), returning a `RunProfile`.
    #[tool(
        name = "wf_profile",
        description = "DryRun a workflow then profile it. Input { workflow, \
                       trigger_input }. Returns a RunProfile (per-step latency, \
                       critical path, flagged inefficiencies)."
    )]
    pub async fn wf_profile(
        &self,
        Parameters(input): Parameters<RunInput>,
    ) -> Result<CallToolResult, ErrorData> {
        ok(self.profile_logic(input).await?)
    }

    /// Analyze a workflow for optimization suggestions.
    #[tool(
        name = "wf_optimize",
        description = "Analyze a workflow and return optimization Suggestions \
                       (e.g. Parallelize, RemoveDeadNode) as IR diff ops. Input \
                       { workflow, with_profile?, trigger_input? }. With \
                       with_profile=true, DryRun+profile first to fill \
                       estimated_gain_ms and surface dead nodes."
    )]
    pub async fn wf_optimize(
        &self,
        Parameters(input): Parameters<OptimizeInput>,
    ) -> Result<CallToolResult, ErrorData> {
        ok(self.optimize_logic(input).await?)
    }

    /// Apply IR diff ops to a workflow, returning the new workflow.
    #[tool(
        name = "wf_apply_ops",
        description = "Apply IR diff ops (e.g. from wf_optimize) to a workflow. \
                       Input { workflow, ops: [IrOp] }. Returns the resulting \
                       Workflow."
    )]
    pub async fn wf_apply_ops(
        &self,
        Parameters(input): Parameters<ApplyOpsInput>,
    ) -> Result<CallToolResult, ErrorData> {
        ok(self.apply_ops_logic(input)?)
    }

    /// Search the golden template corpus by keyword.
    #[tool(
        name = "wf_search_templates",
        description = "Search A2W's golden workflow templates by keyword. Input \
                       { query }. Matches case-insensitively over each template's \
                       name, description, and tags. Returns an array of \
                       { id, name, description, tags }; fetch a full workflow with \
                       wf_get_template."
    )]
    pub async fn wf_search_templates(
        &self,
        Parameters(input): Parameters<SearchTemplatesInput>,
    ) -> Result<CallToolResult, ErrorData> {
        ok(self.search_templates_logic(input)?)
    }

    /// Fetch a single template's workflow by id.
    #[tool(
        name = "wf_get_template",
        description = "Fetch one golden template's workflow IR by id. Input \
                       { id }. Returns the Workflow document, or an \
                       invalid_params error if no template has that id."
    )]
    pub async fn wf_get_template(
        &self,
        Parameters(input): Parameters<GetTemplateInput>,
    ) -> Result<CallToolResult, ErrorData> {
        ok(self.get_template_logic(input)?)
    }

    /// Upsert a credential under its id (encrypted under the master key).
    #[tool(
        name = "wf_store_credential",
        description = "Upsert a credential into the AES-256-GCM vault. Input \
                       { id, name, secret }. Returns { saved: id }. The plaintext \
                       is never returned by any later tool call. Requires the server \
                       to be started with A2W_MASTER_KEY (base64 32 bytes); without \
                       it the call returns an invalid_params error."
    )]
    pub async fn wf_store_credential(
        &self,
        Parameters(input): Parameters<StoreCredentialInput>,
    ) -> Result<CallToolResult, ErrorData> {
        ok(self.store_credential_logic(input).await?)
    }

    /// List stored credentials as `{id, name, created_at}` — no secrets.
    #[tool(
        name = "wf_list_credentials",
        description = "List stored credentials as { id, name, created_at } objects. \
                       The plaintext secret is NEVER returned. No input. Requires \
                       A2W_MASTER_KEY on the server."
    )]
    pub async fn wf_list_credentials(&self) -> Result<CallToolResult, ErrorData> {
        ok(self.list_credentials_logic().await?)
    }

    /// Delete a credential by id (idempotent).
    #[tool(
        name = "wf_delete_credential",
        description = "Delete a stored credential by id (no-op when absent). Input \
                       { id }. Returns { deleted: id }. Requires A2W_MASTER_KEY on \
                       the server."
    )]
    pub async fn wf_delete_credential(
        &self,
        Parameters(input): Parameters<DeleteCredentialInput>,
    ) -> Result<CallToolResult, ErrorData> {
        ok(self.delete_credential_logic(input).await?)
    }

    /// Verify a workflow: run a verification plan and return a calibrated
    /// confidence report (engine-invariants reported separately from outcome).
    #[tool(
        name = "wf_verify",
        description = "Verify a workflow and return a calibrated ConfidenceReport. \
                       Input { workflow, observe_node, spec?, golden?, semantic?, \
                       metamorphic? }. Runs zero-token (DryRun). The report \
                       separates ENGINE-INVARIANTS (rerun/permutation/scaling/ \
                       additivity — they verify the engine, NOT the outcome) from \
                       OUTCOME evidence (spec assertions, golden fixtures, \
                       spec-derived semantic relations). score()/meets() use \
                       outcome evidence only."
    )]
    pub async fn wf_verify(
        &self,
        Parameters(input): Parameters<VerifyInput>,
    ) -> Result<CallToolResult, ErrorData> {
        ok(self.verify_logic(input).await?)
    }

    /// Promote a verified workflow into the durable skill library.
    #[tool(
        name = "wf_promote_skill",
        description = "Verify a workflow against the supplied HOLDOUT plan and \
                       persist it as a reusable skill IFF the report clears the \
                       threshold. Input { query, workflow, observe_node, holdout: \
                       { spec?, golden?, semantic?, metamorphic? } }. Promotion is \
                       gated on the holdout (certification) evidence, never on 'it \
                       ran'. Requires the server to have a Store. Returns \
                       { promoted: id, holdout_score, summary }."
    )]
    pub async fn wf_promote_skill(
        &self,
        Parameters(input): Parameters<PromoteSkillInput>,
    ) -> Result<CallToolResult, ErrorData> {
        ok(self.promote_skill_logic(input).await?)
    }

    /// Retrieve persisted skills by query signature.
    #[tool(
        name = "wf_find_skill",
        description = "Retrieve persisted skills most similar to a query. Input \
                       { query, k? }. Returns { skills: [{ id, query, \
                       observe_node, holdout_score, similarity, workflow }] }, \
                       ranked by task-signature similarity. Requires a Store."
    )]
    pub async fn wf_find_skill(
        &self,
        Parameters(input): Parameters<FindSkillInput>,
    ) -> Result<CallToolResult, ErrorData> {
        ok(self.find_skill_logic(input).await?)
    }

    /// Author a workflow from a plain-English prompt (Generate→Validate→Repair).
    #[tool(
        name = "generate_workflow_from_prompt",
        description = "Author a complete A2W workflow from a plain-English \
                       prompt via a Generate->Validate->Repair loop. Input \
                       { prompt, max_repairs? }. Requires the ANTHROPIC_API_KEY \
                       environment variable (A2W_LLM_MODEL is optional). Returns \
                       an AuthorOutcome { success, workflow, iterations, message }."
    )]
    pub async fn generate_workflow_from_prompt(
        &self,
        Parameters(input): Parameters<GenerateInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // Build the real Anthropic client from the environment. A missing key is
        // a clean, actionable invalid_params error rather than a crash.
        let client = AnthropicClient::from_env().map_err(|e| {
            ErrorData::invalid_params(
                format!(
                    "cannot author a workflow: {e} Set ANTHROPIC_API_KEY (and \
                     optionally A2W_LLM_MODEL / A2W_LLM_BASE_URL) and retry."
                ),
                None,
            )
        })?;
        let max_repairs = input.max_repairs.unwrap_or(3);
        ok(self
            .generate_logic(&input.prompt, max_repairs, &client)
            .await?)
    }
}

#[tool_handler]
impl ServerHandler for A2wServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "A2W (Agent-to-Workflow) control surface. Author a workflow IR \
                 (see wf_get_schema and wf_describe_nodes), then wf_validate it, \
                 wf_dry_run / wf_run it, wf_run_tests against it, wf_profile and \
                 wf_optimize it, and wf_apply_ops to apply suggested IR diffs. \
                 All tools take/return JSON; invalid workflow JSON yields a clean \
                 tool error, never a crash."
                    .to_string(),
            )
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Wrap a successful logic result as a structured `CallToolResult`.
fn ok(value: Value) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::structured(value))
}

/// Parse an untrusted JSON value into a [`Workflow`], mapping any failure to a
/// clean `invalid_params` tool error.
fn parse_workflow(value: Value) -> Result<Workflow, ErrorData> {
    serde_json::from_value(value).map_err(|e| {
        ErrorData::invalid_params(
            format!("`workflow` is not a valid A2W workflow IR document: {e}"),
            None,
        )
    })
}

/// Parse a list of untrusted JSON values into [`TestCase`]s.
fn parse_tests(values: Vec<Value>) -> Result<Vec<TestCase>, ErrorData> {
    values
        .into_iter()
        .enumerate()
        .map(|(i, v)| {
            serde_json::from_value(v).map_err(|e| {
                ErrorData::invalid_params(
                    format!("`tests[{i}]` is not a valid TestCase: {e}"),
                    None,
                )
            })
        })
        .collect()
}

/// Parse a list of untrusted JSON values into [`IrOp`]s.
fn parse_ops(values: Vec<Value>) -> Result<Vec<IrOp>, ErrorData> {
    values
        .into_iter()
        .enumerate()
        .map(|(i, v)| {
            serde_json::from_value(v).map_err(|e| {
                ErrorData::invalid_params(format!("`ops[{i}]` is not a valid IrOp: {e}"), None)
            })
        })
        .collect()
}

/// Map an engine error to an `invalid_params` tool error.
///
/// The engine validates before executing, so most engine errors are caused by a
/// workflow the agent can repair; `invalid_params` is the right MCP class. The
/// structured `ValidationReport` (when present) is attached as the error `data`
/// so the agent gets located, fix-suggesting findings.
fn engine_error(err: a2w_engine::EngineError) -> ErrorData {
    let data = match &err {
        a2w_engine::EngineError::Invalid(report) => serde_json::to_value(report).ok(),
        _ => None,
    };
    ErrorData::invalid_params(format!("workflow run failed: {err}"), data)
}

/// Map a serialization failure (practically impossible for our types) to an
/// internal tool error.
fn internal(err: serde_json::Error) -> ErrorData {
    ErrorData::internal_error(format!("failed to serialize tool output: {err}"), None)
}

/// Build a typed [`VerificationPlan`] from untyped JSON plan parts, mapping any
/// parse failure to a clean `invalid_params` tool error.
fn build_plan(
    observe_node: &str,
    spec: Option<Value>,
    golden: Option<Value>,
    semantic: Option<Value>,
    metamorphic: Option<Value>,
) -> Result<VerificationPlan, ErrorData> {
    let mut plan = VerificationPlan::new(observe_node);
    if let Some(spec) = spec {
        let spec: WorkflowSpec = serde_json::from_value(spec)
            .map_err(|e| ErrorData::invalid_params(format!("`spec` is invalid: {e}"), None))?;
        plan = plan.with_spec(spec);
    }
    if let Some(golden) = golden {
        let golden: Vec<GoldenFixture> = serde_json::from_value(golden)
            .map_err(|e| ErrorData::invalid_params(format!("`golden` is invalid: {e}"), None))?;
        plan = plan.with_golden(golden);
    }
    if let Some(semantic) = semantic {
        let relations: Vec<SemanticRelation> = serde_json::from_value(semantic)
            .map_err(|e| ErrorData::invalid_params(format!("`semantic` is invalid: {e}"), None))?;
        plan = plan.with_semantic(SemanticSuite::new(relations));
    }
    if let Some(metamorphic) = metamorphic {
        let suite: MetamorphicSuite = serde_json::from_value(metamorphic).map_err(|e| {
            ErrorData::invalid_params(format!("`metamorphic` is invalid: {e}"), None)
        })?;
        plan = plan.with_metamorphic(suite);
    }
    Ok(plan)
}

/// Map a verification error to an `invalid_params` tool error (an absent observe
/// node or an unrunnable workflow is something the agent can fix).
fn verify_error(err: a2w_verify::VerifyError) -> ErrorData {
    ErrorData::invalid_params(format!("verification failed: {err}"), None)
}

/// Map a skill-promotion error to a tool error, attaching the calibrated
/// summary as `data` when the report fell short of the threshold.
fn skill_error(err: a2w_skills::PersistError) -> ErrorData {
    match &err {
        a2w_skills::PersistError::Skill(a2w_skills::SkillError::BelowThreshold {
            summary, ..
        }) => ErrorData::invalid_params(
            format!("not promoted: {err}"),
            Some(json!({ "summary": summary })),
        ),
        a2w_skills::PersistError::Store(_) => {
            ErrorData::internal_error(format!("skill persistence failed: {err}"), None)
        }
        _ => ErrorData::invalid_params(format!("not promoted: {err}"), None),
    }
}
