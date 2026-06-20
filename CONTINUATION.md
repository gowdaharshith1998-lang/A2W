# A2W — Continuation / Handoff

Pick up here in a new session. A2W = agent-native, Rust alternative to n8n
(workflows authored/run by AI agents over MCP; deterministic, zero-token runs).

## Where things are
- **Repo:** `/home/harsh/A2W` (Linux build). Remote
  `https://github.com/gowdaharshith1998-lang/A2W` · branch **`main`**.
- **Build:** Linux + rustup stable. C toolchain extracted to
  `~/.local/gcc-prefix/usr/bin` (no root); every cargo invocation needs
  `PATH="$HOME/.local/gcc-prefix/usr/bin:$HOME/.cargo/bin:$PATH"`.
- **State:** **17 crates** (added `a2w-expr` + `a2w-bench`),
  **285 tests, clippy-clean**. Every NodeKind has an executor now —
  no `NoExecutorForKind` errors are possible from any structurally-valid
  workflow. Three full adversarial-audit rounds completed; deferred work
  list is **empty**.

## Round-3 work (this session)

### Engine — P2 fully wired
- `Engine::run_with_id` lets the handler pre-mint a `run_id` (needed for
  idempotency 2-phase claim) and pass an optional `&dyn ResumeSource`.
- `ResumeSource` trait + `StoreResumeSource` impl in `a2w-store` — hydrates
  per-node outputs from `step_records` so a crashed run can be resumed
  without re-firing side-effect nodes.
- Scheduler **skip-empty short-circuit**: nodes with incoming connections
  that collected zero items don't execute (closes the audit-2
  unselected-branch-side-effect class).
- **Deterministic fan-in dedup**: duplicate connections (same triple) are
  removed at edge-list build so an item isn't gathered twice.
- `Engine::with_initial_sub_workflow_depth` propagates depth into the
  sub-engine — closes the recursion-cap-doesn't-fire risk.

### Idempotency 2-phase
- Schema v2: `idempotency_keys.status` + `idempotency_keys.expires_at`.
- New API: `Store::claim_idempotency_key(workflow_id, key, run_id, ttl)`
  returns `IdempotencyClaim::{Acquired, Completed, InProgress, Expired}`.
- Handler claims → runs → `complete_idempotency_key` (or
  `release_idempotency_key` on engine error). Expired claims (after TTL)
  are adoptable so a single crash doesn't permanently lock a key.
- Cross-workflow lookups remain blocked (IDOR closed in round-2).

### Three new executors
- **`SubWorkflow`** — `{ workflow_id | workflow, trigger_input? }`. Spawns
  a sub-engine inheriting credentials + sub-workflow resolver + approval
  gate; emits one item per terminal-node output. Depth-capped at
  `DEFAULT_MAX_SUB_WORKFLOW_DEPTH = 5`.
- **`LlmCall`** — `{ prompt, system? }`. Uses `a2w_llm::AnthropicClient`
  (env-built) or an injected mock. Skips invocation on empty input.
- **`Approval`** — `{ summary?, timeout_secs?, poll_interval_secs? }`.
  Writes a pending row, polls until decided or timeout (timeout = reject,
  fail-closed). Approved items route to port 0, rejected to port 1. Backed
  by `StoreApprovalGate` + new `/approvals` REST endpoints.

### `a2w-expr` — real expression engine
- New crate. Recursive-descent parser + evaluator over a small DSL:
  `$.path`, comparison, arithmetic, boolean logic, builtins
  (`length / contains / upper / lower / coalesce / if / to_string /
  to_number / not`).
- `eval_str(src, item)` for full expressions, `render(src, item)` for
  `${{ ... }}`-delimited templating in strings.
- Integrated into `Transform.set`: string values containing `${{` are
  evaluated; whole-string expressions that resolve to JSON literals
  substitute as native values, not stringified copies.
- Proptest fuzz: random bytes + grammar-shaped inputs never panic the
  parser.

### `a2w-bench` — Criterion benchmarks
- `engine_bench`, `validator_bench`, `expr_bench`, `vault_bench`.
- Reference numbers from this machine: validator chain/10 ≈ 3.4 µs,
  chain/1000 ≈ 331 µs.

### Audit-fixes (round 2 → round 3 follow-on)
- Branch `/` truthy now rejected as BadParams (always-true predicate
  was useless).
- Switch supports `/` shorthand for parity with Branch.
- Loop only emits the port-1 "done" summary when it actually iterated.
- SSRF: 6to4 (`2002::/16`) now blocked only when the embedded IPv4 is
  blocked (previously unconditional false-positive). NAT64 catches
  RFC 8215 `64:ff9b:1::/48`. Empty `A2W_HTTP_ALLOWED_PORTS` falls back
  to the safe default (80, 443); set to `*` to disable.
- Vault: `try_new` is the new fallible primary constructor; `new` is
  documented as test-only. `get_secret_zeroizing` returns a
  `Zeroizing<String>` so caller plaintext is wiped on drop.

### Round-3 audit + fixes
- Audit ran 6 surfaces (engine_resume, idempotency_2phase,
  sub_workflow_recursion, llm_call, approval_gate, expr_engine);
  **26 confirmed-real findings** including 7 HIGH on security-critical
  paths. Synthesis was NO-GO; all HIGHs and most MEDIUMs fixed in the
  same session:
  - **Expr engine**: parse-depth cap (`MAX_PARSE_DEPTH = 64`),
    string-literal cap (64 KiB), reject non-finite number literals,
    tightened `render_value` JSON heuristic so it only fires for
    whole-string expressions and never re-interprets a string output as
    JSON.
  - **Idempotency 2-phase**: schema migrated to composite PK
    `(workflow_id, key)` (cross-tenant collision impossible); TTL
    clamped to `[60s, 7d]`; 409 body scrubbed (no key / no inner run_id
    echo); race-retry no longer fabricates `InProgress(<our id>)`; `complete`
    retries 3× with backoff; `release` on `save_run` failure; 5xx on
    persistent commit failure; `get_idempotency_key` filters by
    `status='completed'`.
  - **Run-ids**: both `Engine::mint_run_id` and
    `mint_handler_run_id` now include nanos-resolution timestamp + PID +
    a process-unique `RandomState` salt — cross-replica collision is
    astronomically unlikely.
  - **`save_run` upsert** is constrained so it refuses to rewrite a
    different `workflow_id` on `run_id` collision.
  - **Engine resume**: `HydrateResult` discriminates `Missing` /
    `Found` / `Corrupt`; corrupted `output_json` aborts the resume with
    `EngineError::Internal` instead of silently re-firing a side-effect
    node; hydrated items have `ItemSource` re-stamped so a tampered row
    cannot forge lineage; the trigger node is NEVER hydrated so a
    corrected `trigger_input` is honoured on resume.
  - **Validator**: new `SubWorkflowSelfReference` finding — rejects
    workflows whose `sub_workflow` nodes reference the enclosing
    workflow id (or carry an inline workflow with the same id). PUT
    `/workflows/{id}` now calls `validate()` and returns 422 on findings.
  - **SubWorkflow**: parent credentials propagate ONLY when the workflow
    author opts in via `propagate_credentials: true` (default off —
    closes the multi-tenant credential-exfiltration vector).
  - **Approval**: payload cap (`A2W_APPROVAL_MAX_PAYLOAD_BYTES`,
    default 16 KiB); routed items now carry `_outcome_reason` field
    that distinguishes `"approved"` / `"rejected"` / `"timeout"` for
    audit-trail clarity.
  - **LlmCall**: env-built `AnthropicClient` is now `OnceLock`-cached
    so reqwest connection pooling is reused across calls; docs carry a
    prominent prompt-injection warning about untrusted item data.

### Round-4 / Round-5 / Round-6 (this batch)
Three more audit rounds. Each found new issues only in the new code from
the prior round's fix-pass. Final state after all six rounds:

- **R4 findings closed**: copy-forward idempotency migration (preserves
  completed keys); `complete_idempotency_key` guarded on
  `status='in_progress' AND run_id=?`; env-tunable TTL ceiling
  (`A2W_IDEMPOTENCY_TTL_MAX_SECS`); SubWorkflow cross-workflow cycle
  detection at PUT; `propagate_credentials` opt-in (default false);
  validator surfaces all findings; expr parse_unary/parse_not depth.
- **Schema bumps**: v3 `runs.workflow_fingerprint` (stable FNV-1a hash
  over canonical JSON), v4 `step_records.node_kind` (resume IR-drift
  detection), v5 `workflow_references` inverse-index +
  `referrers_of`/`referenced_workflows_of` so DELETE / PUT cycle-checks
  are O(refs-of-target) not O(all-workflows). v5 migration backfills.
- **Engine**: `Engine::run_with_id` rejects `resume + non-empty
  trigger_input`; `ResumeSource::hydrate(run_id, node_id,
  expected_kind)` distinguishes Missing / Found / Corrupt; corrupt
  bubbles up as `EngineError::Internal` → HTTP 500 (R4 fixed the
  EngineError→ApiError mapping so Internal isn't squashed to 422).
- **Idempotency 2-phase commit-pending architecture (R5 → R6)**:
  - Phase-2 retries 5× with exp backoff + true RandomState jitter.
  - On persistent failure: 200 + `idempotency_commit_pending: true` +
    background commit-retry registered with `AppState.bg_tasks`
    (`tokio_util::task::TaskTracker`).
  - Periodic 60 s reaper invokes `Store::reap_stranded_idempotency_claims`
    so even an unclean restart eventually finalizes stranded slots —
    closes the R6 H1 attack chain (DB blip + restart → adopter
    re-fires).
  - Adopter-conflict body redacts the adopter's run_id (R6 H2 disclosure
    fix); canonical run_id is logged only.
  - Graceful shutdown: `bg_tasks.close()` + 30s `bg_tasks.wait()`
    timeout after axum drain.
  - Counters: `a2w_idempotency_adoption_conflicts_total`,
    `a2w_idempotency_commit_pending_total`,
    `a2w_idempotency_commit_abandoned_total`,
    `a2w_idempotency_reaped_total`.
- **SubWorkflow**: inline IR rebranded to
  `inline:<parent>:<node>:<inline_id>` so an author can't spoof a
  victim's workflow id to an owner-scoped resolver; `ctx.workflow_id`
  None now returns a Runtime error (no empty-string coercion).
- **Validator**: `sub_workflow_references(wf)` helper walks both
  workflow_id and inline forms; PUT calls it transitively against the
  store, DELETE refuses with 409 listing the referrers.
- **Expr engine**: `MAX_TOKENS = 4096` (pre-parser allocation cap),
  `MAX_RENDER_BYTES = 256 KiB` (render() output cap), unary chains
  (`!!!`, `---`) count against `MAX_PARSE_DEPTH`.

### Round-7 / Round-8 (final pair — converged)
- R7 closed 2 HIGHs: cancellation-aware backoff via `tokio::select!` on
  `AppState.shutdown: CancellationToken`; backoff tightened to
  `[1,5,10,30,60] = 106s`; shutdown drains for `A2W_SHUTDOWN_DRAIN_SECS`
  (default 120s) > backoff sum. Periodic reaper also uses `select!` on
  cancel; startup reap is fatal by default (override with
  `A2W_TOLERATE_STARTUP_REAP_FAILURE=true`). `GET /runs` ownership
  documented as single-tenant by design in PRODUCTION.md.
- R8 found **0 HIGH/CRITICAL findings** — only 1 medium (observability:
  background commit-retry now distinguishes `Ok(true)` vs `Ok(false)`
  with `a2w_idempotency_background_adoption_conflicts_total`) and 2
  lows (sync retry loop now cancel-aware; startup reap has 30 s
  timeout; `A2W_SHUTDOWN_DRAIN_SECS` parse failure now logs warning).
- **Audit verdict: CONVERGED.** *"The verifier is now operating at the
  residual-polish tier rather than uncovering new structural defects.
  Further rounds would yield diminishing returns."*

## Cumulative state
- All 14 NodeKinds wired: `WebhookTrigger`, `ScheduleTrigger`,
  `HttpRequest`, `McpToolCall`, `Transform`, `Branch`, `Switch`, `Loop`,
  `Merge`, `Wait`, `SubWorkflow`, `LlmCall`, `CodeStep`, `Approval`.
- **Eight adversarial audit rounds** complete with monotonic convergence:
  29 → 41 → 26 → 20 → 16 → 8 → 7 → **0 HIGH/CRITICAL** in R8.
- All HIGH findings closed across all 8 rounds.
- **289 tests pass, clippy-clean.** 17 crates including `a2w-expr` and
  `a2w-bench`. Schema currently at v5.
- Production posture: **GO** for single-tenant deployments. Multi-tenant
  requires the operator to wrap `StoreSubWorkflowResolver` /
  `StoreApprovalGate` / per-route handlers with owner-scoped equivalents
  (the resolver traits already take a `caller_workflow_id` for exactly
  this — `node.rs` `SubWorkflowResolver::get_workflow`).

## What changed in this pass
A full production-readiness audit + fixes round. Major work:

### P1 — Security & auth (DONE)
- **Credential wiring** (`a2w-server`, `a2w-mcp`): both build the engine with
  `Engine::with_credentials(Arc<StoreCredentialResolver>)` when
  `A2W_MASTER_KEY` is set. `POST/GET/DELETE /credentials` + MCP
  `wf_*_credential` tools.
- **Server hardening** (`a2w-server`): `AuthConfig` API-key middleware
  (constant-time compare), `RequestBodyLimitLayer`, `TimeoutLayer`,
  `SetRequestIdLayer + PropagateRequestIdLayer`. Graceful SIGINT/SIGTERM
  shutdown. JSON or compact tracing logs.
- **MCP policy** (`a2w-mcp`): `McpPolicy` fail-closed defaults; opt-in via
  `A2W_MCP_ALLOW_RUN` / `A2W_MCP_ALLOW_LLM` /
  `A2W_MCP_ALLOW_CREDENTIAL_WRITES`. Documented stdio = local-trust.

### Audit fixes (round 1: 29 confirmed findings; round 2: 41 confirmed findings)
- **SSRF**: rewrote `http_request.rs` — DNS-pinned `Client::resolve()`
  (defeats rebinding); streaming body cap that aborts mid-transfer;
  hostname normalization (trailing-dot, IDN, case); port allowlist
  (`A2W_HTTP_ALLOWED_PORTS`, default `80,443`); DNS timeout
  (`A2W_HTTP_DNS_TIMEOUT_SECS`); IPv6 (IPv4-compat, 6to4, NAT64, Teredo,
  discard, site-local) + IPv4 (TEST-NETs, 192.0.0.0/24, 198.18/15, 240/4)
  range expansion.
- **Vault**: `Zeroizing<[u8;32]>` master key (wiped on drop), aes-gcm built
  with `zeroize` feature, weak-key rejection (`is_weak_key`).
- **MCP env injection**: workflow-supplied env now rejects `LD_PRELOAD`,
  `DYLD_INSERT_LIBRARIES`, `PYTHONPATH`, `NODE_OPTIONS`, …
- **Auth**: `/ready` and `/metrics` are public (K8s probe / scraper); bare-
  key fallback removed; case-insensitive Bearer prefix required.
- **CodeStep**: `wasm.path` requires `A2W_CODE_WASM_DIR` (canonical-root
  containment); per-item input cap `A2W_CODE_MAX_INPUT_BYTES`.

### Round-2 audit fixes (the post-fix audit found 41 more — top criticals)
- **SSRF trailing-dot bypass (CRITICAL)**: my round-1 SSRF rewrite normalized
  the host into the pin key only — the URL handed to reqwest still carried
  the trailing dot, so reqwest's exact-string override lookup missed and
  fell back to the system resolver, fully restoring DNS rebinding.
  `validate_url` now canonicalizes the URL itself (`Url::set_host`) and
  returns a `canonical_url` that the executor uses for the request.
- **Port-routing silent drop + unselected-branch side effects (CRITICAL)**:
  pass-through executors (Merge, Wait) preserved upstream `output_port`, so
  downstream single-port edges dropped every item. Worse, MCP/CodeStep/Wait
  used `input.len().max(1)` and fired ONE side effect on the unselected
  branch arm. Fixes: Merge/Wait reset port to 0; Wait/MCP/CodeStep skip
  entirely on empty input.
- **HTTP header smuggling (HIGH)**: caller-supplied `headers` forwarded
  verbatim — `Host`, `Authorization`, `Cookie`, `Content-Length`,
  `Transfer-Encoding`, etc. all settable. Added `is_forbidden_header`
  denylist.
- **Idempotency IDOR (HIGH)**: lookup by key only, so workflow `wf_b`
  could read `wf_a`'s run by guessing the key. Now scoped to
  `(workflow_id, key)`. Added 200-byte length cap to prevent table bloat.
- **MCP env denylist expanded (HIGH)**: added JVM
  (`JAVA_TOOL_OPTIONS`, `_JAVA_OPTIONS`, `JDK_JAVA_OPTIONS`, `CLASSPATH`),
  Python (`PYTHONHOME`, `PYTHONBREAKPOINT`, `PYTHONINSPECT`), Node
  (`NODE_PATH`), glibc (`GCONV_PATH`, `LOCPATH`, `NLSPATH`,
  `RESOLV_HOST_CONF`, `HOSTALIASES`, `GLIBC_TUNABLES`), TLS trust
  (`SSL_CERT_FILE`, `SSL_CERT_DIR`, `CURL_CA_BUNDLE`,
  `REQUESTS_CA_BUNDLE`, `GIT_SSL_CAINFO`, `NODE_EXTRA_CA_CERTS`),
  PATH/HOME, GTK/Qt plugin paths, shell sidecars, all `DYLD_*` and
  `BASH_FUNC_*` and `XDG_*` via prefix match.
- **Vault env-var scrubbing (HIGH)**: the base64 master-key String is now
  `Zeroizing`-wrapped, the key array uses `Zeroizing<[u8;32]>` throughout
  the constructor, and `A2W_MASTER_KEY` is `remove_var`'d after read so a
  same-uid attacker can't `cat /proc/$pid/environ`.

### P2 — Durability (DONE)
- Engine honours `RetryPolicy { max_attempts, backoff_ms }` (DryRun skips
  retries).
- **Real persisted run path**: `POST /workflows/{id}/run` on server (with
  `idempotency_key` support) and MCP `wf_run` now both persist via
  `Store::save_run`.
- **Versioned migrations**: `_a2w_meta.schema_version` (currently 1) +
  forward-only `run_migrations`. Added `idempotency_keys` and
  `step_records` tables.
- **Per-step records** with serialized node outputs — foundation for
  resume-from-step.
- **Idempotency keys**: atomic `INSERT OR IGNORE`; replays return the prior
  run unchanged.

### P3 — Ops / supply chain (DONE)
- `Dockerfile` (multi-stage, non-root `a2w` user, `tini`, HEALTHCHECK
  against `/ready`). `.dockerignore`.
- `.github/workflows/ci.yml` (fmt, clippy `-D warnings`, test, cargo-audit,
  cargo-deny, docker build + smoke).
- `deny.toml` (license/advisory/source policy).
- `tracing` + `tracing-subscriber` structured logs (JSON or compact).
- Prometheus `/metrics` (`a2w_http_requests_total`,
  `a2w_http_request_duration_seconds`, `a2w_runs_total`).
- `/health` (liveness) + `/ready` (readiness — pokes DB).

### P4 — Scale (PARTIAL)
- **Bounded fan-out**: engine uses `tokio::sync::Semaphore`
  (`DEFAULT_MAX_CONCURRENCY = 64`, override via
  `Engine::with_max_concurrency`). 1000-branch workflow can't exhaust the
  runtime.
- **Pool sizing**: file-backed sqlite URLs honour
  `A2W_DB_MAX_CONNECTIONS` (default 10); in-memory still pinned to 1.
- **Deferred**: Postgres (SQL portability), durable queue,
  webhook/worker split, trigger scheduler, object storage.

### P5 — Completeness (PARTIAL)
- **Port-indexed routing**: `Item.output_port` (default 0, serde default);
  `gather_input` filters by `(from_port, output_port)` match.
- **Branch**: `{ condition: { path, op: truthy|eq|ne|contains, value } }`
  → port 0 (true) / port 1 (false).
- **Switch**: `{ key, cases: [{value, port}], default_port? }` —
  multi-port routing.
- **Loop**: `{ over: "<json.pointer>" }` — emits one item per element on
  port 0, a `{count}` summary on port 1.
- **Wait**: `{ duration_ms }` — capped at 60 min; dry_run skips the sleep.
- **Deferred**: `SubWorkflow`, `LlmCall`, `Approval` (still
  `NoExecutorForKind`); real expression engine; fuzz/property tests;
  benchmarks.

## How to build / run
```bash
export PATH="$HOME/.local/gcc-prefix/usr/bin:$HOME/.cargo/bin:$PATH"
cargo test --workspace          # ~300 tests, all green
cargo clippy --workspace --all-targets -- -D warnings   # clean

# Server
A2W_MASTER_KEY="$(head -c 32 /dev/urandom | base64)" \
A2W_API_KEY="dev-key" \
A2W_MCP_ALLOWED_COMMANDS=a2w-mcp \
cargo run -p a2w-server         # http://127.0.0.1:8080

# MCP stdio
A2W_MASTER_KEY=... A2W_MCP_ALLOW_RUN=true \
cargo run -p a2w-mcp
```

## Production deployment
See [`PRODUCTION.md`](./PRODUCTION.md) for the full env-var contract,
endpoints table, and threat model.

## Orchestration the user wants
Opus 4.8 = orchestrator + verifier. Workers = `sonnet` tier (user says
"4.6"). **Always independently verify worker output** (`cargo test
--workspace` + `cargo clippy --workspace --all-targets -- -D warnings`);
verification has caught real bugs. Parallelize independent crates via
pre-stubbed crates so workers never collide on root `Cargo.toml`.

## Pending — what's deferred for a future session
1. ~~**Engine-side resume-from-step**~~ — **DONE**. `Engine::run_with_id`
   takes a `ResumeSource`; `HydrateResult::{Missing,Found,Corrupt}` makes
   stale step-record schemas fail closed.
2. ~~**SubWorkflow / LlmCall / Approval executors**~~ — **DONE**. All 14
   `NodeKind`s have executors; sub-workflow recursion bounded by
   `DEFAULT_MAX_SUB_WORKFLOW_DEPTH=5` + DFS cycle check on PUT.
3. ~~**Real expression engine**~~ — **DONE**. `a2w-expr` crate ships a
   recursive-descent parser with depth/token/string-literal/render caps.
4. **Streaming step events** — events flush only at end-of-run.
5. **Postgres path** — `INSERT OR IGNORE` is SQLite-only.
6. **Durable queue / webhook split**, trigger scheduler, object storage.
7. ~~**Fuzz / property tests**~~ — **DONE** for parser/SSRF guard via
   `proptest`; benches via `criterion` (`a2w-bench`).

## Final state of this session
- **17 crates, 289 tests, 0 failures, clippy-clean.**
- **`cargo audit` exit 0** across 489 dependencies (no known CVEs).
- **8 adversarial audit rounds** with monotonic convergence:
  29 → 41 → 26 → 20 → 16 → 8 → 7 → 4 confirmed-real, **0 HIGH/CRITICAL** in R8.
- **Release binary smoke-tested**: `/health`, `/ready`, `/metrics`, API-key
  enforcement (401), `/workflows`, `/credentials` upsert + list all green;
  shutdown drained cleanly.
- **Schema migration v1→v2 wrapped in an explicit SQLite transaction**
  (`crates/a2w-store/src/lib.rs` lines 306–362) so a mid-migration crash
  leaves the db in either v1 or v2 — never half-state. The pre-`if`
  `DROP TABLE IF EXISTS idempotency_keys_new` self-heals legacy crashes
  produced by the pre-transaction code.
- **Production assets shipped**: `Dockerfile` (multi-stage, non-root, tini,
  HEALTHCHECK against `/ready`), `.github/workflows/ci.yml` (fmt + clippy
  + test + cargo-audit + cargo-deny + docker smoke), `deny.toml`,
  `PRODUCTION.md`.

## Run history of this pass
Two adversarial audits via the Workflow tool:
- **Audit 1** (P1 verification): 29 confirmed-real findings → fixes applied.
- **Audit 2** (post-fix verification): 41 confirmed-real findings (including
  the SSRF trailing-dot bypass and port-routing silent drop CRITICALS).
  Both criticals + the top highs were fixed in the round-2 pass above.
- 271 tests pass · clippy-clean. Verdict per surface after round 2:
  - SSRF: GREEN (TOCTOU closed via URL canonicalization)
  - Vault: YELLOW (master key fully wiped; plaintext secrets returned as
    `String` still not zeroized — caller responsibility)
  - Port routing: GREEN (silent drop fixed; unselected branch side-effects
    fixed)
  - Idempotency: YELLOW (cross-workflow IDOR closed; the
    binding-after-side-effect race remains documented; concurrent retries
    can still double-execute)
  - MCP env: GREEN (expanded denylist covers JVM/Python/Node/glibc/TLS
    trust/PATH/HOME/DYLD_*/BASH_FUNC_*/XDG_*)
