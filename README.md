# A2W — Agent-native workflow engine

A2W is a Rust workflow engine designed for **AI agents** to author, validate,
run, and optimize over a narrow, deterministic JSON IR via the
[Model Context Protocol](https://modelcontextprotocol.io). Workflows execute
as a concurrent async DAG with guaranteed item lineage; everyday runs are
deterministic and zero-token.

```
agent ─── MCP (stdio) ──── a2w-mcp ──┐
                                     ├── a2w-engine ── a2w-nodes
agent / user ── HTTP ─── a2w-server ─┘                       │
                                                   ↑    ↓    │
                                              a2w-store     a2w-templates
                                              (sqlite +     a2w-author
                                               vault)       a2w-optimizer
```

## Crates

| Crate            | Purpose |
|------------------|---------|
| `a2w-ir`         | Workflow IR (`Workflow`, `Node`, `NodeKind`, `Connection`, policies) — single source of truth |
| `a2w-validator`  | Structural validation (cycles, ports, trigger uniqueness, dangling refs) |
| `a2w-engine`     | Concurrent DAG executor: bounded fan-out, retry, port-indexed routing, credential resolver hook |
| `a2w-nodes`      | Core executors: webhook/schedule triggers, http_request, transform, merge, mcp_tool_call, code_step, branch, switch, loop, wait |
| `a2w-store`      | sqlite persistence: workflows, runs, per-step records, idempotency keys, AES-256-GCM vault |
| `a2w-server`     | REST API + observability dashboard (axum), API-key auth, /metrics, /ready, JSON logs |
| `a2w-mcp`        | MCP stdio server exposing `wf_*` tools (validate / dry_run / run / profile / optimize / apply / search_templates / store_credential / …) |
| `a2w-author`     | Generate→Validate→Repair authoring loop (`generate_workflow_from_prompt`) |
| `a2w-llm`        | LLM client abstraction (`AnthropicClient`, `MockLlm`) |
| `a2w-optimizer`  | Workflow analysis: parallelize, dead-node, profile, suggest IR diffs |
| `a2w-testkit`    | Declarative `TestCase` evaluator (DryRun-based) |
| `a2w-templates`  | Golden template corpus (`wf_search_templates`) |
| `a2w-import`     | n8n → A2W IR importer |
| `a2w-openapi`    | OpenAPI → A2W IR adapter |
| `a2w-acceptance` | End-to-end acceptance tests |

## Quickstart

```bash
# Build + test the workspace.
cargo test --workspace

# Run the REST server with credentials enabled.
A2W_MASTER_KEY="$(head -c 32 /dev/urandom | base64)" \
A2W_API_KEY="dev-key" \
cargo run -p a2w-server
# → http://127.0.0.1:8080  (dashboard at /, REST at /workflows, /runs, /credentials)

# Run the MCP stdio server. Default policy is fail-closed (no wf_run, no LLM,
# no credential writes); opt in per surface.
A2W_MASTER_KEY=... \
A2W_MCP_ALLOW_RUN=true \
A2W_MCP_ALLOWED_COMMANDS=a2w-mcp \
cargo run -p a2w-mcp
```

## MCP tools

| Tool                            | Purpose |
|---------------------------------|---------|
| `wf_get_schema`                 | Return the `Workflow` JSON Schema |
| `wf_describe_nodes`             | Node taxonomy (kind, port count, is_trigger) |
| `wf_validate`                   | Structural validation report |
| `wf_dry_run`                    | Run with side effects mocked (always allowed) |
| `wf_run`                        | Run for real — gated by `A2W_MCP_ALLOW_RUN` |
| `wf_run_tests`                  | Evaluate declarative test cases |
| `wf_profile`                    | DryRun + per-step latency + critical path |
| `wf_optimize`                   | Suggestions (parallelize, dead-node) as IR diff ops |
| `wf_apply_ops`                  | Apply IR diff ops |
| `wf_search_templates`           | Keyword search the golden template corpus |
| `wf_get_template`               | Fetch a template's workflow IR |
| `wf_store_credential`           | Upsert a credential — gated by `A2W_MCP_ALLOW_CREDENTIAL_WRITES` |
| `wf_list_credentials`           | List `{id, name, created_at}` — no secrets |
| `wf_delete_credential`          | Delete by id — gated by `A2W_MCP_ALLOW_CREDENTIAL_WRITES` |
| `generate_workflow_from_prompt` | Generate→Validate→Repair authoring — gated by `A2W_MCP_ALLOW_LLM` |

## Production deployment

See [`PRODUCTION.md`](./PRODUCTION.md) for the complete env-var contract,
REST endpoints table, Docker image, observability, and threat model.

## Status

15 crates · ~300 tests · clippy-clean · multi-stage Docker image · CI
(fmt + clippy + test + cargo-audit + cargo-deny + docker smoke). Full
production audit + fix round completed; security/auth surfaces flipped
red → green.

Known limitations:
- `SubWorkflow`, `LlmCall`, `Approval` node kinds have no executor yet —
  workflows referencing them get a clean `NoExecutorForKind` error.
- Engine-side resume-from-step not yet wired (per-step records ARE
  persisted with serialized outputs — foundation only).
- Postgres support requires SQL portability work (`INSERT OR IGNORE` is
  SQLite-only).
- `Transform.set` is static; a real expression engine is a future
  milestone.

