# a2w-validator

Deterministic structural and semantic validation for the
[`a2w-ir`](../a2w-ir) workflow IR.

`validate(&Workflow) -> ValidationReport` returns **located, fix-suggesting**
findings in a **stable order**. Identical input always yields a byte-identical
report — the property the validate→repair loop depends on.

```rust
let report = a2w_validator::validate(&a2w_ir::sample_workflow());
assert!(report.is_valid); // no Error-severity findings
```

## Findings

Each `Finding` has a `severity`, a machine-readable `code`, a `message` (always
naming the offending id), a `location`, and an optional `suggestion`.
`ValidationReport.is_valid` is `true` iff there are no `Error`-severity
findings.

| Code                         | Severity | Meaning                                            |
| ---------------------------- | -------- | -------------------------------------------------- |
| `EmptyWorkflow`              | Error    | No nodes at all                                    |
| `DuplicateNodeId`            | Error    | An id is used by more than one node                |
| `NoTrigger`                  | Error    | No trigger node                                    |
| `MultipleTriggers`           | Error    | More than one trigger node                         |
| `DanglingConnectionSource`   | Error    | `from_node` names no existing node                 |
| `DanglingConnectionTarget`   | Error    | `to_node` names no existing node                   |
| `InvalidOutputPort`          | Error    | `from_port` out of range for the source kind       |
| `Cycle`                      | Error    | The graph is not acyclic                           |
| `UnreachableNode`            | Warning  | Node not reachable from the trigger                |

## Design notes

- **Multiple triggers is an Error**, not a warning: M1 workflows have a single
  entry point, and "reachable from the trigger" is otherwise ambiguous.
- **Switch ports are dynamic.** `Switch` reports `DYNAMIC_PORTS`, so
  `InvalidOutputPort` is *not* enforced for it in M1; the real per-case bound
  is checked once typed params land.
- **Reachability is conditional.** `UnreachableNode` is only computed when there
  is exactly one trigger and no cycle, since reachability is meaningless
  otherwise. Dangling connections are excluded from graph analysis (they're
  already reported), so they don't corrupt cycle/reachability results.
- **Determinism.** Findings are sorted by `(severity, code, location, message)`;
  cycle detection uses `petgraph::algo::toposort` over nodes inserted in
  declaration order.
