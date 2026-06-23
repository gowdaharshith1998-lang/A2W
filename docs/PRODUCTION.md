# A2W — Production Deployment

This document captures the runtime contract for operating A2W in production:
the environment variables, observability surface, and security posture.

## Binaries

- **`a2w-server`** — REST API + observability dashboard over the workflow store
  and engine. Bind: `127.0.0.1:8080` by default. Builds from `crates/a2w-server`.
- **`a2w-mcp`** — MCP stdio server exposing `wf_*` tools to an agent (Claude,
  etc.). Builds from `crates/a2w-mcp`.

## Container image

A multi-stage `Dockerfile` lives at the repo root.

```bash
docker build -t a2w-server .
docker run --rm -p 8080:8080 \
  -e A2W_MASTER_KEY="$(head -c 32 /dev/urandom | base64)" \
  -e A2W_API_KEY="$(head -c 24 /dev/urandom | base64)" \
  -e A2W_MCP_ALLOWED_COMMANDS=a2w-mcp \
  a2w-server
```

The runtime stage runs as the **non-root** `a2w` user, uses `tini` as PID-1
for child reaping (the MCP node spawns child processes), and runs an HTTP
healthcheck against `GET /ready` every 15 s.

## Environment variables

### Persistence
| Var | Default | Effect |
|---|---|---|
| `A2W_DB_URL` | `sqlite://a2w.db?mode=rwc` | sqlx URL — `sqlite::memory:` (single-conn) or a file-backed URL (pooled) |
| `A2W_DB_MAX_CONNECTIONS` | `10` (file-backed) | Pool size; ignored for in-memory URLs (forced to 1) |

### Credentials (vault)
| Var | Default | Effect |
|---|---|---|
| `A2W_MASTER_KEY` | _unset_ | base64 of **exactly 32 bytes**; required for HTTP nodes that reference `credential_ref`. Weak keys (all-equal byte) are rejected at startup. |

When unset, the server starts in **no-credential mode**: the `/credentials`
endpoints return `503`, the MCP `wf_*_credential` tools return an error,
and HTTP/MCP nodes referencing a `credential_ref` fail closed at run time.
A misconfigured key is a **fatal startup error** rather than a silent fallback.

### Server hardening
| Var | Default | Effect |
|---|---|---|
| `A2W_BIND` | `127.0.0.1:8080` | TCP listener address |
| `A2W_API_KEY` | _unset_ | When set, every request other than `/`, `/health`, `/ready`, `/metrics` requires `Authorization: Bearer <key>` (case-insensitive `Bearer`); rejected with `401` otherwise. |
| `A2W_MAX_BODY_BYTES` | `1048576` (1 MiB) | Cap on request body; oversized requests get `413` |
| `A2W_REQUEST_TIMEOUT_SECS` | `30` | Per-request handler timeout; expired requests get `408` |

### SSRF guard (HTTP node)
| Var | Default | Effect |
|---|---|---|
| `A2W_HTTP_BLOCK_PRIVATE` | `true` | When `true`, refuses connection to loopback / private / link-local / CGNAT / IANA-reserved IPs (IPv4 + IPv6, including IPv4-mapped, 6to4, NAT64, Teredo) |
| `A2W_HTTP_ALLOWED_HOSTS` | _empty_ | Comma-separated allowlist; when set ONLY these hosts are permitted (normalized: trailing dot trimmed, IDN→ASCII, lowercased) |
| `A2W_HTTP_DENIED_HOSTS` | _empty_ | Comma-separated denylist |
| `A2W_HTTP_ALLOWED_PORTS` | `80,443` | Comma-separated allowed ports; empty string disables port filtering |
| `A2W_HTTP_TIMEOUT_SECS` | `30` | Per-request response timeout |
| `A2W_HTTP_DNS_TIMEOUT_SECS` | `3` | DNS lookup timeout |
| `A2W_HTTP_MAX_BODY_BYTES` | `10485760` (10 MiB) | Response body cap; streams chunk-by-chunk and aborts when exceeded |

The egress guard **pins the resolved IP**: the SocketAddr validated by
`validate_url` is the one reqwest connects to (`Client::resolve()`), so a
DNS-rebinding attack between guard and connect is closed.

### MCP node (stdio child processes)
| Var | Default | Effect |
|---|---|---|
| `A2W_MCP_ALLOWED_COMMANDS` | _empty (fail-closed)_ | Comma-separated list of command names a workflow's `mcp_tool_call` may spawn. Unset = no stdio MCP spawn permitted. |

The child's environment is `env_clear()`ed before workflow-supplied env is
re-applied; library-injection keys (`LD_PRELOAD`, `LD_LIBRARY_PATH`,
`DYLD_INSERT_LIBRARIES`, `PYTHONPATH`, `NODE_OPTIONS`, etc.) are rejected.

### CodeStep (WASM sandbox)
| Var | Default | Effect |
|---|---|---|
| `A2W_CODE_WASM_DIR` | _unset (disables path source)_ | Canonical directory; `wasm.path` may only load modules under here |
| `A2W_CODE_MAX_INPUT_BYTES` | `1048576` (1 MiB) | Per-item input payload cap |

### MCP server policy (`a2w-mcp` binary)
The MCP stdio transport is **local-trust** (any process that can spawn
`a2w-mcp` has full access). The policy below provides operator-level fail-
closed defaults; turn them on consciously.

| Var | Default | Effect |
|---|---|---|
| `A2W_MCP_ALLOW_RUN` | `false` | When `true`, `wf_run` may execute real side effects |
| `A2W_MCP_ALLOW_LLM` | `false` | When `true`, `generate_workflow_from_prompt` may call the LLM |
| `A2W_MCP_ALLOW_CREDENTIAL_WRITES` | `false` | When `true`, `wf_store_credential` / `wf_delete_credential` may mutate the vault. Reading is always allowed. |

### Observability
| Var | Default | Effect |
|---|---|---|
| `RUST_LOG` | `info` | tracing-subscriber filter |
| `A2W_LOG_JSON` | `false` | When `true`/`1`, logs are JSON; otherwise compact text |

## Endpoints

| Method   | Path                          | Auth      | Notes |
|----------|-------------------------------|-----------|-------|
| `GET`    | `/`                           | none      | HTML observability dashboard |
| `GET`    | `/health`                     | none      | Liveness — process is up |
| `GET`    | `/ready`                      | none      | Readiness — pokes the DB; `503` on failure |
| `GET`    | `/metrics`                    | none      | Prometheus text format |
| `GET`    | `/workflows`                  | API key   | list `{id, name}` |
| `GET`    | `/workflows/{id}`             | API key   | get the workflow IR |
| `PUT`    | `/workflows/{id}`             | API key   | upsert (body id must match path) |
| `DELETE` | `/workflows/{id}`             | API key   | idempotent delete |
| `GET`    | `/workflows/{id}/runs`        | API key   | run id list |
| `POST`   | `/workflows/{id}/validate`    | API key   | validate the stored IR |
| `POST`   | `/workflows/{id}/dry_run`     | API key   | dry-run (side effects mocked) and persist |
| `POST`   | `/workflows/{id}/run`         | API key   | **real run** — body may carry `{ trigger_input, idempotency_key }` |
| `GET`    | `/runs/{run_id}`              | API key   | the stored run record (events + status) |
| `POST`   | `/runs/{run_id}/resume`       | API key   | resume a crashed run from persisted `step_records` (side-effect nodes are not re-fired) |
| `POST`   | `/verify`                     | API key   | run a verification plan → calibrated confidence report (engine-invariants vs outcome evidence) |
| `POST`   | `/search`                     | API key   | evolve a seed workflow; ranks on a fitness plan, certifies the winner on a **disjoint holdout** |
| `GET`    | `/skills`                     | API key   | retrieve persisted skills, ranked by `query` similarity |
| `POST`   | `/skills`                     | API key   | verify on a holdout plan, then promote iff it clears the threshold |
| `GET`    | `/approvals`                  | API key   | list pending human-approval requests |
| `GET`    | `/approvals/{id}`             | API key   | get one approval request |
| `POST`   | `/approvals/{id}`             | API key   | decide an approval (approve / reject) |
| `GET`    | `/credentials`                | API key   | list `{id, name, created_at}` — never the secret |
| `POST`   | `/credentials`                | API key   | upsert `{id, name, secret}` |
| `DELETE` | `/credentials/{id}`           | API key   | idempotent delete |

`x-request-id` is set on every response (echoed when supplied on the request).

### Idempotency

`POST /workflows/{id}/run` accepts `idempotency_key` in the body. The first
call commits `(key → run_id)`; subsequent calls with the same key return the
prior run unchanged (`idempotent_replay: true`) without re-firing side
effects.

## Operational notes

- **Graceful shutdown**: the server traps SIGINT and (on Unix) SIGTERM, then
  drains in-flight requests before exiting.
- **Bounded fan-out**: the engine caps concurrent node execution at 64 (set
  via `Engine::with_max_concurrency`). A 1000-branch workflow can't exhaust
  the tokio runtime.
- **Versioned migrations**: `_a2w_meta.schema_version` tracks the current
  schema (currently **7**). New migrations run forward-only and are idempotent;
  destructive ones are wrapped in a single transaction, so a mid-migration crash
  leaves the DB at a clean prior-or-next version — never half-applied.
- **Per-step records**: `step_records` captures every node event with the
  serialized output, plus its real `external_calls` count and LLM `tokens` (v7).
  This is the substrate for **resume-from-step**: `POST /runs/{run_id}/resume`
  rehydrates per-node outputs so a crashed run continues without re-firing
  side-effect nodes (a corrupt or schema-drifted record fails closed).
- **Retry**: nodes honour `retry: { max_attempts, backoff_ms }` from the IR;
  attempts are recorded in the step event stream (`Failed` events with
  `external_calls` carrying the attempt number).

## Ownership / multi-tenancy

A2W ships as a **single-tenant** service: a single `A2W_API_KEY` (when
configured) grants full access to every workflow, run, credential, and
approval in the store. Endpoints like `GET /runs/{run_id}`,
`GET /workflows/{id}/runs`, `GET /credentials`, and `GET /approvals` do
not perform per-caller ownership checks beyond the API-key gate.

This is an architectural choice, not an oversight. Audit findings such
as "GET /runs has no per-caller authorization" are accurate against a
multi-tenant deployment but **out-of-scope for single-tenant**, because
there is no caller identity richer than the shared key. The
`audit_warning` body intentionally redacts adopter `run_id`s under that
same threat model (a same-key holder could already enumerate via
`list_runs`, but the redaction keeps the disclosure surface from
*growing* per-request).

**To deploy multi-tenant**, the operator must:

1. Wrap `StoreSubWorkflowResolver`, `StoreApprovalGate`, and the
   per-route handlers with owner-scoped equivalents that consult an
   external identity layer (the `SubWorkflowResolver` trait already
   takes a `caller_workflow_id` for exactly this).
2. Add an owner column to `workflows` / `runs` / `credentials` and
   filter every list/get by owner.
3. Replace the single `A2W_API_KEY` with per-principal credentials
   (JWT, mTLS, etc.).

The shipped code is structured so this is additive — no breaking
changes to the data model are needed; the resolvers and handlers are
already factored as the natural extension points.

## Threat model summary

| Surface | Posture |
|---|---|
| External HTTP egress | Default-deny private/reserved; allowlist + denylist + port allowlist; pinned-IP connector defeats DNS rebinding; streaming body cap |
| Credential vault | AES-256-GCM, fresh nonce per write; key wiped on drop; weak-key rejection |
| API auth | API key (constant-time compare) required when `A2W_API_KEY` set |
| MCP stdio | Local-trust; per-tool policy flags must be opted-into; command allowlist + env-injection denylist |
| WASM CodeStep | extism sandbox, no host functions, bounded memory + wall-clock; per-item input cap; path source restricted to a configured directory |
| Persistence | sqlite WAL by default; transactional save_run; idempotency keys |

## Known limitations

These are the **true, current** limitations — calibrated, not aspirational.
Items the early roadmap deferred (resume-from-step, all 14 node executors, the
expression DSL, durable step metrics) have since shipped and are no longer
listed here; see the engine and `a2w-expr` crates for the implementations.

- **Streaming step events**: events flush at end-of-run only. A hung node
  produces no visibility until it returns (or the run is killed).
- **Postgres**: the durable store targets SQLite (`INSERT OR IGNORE`, WAL); a
  Postgres backend needs portable upserts. Single-node SQLite is the supported
  deployment today.
- **Distributed queue**: no durable queue or webhook/worker split — one process
  owns triggering and execution. Horizontal scale is a future milestone.
- **Multi-tenancy**: ships **single-tenant** (one `A2W_API_KEY` grants full
  access). The resolvers and per-route handlers are factored as the extension
  points for an owner-scoped layer (see *Ownership / multi-tenancy* above), but
  that layer is not bundled.
- **Outcome verification is only as strong as its evidence**: engine-invariants
  hold for *any* valid workflow and verify the **engine**, not the **outcome**.
  Outcome correctness depends on the spec assertions, golden fixtures, and
  spec-derived semantic relations an author supplies; an evidence-free report is
  reported as *"engine-verified; outcome UNVERIFIED."*
