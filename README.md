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
| `a2w-validator`  | Static IR validity: cycles, ports, trigger uniqueness, dangling refs, **per-kind required-field/role checks** (reject-before-execute) |
| `a2w-engine`     | Concurrent DAG executor: bounded fan-out, retry, port-indexed routing, credential resolver hook |
| `a2w-nodes`      | Core executors: webhook/schedule triggers, http_request, transform, merge, mcp_tool_call, code_step, branch, switch, loop, wait, sub_workflow, llm_call, approval |
| `a2w-expr`       | Sandboxed, deterministic expression DSL (no I/O) used by `Transform.set` and templating |
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
| `a2w-verify`     | Verification spine. Calibrated confidence report that **separates engine-invariants** (rerun/permutation/scaling/additivity — verify the engine, NOT the outcome) from **outcome evidence** (spec assertions, golden fixtures, differential cross-checks, spec-derived semantic relations) |
| `a2w-skills`     | Skill library / workflow memory: promote (gated on **outcome** evidence), index by task signature, retrieve & compose. In-memory or persisted to `a2w-store` (`PersistentSkillLibrary`) |
| `a2w-search`     | Deterministic, RNG-free beam search over validity-preserving IR mutations. Selects by a **fitness** plan, certifies the winner on a **disjoint holdout** plan, and reports the holdout-certified score + `overfit_gap` |
| `a2w-bench`      | Criterion benchmarks |
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

20 crates · 346 tests · clippy-clean · cargo-deny-clean · multi-stage Docker
image · CI (fmt + clippy + test + cargo-audit + cargo-deny + docker smoke).
All 14 node kinds have tested executors. Schema v6.

**What "verification" means here — read this before trusting a score.**
A2W's engine is deterministic and per-item-independent *by construction*, so a
class of checks (re-run identity, permutation invariance, duplication scaling,
additivity) holds for **any** valid workflow. Those are **engine-invariants**:
they verify the *engine*, not the *outcome*. They are reported separately and
are **never** counted toward an outcome-correctness claim. **Outcome
verification** rests on spec assertions, golden fixtures, differential
cross-checks, and **spec-derived semantic relations** (which encode the
workflow's intent and catch logic faults engine-invariants cannot). A
confidence report holding only engine-invariants is labeled *"engine-verified;
outcome UNVERIFIED."*

The IR **search** optimizes a fitness plan, so the fitness score is not
independent evidence about the winner. The winner is re-scored on a **disjoint
holdout** plan (a checked-disjoint contract), and the **holdout** score is what
is reported and what gates skill promotion; any `overfit_gap` is surfaced, not
hidden. The full loop (verify → promote → retrieve) runs through the persisted
store and the MCP tools / REST endpoints, not just in memory. Test counts above
are **local** (CI runs the same `--workspace` gate).

Known limitations:
- Engine-invariant relations assert engine guarantees only; outcome correctness
  depends on the quality of the spec/golden/semantic evidence an author supplies
  (garbage-in still applies — the system reports *what* it checked, calibrated).
- Skill retrieval ranks by loading all rows in memory (fine at current scale).
- `compose_sequential` connects every left-terminal to every right-entry node
  (clean for single-terminal/single-entry graphs).
- Query-adaptive sampling (M6) is not implemented — gated behind the
  multi-tenant auth wall.
- Postgres support requires SQL portability work (`INSERT OR IGNORE` is
  SQLite-only).

