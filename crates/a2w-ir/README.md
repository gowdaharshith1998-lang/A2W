# a2w-ir

The A2W (Agent-to-Workflow) **Intermediate Representation**: a narrow,
versioned, declarative JSON IR for workflows.

Rust structs are the single source of truth:

- [`schemars`](https://docs.rs/schemars) derives the JSON Schema an LLM emits
  against (`workflow_json_schema()`).
- [`serde`](https://docs.rs/serde) (de)serializes instances
  (`Workflow::from_json`, `Workflow::to_json_pretty`).

## Shape

A `Workflow` is a flat list of `Node`s plus a flat list of `Connection`s.

```text
Workflow { schema_version, id, name, nodes[], connections[] }
Node       { id, kind: NodeKind, params: JSON, retry?, on_error? }
Connection { from_node, from_port, to_node }
```

Design rules:

- **Stable IDs** — nodes are referenced by `id`, never by display name.
- **Port indices** — a `Connection` addresses a source node's output by
  zero-based `from_port`, not by name. Fan-out/fan-in is expressed by repeating
  connections.
- **Shallow** — a flat graph an LLM can emit reliably.

## Node kinds & output ports

`NodeKind::output_port_count()` defines the valid range of `from_port`:

| Kind     | Ports                                            |
| -------- | ------------------------------------------------ |
| `Branch` | 2 (`0` = true, `1` = false)                      |
| `Switch` | dynamic (`DYNAMIC_PORTS`); bound checked later   |
| others   | 1 (index `0`), including triggers                |

`SCHEMA_VERSION` is the current IR version (`1`); instances carry their own
`schema_version`.

## Example

```rust
let wf = a2w_ir::sample_workflow(); // webhook_trigger -> http_request -> transform
let json = wf.to_json_pretty().unwrap();
let back = a2w_ir::Workflow::from_json(&json).unwrap();
assert_eq!(wf, back);
```
