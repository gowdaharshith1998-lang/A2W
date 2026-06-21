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

The gallery test also demonstrates the compounding loop end-to-end:

- **Promote (M4):** a verified workflow is promoted into the skill library on its
  outcome evidence and retrieved for a similar query.
- **Search (M5):** a deliberately-broken pricing workflow is *evolved* — ranked
  on a **fitness** plan and certified on a **disjoint holdout** — and its
  certified score improves to perfect with no overfit gap.
