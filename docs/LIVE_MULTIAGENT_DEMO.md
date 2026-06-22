# Live multi-agent demo — A2W driven by real Claude agents in real time

This is a record of an end-to-end exercise where **five independent Claude agents**
each authored a distinct workflow and drove the **entire A2W loop** against a
**running `a2w-server`** (real HTTP, API-key auth, AES-256-GCM credential vault,
SQLite persistence) — proving the product works in real time, not just in unit
tests.

## Setup

```
a2w-server  →  http://127.0.0.1:8099   (Authorization: Bearer <key>, vault enabled)
DB          →  sqlite:///tmp/a2w_live.db
execution   →  ExecutionMode::Run (production), pure-logic workflows ⇒ zero LLM tokens
```

Each agent was given only a domain (a linear formula) and the HTTP contract. It
authored its own IR and drove six steps over `curl`:

`PUT /workflows/{id}` → `POST /workflows/{id}/run` (production) → `POST /verify`
→ `POST /skills` (promote) → `GET /skills?query=…` (find) → `POST /search` (evolve).

## Result — 5/5 agents passed all 6 steps

| Workflow | run (prod) | verify | promote (skill) | find | evolve (certified) |
|---|---|---|---|---|---|
| `wf_live_area` | ✅ zero-token | ✅ 1.00 | ✅ `skill_84d0e1ed…` h=1.0 | ✅ | ✅ 0.0→1.0 |
| `wf_live_discount` | ✅ zero-token | ✅ 1.00 | ✅ `skill_631ff071…` h=1.0 | ✅ | ✅ 0.0→1.0 |
| `wf_live_distance` | ✅ zero-token | ✅ 1.00 | ✅ `skill_269f5798…` h=1.0 | ✅ | ✅ 0.0→1.0 |
| `wf_live_invoice` | ✅ zero-token | ✅ 1.00 | ✅ `skill_859aaee4…` h=1.0 | ✅ | ✅ 0.0→1.0 |
| `wf_live_payroll` | ✅ zero-token | ✅ 1.00 | ✅ `skill_d018109f…` h=1.0 | ✅ | ✅ 0.0→1.0 |

### What each step proved (verbatim agent evidence, `wf_live_invoice`)

- **run_production** — `status "completed"`, node `calc` `finished` with
  `output_items=3`, **every event `tokens:0`** (zero-token). Computed
  `qty*unit_price` → `12.0, 19.0, 12.5` (floats).
- **verify** — `outcome_verified:true, outcome_score:1.0`; 4/4 checks: spec
  `every_item_has /line_total`, `output_count==3`, golden (`3*4.0=12.0`), and the
  spec-derived **FieldScaling** relation `Σ/line_total 43.5→87 (expected 87)`.
- **promote** — `skill_859aaee402b97126`, **holdout_score 1.0** (certified on a
  disjoint holdout golden + a factor-3 scaling relation).
- **find** — that exact skill id returned as the **top match** on a *paraphrased*
  query (`similarity 0.3125`).
- **evolve** — a deliberately broken seed (`line_total = ${{ $.discount * 2 }}`,
  field absent) was repaired by beam search: `certified_score 0.0 → 1.0`,
  `overfit_gap 0.0`, rewriting the transform back to `${{ $.qty * $.unit_price }}`
  and certifying it on a **disjoint holdout**.

## Independent persistence check (read straight from the SQLite file)

```
tables: workflows, runs, step_records, skills, credentials, approvals, …
skills:        5 rows, all holdout_score = 1.0   (one per domain)
workflows:     wf_live_{area,discount,distance,invoice,payroll}
runs:          6 production runs persisted
step_records: 24 production-run step events persisted
```

The skills, workflows, and production-run records were all written by the live
agents through the real server — confirmed by querying the database directly,
independently of the HTTP API.

## Why this is meaningful

The full compounding loop — **author → run → verify → promote → retrieve →
evolve** — ran through the shipped product over the network, driven by autonomous
Claude agents, with **deterministic, zero-token** production execution and
**holdout-certified** self-improvement. Nothing here is mocked: it is the same
`a2w-server`, engine, verifier, skill library, and search that the test suite and
CI exercise.
