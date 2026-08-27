---
description: Wire format and workflow for authoring, scheduling and running metalcraft flows (spec v3)
---

# Authoring Flows

A flow is a JSON graph of nodes (`flows/<id>.json`) executed by the runtime. Edit
with `flow_write` (pass a `flow` document), read with `flow_read`, check with
`flow_validate`, run with `flow_run`. Start from a template with
`flow_templates_list` + `flow_template_read`.

**Always `flow_validate` before `flow_write`.** `flow_write` re-validates and
refuses to save an invalid flow, returning the errors.

**Use `spec_version: "3"`** — a state machine: shared state, conditional and
LLM-driven routing, effectors, and durable pause/resume. (v1 `entry`+`prompt`
flows still run for back-compat, but author new flows as v3.)

**A flow says only WHAT the work is.** When it runs is a separate document — see
*Scheduling* below. Writing a flow starts nothing.

## Document shape (`SavedFlow`)

```json
{
  "spec_version": "3",
  "id": "triage",
  "name": "Triage",
  "created_at": "2026-01-01T00:00:00Z",
  "updated_at": "2026-01-01T00:00:00Z",
  "flow": {
    "nodes": [
      { "id": "entry", "node_type": "entry", "data": {} },
      { "id": "work",  "node_type": "prompt", "data": { "prompt": "Summarize {{topic}}." } }
    ],
    "edges": [ { "id": "e1", "source": "entry", "target": "work" } ]
  }
}
```

- **id** — `^[A-Za-z0-9-]{1,64}$` (lowercase-hyphen).
- There is no `enabled` field and no schedule on the entry node. A flow runs when
  something runs it: `flow_run`, or a scheduled-flow document.
- Execution starts at the single `entry` node and follows edges by handle.

## Scheduling

`scheduled_flow_create { flow_id, schedule, enabled?, instance_id? }` says when a
flow runs. The `schedule` is `{ type: "cron"|"minutes"|"hours"|"manual", cron?,
interval?, name?, timezone?, persona?, inputs? }`.

```json
{ "flow_id": "triage", "schedule": {
    "name": "Morning triage", "type": "cron", "cron": "0 0 8 * * *",
    "timezone": "America/Detroit", "inputs": { "topic": "inbox" } } }
```

- **Cron takes six fields** (seconds first): `0 0 8 * * *` is 08:00 daily. A
  five-field POSIX expression is rejected.
- One flow can have **several** schedules — 8am and 6pm are two documents, each
  with its own inputs and persona.
- Creating one also creates the **persistent agent** it runs as, so successive
  firings remember each other. Schedules of one flow share that agent.
- `scheduled_flow_delete` stops the timer and **keeps** the agent and its memory.
  `scheduled_flow_update { id, enabled: false }` pauses without deleting.
- A `manual` schedule never fires by itself; it names the agent a hand-run
  resolves to.
- `scheduled_flow_list` is the complete answer to "what will this agent do on its
  own" — an empty list means nothing runs unless somebody asks.

## State & templating

A run carries one JSON `variables` object, threaded through every node:

- Declare typed invocation params on `entry.data.inputs`; they seed `variables`
  at run start. Pass them via `flow_run`'s `inputs`.
- `variables._last` is the payload of the edge just traversed — the current
  node's input. Nodes write their result to `_last` (and to `output_var` if set).
- Any string field (`prompt`, tool `args`, `http.url`, `set_variable.value`,
  `approval.message`, …) supports `{{name}}` / `{{name.path}}` interpolation
  from `variables`.

## Core node types (`node_type` + `data`)

| node_type      | data | Purpose / handles |
|----------------|------|-------------------|
| `entry`        | `{ inputs?: { name: { type, required?, default? } }, persona?, max_steps? }` | Start; seeds state. ≤1 per flow. |
| `prompt`       | `{ prompt, persona?, model?, output_var?, output_schema? }` | Run an agent. Stashes the answer (parsed as JSON if `output_schema` is set). Emits `ok` / `error`. |
| `conditional`  | `{ conditions: [{ handle, variable, operator, value? }], default_handle? }` | Deterministic routing: first matching predicate's `handle` wins, else `default_handle`. |
| `branch`       | `{ query, outputs: [{ handle, description?, schema?, var? }], persona?, model?, default_handle?, timeout? }` | LLM classifier: model picks exactly one output and fills its typed args → that edge's payload (`_last`). |
| `set_variable` | `{ variable, value? \| from? }` | Assign a literal/`{{template}}` `value`, or copy a dotted `from` path out of `_last`. |
| `tool`         | `{ tool_name, args?, output_var? }` | Call one registered tool directly (no agent loop). Emits `ok` / `error`. |
| `http`         | `{ method, url, headers?, body?, output_var? }` | Direct HTTP; result `{ status, body }`. Emits `ok` / `error`. |
| `sub_agent`    | `{ task, persona? \| tool_set?/pack?, output_var? }` | Delegate a subtask to a scoped sub-agent. Emits `ok` / `error`. |
| `approval`     | `{ message, choices?, timeout? }` | **Pauses** for a human decision; resume via the chosen handle. |
| `wait`         | `{ duration?: "30m"\|"2h"\|"1d" \| until?: RFC3339 }` | **Pauses** for a durable delay; resumes via `after`. |
| `end`          | `{ status?, outputs? }` | Explicit terminal. |

A `node_type` with a colon (e.g. `slack:send_message`) is a custom vendor node —
preserved but not executable by the core runtime.

## Edges & handles

Each edge routes from a node's named output: `{ "source": "n", "target": "m",
"source_handle": "ok" }`. **Every handle a node emits must have an edge** —
`conditional` condition handles + `default_handle`, `branch` output handles +
`default_handle`, and the `ok`/`error` of `prompt`/`tool`/`http`/`sub_agent`. An
edge with no `source_handle` is the unlabeled fallback.

**Conditional operators:** `equals, not_equals, contains, starts_with, ends_with,
gt, lt, exists, truthy, matches` (regex). `gt`/`lt` compare numerically. `value`
is typed JSON (`50`, not `"50"`).

## Running & pause/resume

`flow_run { id, persona?, model?, inputs? }` runs a v2 flow on the state-machine
executor and returns `{ run_id, status, steps, variables }`. `status` is
`completed` | `failed` | `paused`.

When `paused` (an `approval` or `wait` node), the run is checkpointed:
- `flow_run_status { run_id }` — inspect status / pause info / steps.
- `flow_runs_list` — list runs.
- `flow_resume { run_id, handle, data? }` — continue: pass the approval decision
  (or `after` for a wait). `wait` runs also auto-resume when their time arrives.

See the full spec (state, typed edge payloads, operators, versioning):
`https://docs.rs/metalcraft-flows` and `SPEC.md`.
