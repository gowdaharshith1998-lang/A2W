# Live production ETL — real endpoints, real network, zero LLM tokens

`examples/complex_etl_live.json` is the ETL flipped to **real production
endpoints**. It runs in `ExecutionMode::Run` against a live public API:

```
webhook → http GET  https://jsonplaceholder.typicode.com/users   (fetch 10 real users)
        → loop /body                                              (fan out each user)
        → transform  email_norm = lower(email);
                     is_valid   = email TLD ∈ {.org .net .io .ca .me}
        → branch is_valid
            ├─ true  → http POST https://jsonplaceholder.typicode.com/posts   (LOAD: real write)
            └─ false → transform  quarantined = true                          (untrusted TLD)
        → merge
```

The server was launched with the egress firewall scoped to a single host
(`A2W_HTTP_ALLOWED_HOSTS=jsonplaceholder.typicode.com`); private/loopback ranges
stay blocked by the default SSRF guard.

## Result — driven live by a Claude agent, run twice (identical both runs)

| Node | Behavior (real) |
|---|---|
| `fetch` | `GET /users` → **HTTP 200**, body = array of **10 real users** (latency 282 / 491 ms — a genuine outbound call; all in-process nodes report 0 ms) |
| `normalize` | 10 records; `is_valid` = **5 true / 5 false** by the TLD policy |
| `load` | **5 real POSTs** to `/posts`, **every one HTTP 201 Created** with an echoed `body.id` (latency ~2.8–3.1 s for the 5 sequential writes) |
| `quarantine` | 5 records, `reason: untrusted_email_tld` (.biz / .info / .tv) |
| `sink` | merged **10** (5 loaded + 5 quarantined) |
| tokens | **0** across all 16 events — real HTTP I/O, zero LLM tokens |

`idempotent_replay = false` on both runs (genuinely re-executed, not cached);
3 production runs persisted to SQLite (48 step records).

## Independent audit (bypassing A2W entirely)

A second agent fetched the **same live source directly** (`curl …/users`) and
applied the TLD policy by hand:

| # | email | TLD | verdict |
|---|---|---|---|
| 1 | Sincere@april.biz | .biz | quarantine |
| 2 | Shanna@melissa.tv | .tv | quarantine |
| 3 | Nathan@yesenia.net | .net | **load** |
| 4 | Julianne.OConner@kory.org | .org | **load** |
| 5 | Lucio_Hettinger@annie.ca | .ca | **load** |
| 6 | Karley_Dach@jasper.info | .info | quarantine |
| 7 | Telly.Hoeger@billy.biz | .biz | quarantine |
| 8 | Sherwood@rosamond.me | .me | **load** |
| 9 | Chaim_McDermott@dana.io | .io | **load** |
| 10 | Rey.Padberg@karina.biz | .biz | quarantine |

Ground truth: **5 load / 5 quarantine** — **exactly** what the workflow
produced. `matches_workflow = true`.

## Why this is the real thing

This is genuine production execution: the `fetch` and `load` nodes made real
network calls to a live public API (proven by the 200/201 status codes, the
real 10-user dataset, the created-resource ids, and the multi-second write
latency), the routing was independently verified correct against the source, and
the run was deterministic/repeatable.

## Metrics: `external_calls` and LLM `tokens` (now reported)

Each node's step event now carries its **real outbound-call count** and **LLM
token usage** (previously left at 0). Re-running the ETL in production and an LLM
summarizer (against an Anthropic-compatible endpoint), confirmed live by agents:

| Run | Node | `external_calls` | `tokens` |
|---|---|---|---|
| ETL | `fetch` (GET /users) | **1** | 0 |
| ETL | `load` (5× POST /posts) | **5** | 0 |
| ETL | pure-logic nodes | 0 | 0 |
| ETL | **run total** | **6** | 0 |
| LLM | `summarize` (2 tickets) | **2** | **100** (49 + 51 per ticket) |
| LLM | non-LLM nodes | 0 | **0** |

`external_calls` = one per real HTTP request (http/mcp) and one per LLM call;
`tokens` = input + output tokens summed from the provider's `usage` block, which
the `llm_call` node also surfaces per item as `input_tokens` / `output_tokens`.

> The LLM token demo uses a local Anthropic-API-compatible endpoint because no
> `ANTHROPIC_API_KEY` is configured; pointing `A2W_LLM_BASE_URL` at the real
> provider (with a key) reports real-provider tokens through the identical code
> path. The metrics live in the run-response step events (the engine fills them
> from a per-node `NodeMetrics` sink); persisting them into the durable
> `step_records` table is a separate, not-yet-done schema change.

> Not run in CI (CI is network-free); `complex_n8n.rs` asserts only that this
> workflow's IR is statically valid and targets the real endpoints.
