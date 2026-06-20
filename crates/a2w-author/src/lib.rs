//! # a2w-author
//!
//! Text-to-workflow authoring: turn a plain-English prompt into a complete,
//! valid, dry-runnable [`a2w_ir::Workflow`] via a **Generate → Validate →
//! Repair** loop.
//!
//! [`generate_workflow_from_prompt`] drives an [`LlmClient`](a2w_llm::LlmClient):
//! it builds a focused system prompt (authoring rules + node taxonomy + one or
//! two few-shot examples drawn from [`a2w_templates`]), asks the model for a
//! single JSON workflow, then **parses → validates → dry-runs** it. Any failure
//! is fed back to the model as a concrete repair instruction and the loop tries
//! again, up to [`AuthorConfig::max_repairs`] times.
//!
//! The function only returns `Err` on an LLM **transport** failure. Parse,
//! validation, and dry-run failures are *not* errors of this function — they are
//! recorded as [`AuthorIteration`]s and reflected in [`AuthorOutcome::success`],
//! because they are exactly the signal the repair loop (and the caller) needs.
//!
//! Everything is deterministic given a deterministic client, so tests use
//! [`a2w_llm::MockLlm`] and never touch the network.

#![forbid(unsafe_code)]

use a2w_engine::{Engine, ExecutionMode, MemoryEventLog};
use a2w_ir::{NodeKind, Workflow, SCHEMA_VERSION};
use a2w_llm::{LlmClient, LlmError};
use a2w_validator::ValidationReport;
use serde::Serialize;
use serde_json::json;

/// Configuration for the authoring loop.
#[derive(Debug, Clone)]
pub struct AuthorConfig {
    /// Maximum number of **repair** attempts after the initial generation.
    ///
    /// The loop therefore makes at most `max_repairs + 1` LLM calls.
    pub max_repairs: u32,
}

impl Default for AuthorConfig {
    fn default() -> Self {
        Self { max_repairs: 3 }
    }
}

/// A record of one attempt through the Generate→Validate→Repair loop.
///
/// Exactly one of the failure fields is typically populated for a failed
/// attempt; a successful attempt has all failure fields `None`.
#[derive(Debug, Serialize)]
pub struct AuthorIteration {
    /// Zero-based attempt index (0 = initial generation).
    pub attempt: u32,
    /// The raw assistant text returned by the LLM for this attempt.
    pub raw_output: String,
    /// Set when the raw output could not be parsed into a [`Workflow`].
    pub parse_error: Option<String>,
    /// Set when the parsed workflow failed validation (carries the full report).
    pub validation: Option<ValidationReport>,
    /// Set when the validated workflow failed the dry run.
    pub dry_run_error: Option<String>,
}

/// The result of an authoring run.
#[derive(Debug, Serialize)]
pub struct AuthorOutcome {
    /// Whether a valid, dry-runnable workflow was produced.
    pub success: bool,
    /// The produced workflow on success; the last successfully-*parsed* workflow
    /// on failure (so the caller can inspect the closest attempt), or `None` if
    /// nothing ever parsed.
    pub workflow: Option<Workflow>,
    /// Every attempt, in order.
    pub iterations: Vec<AuthorIteration>,
    /// Human-readable summary of the outcome.
    pub message: String,
}

/// Generate a workflow from a plain-English `prompt`, repairing up to
/// `cfg.max_repairs` times.
///
/// # Errors
/// Returns [`LlmError`] **only** if the underlying LLM call fails at the
/// transport level. Parse/validation/dry-run failures are captured in the
/// returned [`AuthorOutcome`] (with `success == false`) rather than surfaced as
/// errors.
pub async fn generate_workflow_from_prompt(
    prompt: &str,
    llm: &dyn LlmClient,
    cfg: &AuthorConfig,
) -> Result<AuthorOutcome, LlmError> {
    let system = build_system_prompt();

    let mut iterations: Vec<AuthorIteration> = Vec::new();
    let mut last_parsed: Option<Workflow> = None;
    // The message handed to the model this attempt. Starts as the user's prompt;
    // repair attempts replace it with a failure-specific instruction.
    let mut message = prompt.to_string();

    let total_attempts = cfg.max_repairs + 1;
    for attempt in 0..total_attempts {
        // --- Generate -----------------------------------------------------
        let raw = llm.complete(&system, &message).await?;

        // --- Parse --------------------------------------------------------
        let candidate = extract_json(&raw);
        let parsed = match serde_json::from_str::<Workflow>(&candidate) {
            Ok(wf) => wf,
            Err(e) => {
                let parse_error = format!("{e}");
                iterations.push(AuthorIteration {
                    attempt,
                    raw_output: raw,
                    parse_error: Some(parse_error.clone()),
                    validation: None,
                    dry_run_error: None,
                });
                message = repair_message_parse(&parse_error);
                continue;
            }
        };
        last_parsed = Some(parsed.clone());

        // --- Validate -----------------------------------------------------
        let report = a2w_validator::validate(&parsed);
        if !report.is_valid {
            let repair = repair_message_validation(&report);
            iterations.push(AuthorIteration {
                attempt,
                raw_output: raw,
                parse_error: None,
                validation: Some(report),
                dry_run_error: None,
            });
            message = repair;
            continue;
        }

        // --- Dry-run ------------------------------------------------------
        // DryRun mocks all side effects (no network). A failure here means the
        // workflow is structurally valid but not executable (e.g. a kind with no
        // executor, or bad params surfaced at run time).
        let engine = Engine::new(a2w_nodes::default_registry());
        let log = MemoryEventLog::new();
        match engine
            .run(&parsed, vec![json!({})], ExecutionMode::DryRun, &log)
            .await
        {
            Ok(_) => {
                iterations.push(AuthorIteration {
                    attempt,
                    raw_output: raw,
                    parse_error: None,
                    validation: None,
                    dry_run_error: None,
                });
                return Ok(AuthorOutcome {
                    success: true,
                    workflow: Some(parsed),
                    iterations,
                    message: format!(
                        "produced a valid, dry-runnable workflow on attempt {}",
                        attempt + 1
                    ),
                });
            }
            Err(e) => {
                let dry_run_error = format!("{e}");
                iterations.push(AuthorIteration {
                    attempt,
                    raw_output: raw,
                    parse_error: None,
                    validation: None,
                    dry_run_error: Some(dry_run_error.clone()),
                });
                message = repair_message_dry_run(&dry_run_error);
                continue;
            }
        }
    }

    Ok(AuthorOutcome {
        success: false,
        workflow: last_parsed,
        iterations,
        message: format!("could not produce a valid workflow in {total_attempts} attempts"),
    })
}

/// Extract a JSON object substring from raw LLM output.
///
/// Robust to three common wrappings:
/// 1. A fenced code block: <code>```json ... ```</code> or <code>``` ... ```</code>.
/// 2. Surrounding prose before/after the JSON.
/// 3. Plain JSON with only whitespace around it.
///
/// Strategy: strip a leading/trailing code fence if present, then take the
/// substring from the first `{` to the **last** `}` (inclusive). If no braces
/// are found, the trimmed input is returned unchanged so the parse step produces
/// a meaningful error.
fn extract_json(raw: &str) -> String {
    let trimmed = raw.trim();

    // Strip a surrounding fenced code block if the whole thing is fenced.
    let unfenced = strip_code_fence(trimmed);

    // Take from the first '{' to the last '}', which discards any leading or
    // trailing prose the model added around the object.
    match (unfenced.find('{'), unfenced.rfind('}')) {
        (Some(start), Some(end)) if end >= start => unfenced[start..=end].to_string(),
        _ => unfenced.to_string(),
    }
}

/// If `s` is wrapped in a Markdown code fence, return its inner content;
/// otherwise return `s` unchanged.
///
/// Handles an opening fence of <code>```</code> optionally followed by a language
/// tag (e.g. `json`) on the same line, and a closing <code>```</code> on its own
/// line or at the very end.
fn strip_code_fence(s: &str) -> &str {
    let s = s.trim();
    if !s.starts_with("```") {
        return s;
    }

    // Drop the opening fence line (```), including any language hint after it.
    let after_open = match s.find('\n') {
        Some(nl) => &s[nl + 1..],
        // A single ```...``` on one line: strip the leading fence and let the
        // closing-fence trim below handle the rest.
        None => s.trim_start_matches('`'),
    };

    // Drop the trailing closing fence if present.
    let body = match after_open.rfind("```") {
        Some(idx) => &after_open[..idx],
        None => after_open,
    };

    body.trim()
}

/// Build the system prompt: authoring rules + node taxonomy + few-shot examples.
fn build_system_prompt() -> String {
    let mut s = String::new();

    s.push_str(
        "You are an A2W (Agent-to-Workflow) authoring assistant. Convert the \
         user's request into ONE A2W Workflow expressed as a single JSON object.\n\n",
    );

    // (a) Authoring rules.
    s.push_str("# Output rules\n");
    s.push_str(
        "- Emit ONLY a single JSON object that is an A2W Workflow. No prose, no \
         explanation, no Markdown code fences.\n",
    );
    s.push_str(&format!("- Set \"schema_version\" to {SCHEMA_VERSION}.\n"));
    s.push_str("- Give the workflow a short stable \"id\" and a human \"name\".\n");
    s.push_str(
        "- Every node has a unique stable \"id\", a \"kind\" (from the taxonomy \
         below), and a \"params\" object.\n",
    );
    s.push_str(
        "- The workflow MUST have EXACTLY ONE trigger node (webhook_trigger or \
         schedule_trigger). To run independent steps in parallel, fan out from \
         the single trigger to multiple nodes; do NOT add a second trigger.\n",
    );
    s.push_str(
        "- Connections form a directed acyclic graph and reference node ids: \
         each connection is { \"from_node\": <id>, \"from_port\": <0-based int>, \
         \"to_node\": <id> }.\n",
    );
    s.push_str(
        "- from_port is 0 for every kind except branch, whose port 0 = true and \
         port 1 = false. Most kinds have a single output port (index 0).\n",
    );
    s.push_str(
        "- Every non-trigger node must be reachable from the trigger via \
         connections.\n\n",
    );

    // (b) Node taxonomy.
    s.push_str("# Node kinds\n");
    for &kind in ALL_KINDS {
        let ports = if kind.has_dynamic_ports() {
            "dynamic".to_string()
        } else {
            kind.output_port_count().to_string()
        };
        s.push_str(&format!(
            "- {} (output_ports: {}, trigger: {})\n",
            kind_name(kind),
            ports,
            kind.is_trigger()
        ));
    }
    s.push('\n');

    // Param shapes for the most common executable kinds.
    s.push_str("# Param shapes\n");
    s.push_str(
        "- http_request params: { \"method\": \"GET|POST|...\", \"url\": \
         \"https://...\", \"headers\"?: { string: string }, \"json\"?: <body> }. \
         Strings may template input fields with {{json.FIELD}} or {{json}}.\n",
    );
    s.push_str(
        "- transform params: { \"set\": { field: value, ... } } merges the given \
         fields onto each input item.\n",
    );
    s.push_str(
        "- webhook_trigger / schedule_trigger params: an object (schedule_trigger \
         typically { \"cron\": \"*/5 * * * *\" }).\n\n",
    );

    // (c) One or two few-shot examples, serialized from the golden corpus.
    s.push_str("# Examples\n");
    s.push_str(
        "Each example below is a complete, valid A2W Workflow. Imitate this \
         shape exactly.\n\n",
    );
    for tmpl in few_shot_examples() {
        if let Ok(pretty) = serde_json::to_string_pretty(&tmpl.workflow) {
            s.push_str(&format!("Example — {}:\n{}\n\n", tmpl.description, pretty));
        }
    }

    s.push_str(
        "Now produce the workflow for the user's request as a single JSON object \
         only.",
    );

    s
}

/// The few-shot examples included in the system prompt.
///
/// Drawn from [`a2w_templates`] and limited to one or two to keep the prompt
/// focused. Both chosen examples use only kinds the engine can dry-run, so they
/// model a runnable shape.
fn few_shot_examples() -> Vec<a2w_templates::Template> {
    let mut out = Vec::new();
    if let Some(t) = a2w_templates::get("webhook_to_slack") {
        out.push(t);
    }
    if let Some(t) = a2w_templates::get("scheduled_fetch_transform") {
        out.push(t);
    }
    out
}

/// Build a repair message after a parse failure.
fn repair_message_parse(parse_error: &str) -> String {
    format!(
        "The previous output could not be parsed as an A2W Workflow JSON object. \
         Parser error:\n{parse_error}\n\nReturn the corrected workflow as JSON \
         only — a single JSON object, no prose, no code fences."
    )
}

/// Build a repair message after a validation failure, embedding the located
/// findings compactly.
fn repair_message_validation(report: &ValidationReport) -> String {
    let findings = serde_json::to_string(&report.findings)
        .unwrap_or_else(|_| "<unserializable findings>".to_string());
    format!(
        "The previous workflow was invalid. Fix every Error-severity finding \
         below (each names the offending node/connection and suggests a fix):\n\
         {findings}\n\nReturn the corrected workflow as JSON only — a single \
         JSON object, no prose, no code fences."
    )
}

/// Build a repair message after a dry-run failure.
fn repair_message_dry_run(dry_run_error: &str) -> String {
    format!(
        "The previous workflow passed validation but failed a dry run. Engine \
         error:\n{dry_run_error}\n\nLikely causes: a node kind the engine cannot \
         execute, or malformed node params. Prefer the executable kinds \
         (webhook_trigger, schedule_trigger, http_request, transform) and check \
         each node's params. Return the corrected workflow as JSON only — a \
         single JSON object, no prose, no code fences."
    )
}

/// Every node kind, in taxonomy order, for the system-prompt listing.
const ALL_KINDS: &[NodeKind] = &[
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

/// snake_case wire name for a [`NodeKind`].
fn kind_name(kind: NodeKind) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;
    use a2w_llm::MockLlm;

    /// A valid, dry-runnable workflow: webhook_trigger -> transform.
    fn valid_workflow_json() -> String {
        json!({
            "schema_version": 1,
            "id": "wf_ok",
            "name": "ok",
            "nodes": [
                { "id": "trigger", "kind": "webhook_trigger", "params": {} },
                { "id": "shape", "kind": "transform", "params": { "set": { "ok": true } } }
            ],
            "connections": [
                { "from_node": "trigger", "from_port": 0, "to_node": "shape" }
            ]
        })
        .to_string()
    }

    /// An INVALID workflow: a connection targets a node that does not exist.
    fn invalid_workflow_json() -> String {
        json!({
            "schema_version": 1,
            "id": "wf_bad",
            "name": "bad",
            "nodes": [
                { "id": "trigger", "kind": "webhook_trigger", "params": {} }
            ],
            "connections": [
                { "from_node": "trigger", "from_port": 0, "to_node": "ghost" }
            ]
        })
        .to_string()
    }

    #[tokio::test]
    async fn repairs_invalid_then_succeeds() {
        // First response invalid (dangling target), second valid.
        let mock = MockLlm::new(vec![invalid_workflow_json(), valid_workflow_json()]);
        let outcome =
            generate_workflow_from_prompt("notify on webhook", &mock, &AuthorConfig::default())
                .await
                .expect("no transport error");

        assert!(
            outcome.success,
            "should succeed after one repair: {outcome:?}"
        );
        assert_eq!(outcome.iterations.len(), 2, "one failed + one success");
        // First iteration recorded a validation failure.
        assert!(outcome.iterations[0].validation.is_some());
        assert!(!outcome.iterations[0].validation.as_ref().unwrap().is_valid);
        // Final workflow is present and validates.
        let wf = outcome.workflow.expect("final workflow");
        assert!(a2w_validator::validate(&wf).is_valid);
    }

    #[tokio::test]
    async fn extracts_json_from_code_fence() {
        let fenced = format!("```json\n{}\n```", valid_workflow_json());
        let mock = MockLlm::new(vec![fenced]);
        let outcome = generate_workflow_from_prompt("echo", &mock, &AuthorConfig::default())
            .await
            .expect("no transport error");
        assert!(
            outcome.success,
            "fenced JSON must be extracted: {outcome:?}"
        );
        assert_eq!(outcome.iterations.len(), 1, "succeeds on first attempt");
    }

    #[tokio::test]
    async fn extracts_json_with_surrounding_prose() {
        let wrapped = format!(
            "Sure! Here is the workflow you asked for:\n{}\nLet me know if you \
             need changes.",
            valid_workflow_json()
        );
        let mock = MockLlm::new(vec![wrapped]);
        let outcome = generate_workflow_from_prompt("echo", &mock, &AuthorConfig::default())
            .await
            .expect("no transport error");
        assert!(
            outcome.success,
            "prose-wrapped JSON must be extracted: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn junk_exhausts_repairs_and_fails() {
        let cfg = AuthorConfig { max_repairs: 3 };
        let mock = MockLlm::new(vec!["not json".to_string()]);
        let outcome = generate_workflow_from_prompt("do something", &mock, &cfg)
            .await
            .expect("no transport error");
        assert!(!outcome.success);
        assert_eq!(
            outcome.iterations.len(),
            (cfg.max_repairs + 1) as usize,
            "should make max_repairs+1 attempts"
        );
        // Every attempt is a parse error.
        assert!(outcome.iterations.iter().all(|it| it.parse_error.is_some()));
        assert!(outcome.message.contains("could not produce"));
    }

    #[test]
    fn system_prompt_mentions_rules_and_examples() {
        let s = build_system_prompt();
        assert!(s.contains("EXACTLY ONE trigger"));
        assert!(s.contains("http_request"));
        assert!(s.contains("webhook_to_slack") || s.contains("Webhook to Slack"));
        // Few-shot example JSON is embedded.
        assert!(s.contains("\"schema_version\""));
    }

    #[test]
    fn extract_json_strips_fence_and_prose() {
        assert_eq!(extract_json("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(extract_json("prefix {\"a\":1} suffix"), "{\"a\":1}");
        assert_eq!(extract_json("  {\"a\":1}  "), "{\"a\":1}");
        // Nested braces: take to the LAST closing brace.
        assert_eq!(extract_json("{\"a\":{\"b\":2}}"), "{\"a\":{\"b\":2}}");
    }
}
