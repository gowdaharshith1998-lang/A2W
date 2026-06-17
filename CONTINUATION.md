# A2W — Continuation / Handoff

Pick up here in a new session. A2W = agent-native, Rust alternative to n8n (workflows authored/run by AI agents over MCP; deterministic, zero-token runs).

## Where things are
- **Repo:** `C:\Users\HG\N4N` · remote `https://github.com/gowdaharshith1998-lang/A2W` · branch **`production-hardening`** (MVP is on `main`).
- **Build:** Windows + Rust MSVC. Every shell: `$env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path` then cargo.
- **State:** 15 crates, **200 tests, clippy-clean**. M1–M7 done (full MVP). Then a production-readiness audit ran (verdict below) and **P1 security hardening is ~60% done**.
- Plan doc: `C:\Users\HG\.claude\plans\https-github-com-n8n-io-n8n-i-want-to-stateless-kite.md`. Memory: `~/.claude/projects/C--Users-HG-N4N/memory/` (`a2w-project.md`, `a2w-orchestration-preference.md`).

## Orchestration the user wants
Opus 4.8 = orchestrator + verifier. Workers = `sonnet` tier (user says "4.6"). Harness only exposes coarse tiers (opus/sonnet/haiku/fable) — can't pin 4.7/4.6 point releases. **Always independently verify worker output** (`cargo test --workspace` + `cargo clippy --workspace --all-targets -- -D warnings`); verification has caught real bugs. Parallelize independent crates via pre-stubbed crates so workers never collide on root `Cargo.toml`.

## Production audit verdict (the reason for P1–P5)
NOT production-ready as a prototype that runs untrusted agent-authored workflows. Red dimensions: security, auth, durability, ops, scale. Yellow: completeness, testing. Phases:
- **P1 Security+auth** (in progress) · **P2 Durability** · **P3 Ops/supply-chain** · **P4 Scale** · **P5 Completeness/tests**. (Tasks #16–#21.)

## P1 — exact status
**Done & verified:**
1. Engine credential seam (`crates/a2w-engine/src/node.rs`, `engine.rs`, `lib.rs`): `CredentialResolver` async trait + `CredentialError`; `NodeContext.credentials: Option<Arc<dyn CredentialResolver>>` + `NodeContext::resolve_credential(ref).await`; `Engine::with_credentials(resolver)`.
2. `a2w-nodes` security (`http_request.rs`, `mcp_tool_call.rs`, `lib.rs`):
   - **SSRF guard**: `EgressPolicy` (env: `A2W_HTTP_BLOCK_PRIVATE`=true, `A2W_HTTP_ALLOWED_HOSTS`, `A2W_HTTP_DENIED_HOSTS`, `A2W_HTTP_TIMEOUT_SECS`=30, `A2W_HTTP_MAX_BODY_BYTES`=10MiB) + pure `ip_is_blocked(IpAddr)` (loopback/private/link-local incl 169.254.169.254/CGNAT/ULA/IPv4-mapped) + `check_url_allowed`; client built `redirect=none`, connect/read timeouts, body cap.
   - **MCP allowlist + env strip**: `check_mcp_command_allowed[_with_list]` (env `A2W_MCP_ALLOWED_COMMANDS`, fail-closed if unset); `RmcpInvoker` calls `Command::env_clear()` before setting only spec env.
   - **credential_ref auth injection**: http `auth: { credential_ref, scheme: bearer|header, header_name? }` resolved via `ctx.resolve_credential`, fail-closed (never sends without the secret; secret never in output/errors).
3. `a2w-store` (`resolver.rs`): `StoreCredentialResolver { Arc<Store>, Arc<Vault> }` impl `CredentialResolver` via `Vault::get_secret`. Exported.

**Remaining in P1 (next session, do these):**
1. **Wire it together**: in `a2w-mcp` and `a2w-server`, build the engine as `Engine::new(default_registry()).with_credentials(Arc::new(StoreCredentialResolver::new(store, vault)))`; add credential-write surface (`POST /credentials` / a `wf_store_credential` MCP tool → `Vault::store_secret`). Vault key from `A2W_MASTER_KEY` (base64 32 bytes).
2. **Auth + hardening on `a2w-server`**: API-key middleware (fail-closed when `A2W_API_KEY` set; reject otherwise), request body-size limit, request timeout, request-id (tower-http). Tenancy is a later/optional add.
3. **Auth on `a2w-mcp`**: at minimum gate `wf_run`/destructive tools; document that stdio is local-trust.
4. **Re-run the production audit** (was a Workflow: 7 adversarial code-inspection agents + verdict synthesis) and confirm security/auth move red→green. Adversarially attack the SSRF `ip_is_blocked` (DNS-rebind TOCTOU is a known residual — mitigation note: OS egress filtering; full fix = post-connect IP pin, which reqwest doesn't expose).

## P2–P5 (summaries)
- **P2**: per-node transactional persistence + resume-from-step; honor IR `RetryPolicy` w/ backoff; per-(run,node,item) idempotency before re-firing side effects; a real persisted run path (today only the dry_run endpoint persists; `Store::save_run` exists). Tech-debt already cleaned: `StepEvent` now derives `Deserialize`.
- **P3**: CI (`fmt`/`clippy -D`/`test`/`cargo-audit`) + non-root multi-stage Dockerfile; `tracing` structured logs w/ run_id spans; metrics + `/metrics`; `/ready` DB probe; graceful shutdown; **versioned DB migrations** (today `CREATE TABLE IF NOT EXISTS` only).
- **P4**: bounded fan-out (semaphore — engine uses unbounded `join_all`); Postgres path (drop forced `max_connections(1)` for non-memory pools; verify SQL is PG-compatible); durable queue + webhook/worker split; trigger scheduler; object-storage for large payloads.
- **P5**: implement **Branch/Switch/Loop/Wait** executors with real **port-indexed routing** (core engine change — today `from_port` only sorts fan-in, so only 7 of 14 NodeKinds run); a real expression engine (route `Transform` through it; today `template.rs` is `{{json}}` substring substitution, `Transform.set` is static); fuzz/property tests on untrusted parsers; benchmarks (perf claim is unverified).

## Run it
- MCP server (agent surface): `cargo run -p a2w-mcp` (stdio; 12 `wf_*` tools incl `generate_workflow_from_prompt`).
- REST + dashboard: `cargo run -p a2w-server` → `http://127.0.0.1:8080` (`A2W_BIND`, `A2W_DB_URL` default `sqlite://a2w.db?mode=rwc`).
- LLM authoring needs `ANTHROPIC_API_KEY` (not set on this machine; tests use a MockLlm).
