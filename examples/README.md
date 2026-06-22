# Example workflows

Real A2W workflow IR documents — the exact JSON an agent emits against
[`wf_get_schema`](../README.md#mcp-tools). Each is loaded, **statically
validated** (M1), **run deterministically and zero-token**, and **verified**
with a calibrated confidence report by
[`crates/a2w-acceptance/tests/workflow_gallery.rs`](../crates/a2w-acceptance/tests/workflow_gallery.rs),
so everything here is guaranteed runnable — not decorative.

Run the gallery:

```bash
cargo test -p a2w-acceptance --test workflow_gallery -- --nocapture
```

| File | Shape | Exercises |
|---|---|---|
| [`order_pricing.json`](./order_pricing.json) | `trigger → transform` | expression arithmetic (`total = price × qty`); verified by a **semantic scaling relation** |
| [`alert_router.json`](./alert_router.json) | `trigger → branch → {escalate, note}` | two-way port routing; verified by a spec assertion + append relation |
| [`severity_switch.json`](./severity_switch.json) | `trigger → switch → 4 sinks` | multi-way routing on a key with a default port |
| [`order_items_loop.json`](./order_items_loop.json) | `trigger → loop → {process, summary}` | array fan-out (one item per element) + per-parent done summary |
| [`enrich_merge.json`](./enrich_merge.json) | `trigger ⇉ {region, tier} → merge` | concurrent diamond + fan-in with guaranteed lineage |
| [`http_fetch_shape.json`](./http_fetch_shape.json) | `trigger → http_request → transform` | a side-effecting node, **mocked in dry-run** so verification stays zero-token |
| [`deep_pipeline.json`](./deep_pipeline.json) | `trigger → t₁ → t₂ → t₃` | a deep staged chain; count-conservation |

## Complex, n8n-style automations

Production-shaped workflows that combine many node kinds the way a real n8n
automation does — built, run, and asserted by
[`crates/a2w-acceptance/tests/complex_n8n.rs`](../crates/a2w-acceptance/tests/complex_n8n.rs)
(`cargo test -p a2w-acceptance --test complex_n8n -- --nocapture`):

| File | Shape | What it automates |
|---|---|---|
| [`complex_lead_routing.json`](./complex_lead_routing.json) | `webhook → score → classify → switch → {AE+CRM, nurture, newsletter} → merge` | scores inbound leads (`employees·0.5 + budget/1000 + referral bonus`), tiers them hot/warm/cold, routes each to the right play, and fires a CRM-sync HTTP call for hot leads |
| [`complex_order_fulfillment.json`](./complex_order_fulfillment.json) | `webhook ⇉ {loop→price, branch→branch→approval→ship} → merge` | prices each line item via a loop, gates on payment, sends high-value orders through a human **approval** before express shipping, auto-ships the rest, holds the unpaid |
| [`complex_etl_sync.json`](./complex_etl_sync.json) | `schedule → normalize → branch → {load, quarantine} → merge` | a cron ETL that lowercases + validates each record (`length` + `contains`) and splits the batch into load-ready vs quarantined |
| [`complex_ticket_triage.json`](./complex_ticket_triage.json) | `webhook → switch → {page, escalate, llm draft, autoclose} → merge` | routes support tickets by severity, paging on-call for critical, drafting a reply with an **LLM** node for normal ones, auto-acknowledging the rest |

The gallery test also demonstrates the compounding loop end-to-end:

- **Promote (M4):** a verified workflow is promoted into the skill library on its
  outcome evidence and retrieved for a similar query.
- **Search (M5):** a deliberately-broken pricing workflow is *evolved* — ranked
  on a **fitness** plan and certified on a **disjoint holdout** — and its
  certified score improves to perfect with no overfit gap.
