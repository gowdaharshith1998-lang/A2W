//! [`NodeKind::LlmCall`] executor — call a large language model.
//!
//! Params:
//! ```json
//! { "prompt": "<text, supports {{json.path}} templating>",
//!   "system"?: "<system prompt>" }
//! ```
//!
//! Model + max_tokens are configured at process startup via `A2W_LLM_MODEL`
//! (default `claude-opus-4-8`) and the [`a2w_llm::AnthropicClient`]'s defaults.
//!
//! For each input item the executor renders `prompt` against the item (via
//! the same `{{json.path}}` templating used by HttpRequest), calls the LLM,
//! and emits one output item shaped `{ "text": "<reply>" }`.
//!
//! `dry_run` emits a mock per input item without contacting the provider.
//!
//! ## Cost gate
//! Without `ANTHROPIC_API_KEY` (and a wired `LlmCall::default()`), a real run
//! fails closed with a clear error. Inject a [`MockLlm`](a2w_llm::MockLlm) for
//! tests.
//!
//! ## Prompt-injection warning (R3 audit-fix)
//! The `prompt` param is templated against the input item via
//! [`a2w_expr::render`] (and the legacy `{{json.path}}` substitution). That
//! means a workflow author can plug an untrusted upstream item's text directly
//! into the LLM prompt — making LlmCall a classic prompt-injection sink. Item
//! data like `"ignore previous instructions and ..."` will be sent verbatim
//! to the model. **Always sanitize / quote / contextualise** untrusted item
//! text before feeding it into a `prompt`; do NOT rely on the system prompt
//! to defend against tampered user-data.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use a2w_engine::{Item, NodeContext, NodeError, NodeExecutor};
use a2w_llm::{AnthropicClient, LlmClient};

use crate::template;

/// Executor for [`a2w_ir::NodeKind::LlmCall`](a2w_ir::NodeKind::LlmCall).
#[derive(Clone, Default)]
pub struct LlmCall {
    /// Pluggable LLM client. `None` means "build an AnthropicClient from env
    /// lazily per-call" — the default in production.
    client: Option<Arc<dyn LlmClient>>,
}

impl std::fmt::Debug for LlmCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmCall").finish_non_exhaustive()
    }
}

impl LlmCall {
    /// Construct with an explicit LLM client (tests).
    #[must_use]
    pub fn new(client: Arc<dyn LlmClient>) -> Self {
        Self {
            client: Some(client),
        }
    }
}

/// Process-cached env-built [`AnthropicClient`]. Initialised once on first
/// use so the underlying reqwest pool is reused across every `LlmCall` node
/// in every workflow run. (R3 audit-fix.)
fn env_client() -> Result<Arc<dyn LlmClient>, NodeError> {
    use std::sync::OnceLock;
    static CACHED: OnceLock<Arc<dyn LlmClient>> = OnceLock::new();
    if let Some(c) = CACHED.get() {
        return Ok(c.clone());
    }
    let built = AnthropicClient::from_env().map_err(|e| {
        NodeError::Runtime(format!(
            "LlmCall could not build a client from env (set ANTHROPIC_API_KEY): {e}"
        ))
    })?;
    let arc: Arc<dyn LlmClient> = Arc::new(built);
    // First writer wins; everyone gets the same instance.
    let _ = CACHED.set(arc.clone());
    Ok(CACHED.get().cloned().unwrap_or(arc))
}

#[derive(Debug, Deserialize)]
struct LlmCallSpec {
    prompt: String,
    #[serde(default)]
    system: Option<String>,
}

impl LlmCallSpec {
    fn parse(params: &serde_json::Value) -> Result<Self, NodeError> {
        serde_json::from_value(params.clone())
            .map_err(|e| NodeError::BadParams(format!("LlmCall params invalid: {e}")))
    }
}

#[async_trait]
impl NodeExecutor for LlmCall {
    fn has_side_effects(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        if input.is_empty() {
            // Audit-2: unselected branch arms must not call the LLM.
            return Ok(Vec::new());
        }
        let spec = LlmCallSpec::parse(&ctx.params)?;
        let system = spec.system.as_deref().unwrap_or("");

        // Resolve the client: explicit override > cached env-built
        // AnthropicClient. R3 audit-fix: cache the env client in a OnceLock
        // so reqwest's connection pool is reused across calls (previously
        // every execute() rebuilt the client and threw away the pool).
        let client: Arc<dyn LlmClient> = match self.client.clone() {
            Some(c) => c,
            None => env_client()?,
        };

        let mut out = Vec::with_capacity(input.len());
        for item in &input {
            let prompt = template::render(&spec.prompt, &item.json);
            let completion = client
                .complete_with_usage(system, &prompt)
                .await
                .map_err(|e| NodeError::Runtime(format!("LlmCall transport failed: {e}")))?;
            // Report the real outbound call and the tokens it consumed so the
            // engine surfaces them in this node's step event.
            ctx.record_external_call();
            ctx.record_tokens(completion.usage.total());
            out.push(Item::produced(
                serde_json::json!({
                    "text": completion.text,
                    "input_tokens": completion.usage.input_tokens,
                    "output_tokens": completion.usage.output_tokens,
                }),
                ctx.node_id.clone(),
                0,
            ));
        }
        Ok(out)
    }

    async fn dry_run(&self, ctx: &NodeContext, input: Vec<Item>) -> Result<Vec<Item>, NodeError> {
        // No LLM call; validate the params and emit one mock per input.
        let _ = LlmCallSpec::parse(&ctx.params)?;
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(input.len());
        for _ in &input {
            out.push(Item::produced(
                serde_json::json!({
                    "_mock": true,
                    "text": "[dry-run] LLM response would go here",
                }),
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
    use a2w_llm::MockLlm;

    fn ctx(params: serde_json::Value, mode: ExecutionMode) -> NodeContext {
        NodeContext {
            run_id: "r".into(),
            node_id: "llm".into(),
            kind: NodeKind::LlmCall,
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

    #[tokio::test]
    async fn execute_with_mock_emits_one_item_per_input() {
        let mock = Arc::new(MockLlm::new(vec!["hi".to_string(), "ho".to_string()]));
        let node = LlmCall::new(mock);
        let c = ctx(
            serde_json::json!({ "prompt": "echo {{json}}" }),
            ExecutionMode::Run,
        );
        let input = vec![
            Item::root(serde_json::json!({ "id": 1 })),
            Item::root(serde_json::json!({ "id": 2 })),
        ];
        let out = node.execute(&c, input).await.expect("ok");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].json["text"], "hi");
        assert_eq!(out[1].json["text"], "ho");
    }

    #[tokio::test]
    async fn execute_with_no_input_emits_nothing() {
        let node = LlmCall::default();
        let c = ctx(serde_json::json!({ "prompt": "x" }), ExecutionMode::Run);
        let out = node.execute(&c, Vec::new()).await.expect("ok");
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn dry_run_emits_mock_without_calling_llm() {
        let node = LlmCall::default();
        let c = ctx(
            serde_json::json!({ "prompt": "expensive" }),
            ExecutionMode::DryRun,
        );
        let out = node
            .dry_run(&c, vec![Item::root(serde_json::json!({}))])
            .await
            .expect("ok");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].json["_mock"], true);
    }
}
