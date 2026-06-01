---
description: Wire format and workflow for authoring and running metalcraft flows
---

# Authoring Flows

A flow is a JSON DAG of nodes (`flows/<id>.json`) executed by the runtime. Edit
with `flow_write` (pass a `flow` document), read with `flow_read`, check with
`flow_validate`, run with `flow_run`. Start from a template with
`flow_templates_list` + `flow_template_read`.

**Always `flow_validate` before `flow_write`.** `flow_write` re-validates and
refuses to save an invalid flow, returning the errors.

## Document shape (`SavedFlow`)

```json
{
  "spec_version": "1",
  "id": "daily-summary",
  "name": "Daily Summary",
  "created_at": "2026-01-01T00:00:00Z",
  "updated_at": "2026-01-01T00:00:00Z",
  "enabled": false,
  "flow": {
    "nodes": [
      { "id": "entry",  "node_type": "entry",  "data": { "schedule_type": "manual" } },
      { "id": "work",   "node_type": "prompt", "data": { "prompt": "Summarize today's commits." } }
    ],
    "edges": [
      { "id": "e1", "source": "entry", "target": "work" }
    ]
  }
}
```

- **id** — must match `^[A-Za-z0-9-]{1,64}$` (lowercase-hyphen).
- **enabled** — whether the scheduler daemon runs it; `false` for manual/`flow_run`.
- **flow.nodes / flow.edges** — the graph. Only nodes reachable from the entry
  node execute; disconnected nodes are ignored.

## Core node types (`node_type` + `data`)

| node_type     | data                                                                                             | Purpose |
|---------------|--------------------------------------------------------------------------------------------------|---------|
| `entry`       | `{ "schedule_type": "manual"\|"minutes"\|"hours"\|"cron", "interval"?: number, "cron"?: string }` | Start node. At most one per flow. |
| `prompt`      | `{ "prompt": string, "persona"?: string }`                                                       | A natural-language task run by an agent. Optional per-node persona override. |
| `branch`      | `{ "condition": string }`                                                                        | Splits flow; edges use `source_handle` `"true"`/`"false"`. |
| `branch_tool` | `{ "tool_name": string, "branches": { outcome: target_node_id } }`                               | Branches on a tool call's outcome. |

A `node_type` containing a colon (e.g. `slack:send_message`) is a custom vendor
node — preserved losslessly but not interpreted by the core runtime.

## Running

`flow_run` executes every reachable `prompt` node as a one-shot task (tools
auto-approved), logging all turns to one flow-tagged diagnostics session you can
later inspect with `diagnostics_read`. Optionally pass `persona` (default
`coding-agent`) and `model`.

See the full spec for edge handles, versioning, and conformance:
`../metalcraft-flows/SPEC.md`.
