# Flows v2 — General-Purpose Stateful Flow Engine

**Status:** Draft plan
**Goal:** Upgrade metalcraft "flows" from a *stateless linear scheduler* into a
real *state machine* — shared state, conditional edges, cycles, and
checkpoint/pause-resume — modeled on the vix-automation flow engine but
**domain-agnostic** (no telephony primitives; nodes are agent/tool/integration
oriented).

---

## 0. Where we are vs. where we're going

### Current stack (v1)

| Layer | Crate / file | What it does today |
| --- | --- | --- |
| Wire format | `metalcraft-flows` 0.1.0 (`rust4ai/metalcraft-flows`, spec v1) | `SavedFlow` → `FlowDefinition` { nodes, edges }. Core node enum is a **closed set**: `Entry, Prompt, Branch, BranchTool`. `walk_bfs` ignores handles, visited-set for cycles. |
| Runtime | `metalcraft-agent/src/flows.rs` | `collect_reachable_prompts` BFS-collects **only** `Prompt` nodes and **errors** on `Branch`/`BranchTool`/`Custom`. `run_flow` runs each prompt as an **isolated one-shot** `run_one_shot_task`, sequential, **no state passed** between nodes, shared diagnostics session only. |
| Scheduling | `flows.rs::load_enabled_flows` + daemon | Entry node carries `schedule_type` (manual/minutes/hours/cron); daemon fires enabled flows. |
| Authoring | `src/tools/meta_flow.rs` + `seed/skills/authoring-flows.md` | Agent-facing `flow_write/read/validate/run/templates`. |
| Delegation | `src/tools/sub_agent.rs` | The one real "spawn a scoped agent" primitive; concurrent; persona-scoped. |

**Net:** the flow layer is *a scheduled bag of prompts fired in BFS order*.
Branch nodes exist in the schema but are rejected at runtime.

### Target semantics (borrowed from vix `walk_from_node` / `continue_flow`)

1. **Shared state** — a `variables` JSON object threaded through every node and
   **persisted** between steps.
2. **Conditional edges** — deterministic `conditional` (predicate → handle) plus
   LLM-driven `branch` routing, resolved by `source_handle`.
3. **Cycles** — bounded step loop (retries, loops, foreach).
4. **Checkpoint / pause-resume** — a node can pause the run, persist state, and
   resume later on an external signal.
5. **Effector nodes** — nodes that *do a thing* (call a tool, hit an HTTP API,
   set a variable, delegate to a sub-agent) rather than only "run an agent".

### Explicit non-goals

- **No telephony.** vix's `say/gather/record/transfer/hangup/send_sms` are
  replaced by generic effectors. The *only* thing we copy from vix is the
  execution machinery (state + handles + walk + pause/resume).
- Not a rewrite of `sub_agent` — it becomes one node type among many.
- Visual editor is out of scope for the core work (see §6, optional).

---

## 1. Gap analysis (v1 → target), mapped to vix

| Capability | vix has it via | metalcraft v1 | Work needed |
| --- | --- | --- | --- |
| Shared state | `variables` JSON (DB-persisted), `_edge_payload` | none — isolated one-shots | **New state model** in protocol + runtime |
| Read upstream output downstream | `set_variable`, handlebars `{{session.x}}` | none | **Templating/interpolation** + `output_var` on prompt |
| Deterministic branch | `branch` node (variable/operator/value → handle) | `Branch{condition:string}` opaque, **rejected at runtime** | **`conditional` node** (structured predicate) + handle-aware routing |
| LLM/tool routing | `tool_call` (OpenAI classifier → handle), `branch_tool` | `BranchTool` in schema, rejected | **`branch` node** (LLM classifier) / structured-output prompt |
| Effectors | `send_sms`, `webhook`, `create_appointment` | none (prompt only) | New core nodes: `tool`, `http`, `sub_agent`, `set_variable` |
| Cycles / retry | `max_steps` loop, `tool_call` retries | none (linear plan-then-run) | **Stepwise executor** |
| Pause / resume | DB `flow_executions` + Twilio webhook `continue_flow` | none | **Execution store** + resume triggers (§4) |
| Human-in-the-loop | phone caller | none | `approval` node (§4) |

---

## 2. Protocol rework — `metalcraft-flows` spec **v2**

We own the crate, so evolve it to `0.2.0` / `spec_version: "2"` rather than
bolting semantics onto the agent. Guiding rule: **v1 documents remain valid and
execute identically**; v2 is a strict superset.

### 2.1 State model (new)

Two new *optional* concepts enter the wire format. Note: the **running**
`variables` bag is runtime state (lives in the execution store, §4), NOT in
`SavedFlow`. What the wire format gains is the *declaration* and *references*.

**Entry node gains typed inputs** (so a flow can be invoked with arguments, not
just fired by a schedule):

```json
{ "id": "entry", "node_type": "entry",
  "data": {
    "schedule_type": "manual",
    "inputs": {                       // optional; declares invocation params
      "repo":   { "type": "string", "required": true },
      "since":  { "type": "string", "required": false, "default": "24h" }
    }
  } }
```

At run start the runtime seeds `variables` from `inputs` (invocation args or
defaults). Scheduled runs pass `{}`.

**A defined variable namespace** in the running state:

- `variables.<name>` — user variables (set by nodes).
- `variables._last` — the payload of the **edge just traversed** into this node
  (vix's `_edge_payload`); i.e. the current node's typed input. See §2.4.
- `variables._inputs` — the seeded entry inputs (immutable copy).
- `variables._run` — run metadata (id, started_at, step). Reserved, read-only.

### 2.2 Node type expansion

Redefine the core enum. **Keep** `Entry`, `Prompt`. **Rename + restructure** the
v1 deterministic `Branch` → **`conditional`**, and **reassign the name `branch`**
to the new LLM-classifier node (formerly proposed as `route`). **Add** effector +
control nodes. `BranchTool` is **deprecated** (kept parseable for v1 round-trip;
superseded by `conditional` + `branch`).

> **Wire-name collision note.** In v1 the wire name `branch` meant the opaque,
> non-executable `{condition: string}` stub. In v2 `branch` is **reassigned** to
> the LLM classifier (`{prompt, choices}`). This is safe because (a) it's gated
> by `spec_version: "2"`, (b) the v1 `branch` was never runnable, and (c) the two
> `data` shapes are disjoint, so validation rejects a v1-shaped `branch` in a v2
> doc rather than silently misreading it. Rust enum mapping:
> `CoreNodeType::Conditional` (was the deterministic `Branch`) and a new
> `CoreNodeType::Branch` (the classifier); `BranchTool` retained + deprecated.

| `node_type` | Kind | `data` schema (v2) | Purpose |
| --- | --- | --- | --- |
| `entry` | control | `{ schedule_type, interval?, cron?, inputs? }` | Start; seeds state. ≤1 per flow. |
| `prompt` | agent | `{ prompt, persona?, model?, tools?, output_var?, output_schema?, on_error? }` | Run an agent; interpolate `prompt`; stash final answer in `output_var`; optional structured output; emits `ok`/`error` handles. |
| `set_variable` | pure | `{ variable, value?, from? }` | Assign. `value` = literal/template; `from` = JSONPath into `_last`. (vix `set_variable`.) |
| `conditional` | control | `{ conditions: [{ handle, variable, operator, value }], default_handle? }` | **Deterministic** predicate routing → `source_handle`. (vix `branch`, generalized + renamed.) |
| `branch` | agent | `{ query, outputs: [{ handle, description, schema, var? }], persona?, model?, default_handle?, timeout? }` | **LLM classifier with typed outputs (locked as a dedicated node):** each `output` is a tool definition (name=`handle`, JSON-Schema `schema`); the model does forced tool-choice — picks exactly one handle **and fills its typed args**, which become that edge's payload (§2.4). `default_handle`/`timeout` fallback (vix retry+timeout+default). Generalizes vix `tool_call`. (Formerly proposed as `route`.) |
| `tool` | effector | `{ tool_name, args, output_var?, on_error? }` | Call **one registered tool directly** (no agent loop). Deterministic effector. Generalizes vix `send_sms`/`webhook`. |
| `http` | effector | `{ method, url, headers?, body?, output_var?, on_error? }` | Direct HTTP (reuses `HttpApiTool`/`call_webhook_inline` analog). |
| `sub_agent` | agent | `{ task, persona? \| tool_set?/pack?, output_var? }` | Wrap existing `SubAgentTool`; delegate, store result. |
| `approval` | pause | `{ message, choices?: [handle...], timeout? }` | **Human-in-the-loop**: pause, persist, resume on external decision → handle. |
| `wait` | pause | `{ duration \| until }` | Pause and resume later via scheduler (durable delay). |
| `foreach` | control | `{ list, item_var, mode: "sequential"\|"concurrent", body_entry }` | Fan-out over a list variable into a subgraph/handle. (Phase 3.) |
| `end` | control | `{ status?, outputs? }` | Terminal; may set flow outputs. Optional (falling off the graph also ends). |

Everything else stays a **custom vendor node** (`github:open_pr`) — but because
`tool`/`http` can invoke any registered integration tool generically, most
integrations need **no** custom node type anymore.

### 2.3 The `conditional` node (deterministic routing)

Replace v1's `Branch{condition:string}` (opaque) with vix's evaluable predicate,
under the new name `conditional`:

```json
{ "id": "check", "node_type": "conditional",
  "data": {
    "conditions": [
      { "handle": "urgent", "variable": "priority", "operator": "equals", "value": "P0" },
      { "handle": "has_owner", "variable": "assignee", "operator": "exists" }
    ],
    "default_handle": "default"
  } }
```

Operators (runtime-evaluated, deterministic):
`equals, not_equals, contains, starts_with, ends_with, gt, lt, exists, truthy, matches` (regex).
First matching condition wins; else `default_handle`; else fall through to a
single unnamed outgoing edge (v1 compat).

**Numeric coercion (hard requirement).** `variable` may address `_last` (the
incoming edge payload, §2.4) or a nested field (`_last.temp`, `triage.severity`).
`gt`/`lt` MUST compare **numerically** when both operands parse as numbers — vix's
`branch` compared everything as strings, so `"18" > "50"` was a lexicographic bug;
`eval.rs` must not repeat it. `value` is typed JSON (`50`, not `"50"`) where the
schema is numeric.

### 2.4 Typed output handles & edge payloads (dataflow)

Handles aren't just control-flow labels — **an output handle can be typed, and the
value produced for it rides the edge as that node's output → the next node's
input.** This makes the graph a *typed dataflow* graph on top of the global
`variables` bag.

Rules:

- A node's output handle MAY declare a `schema` (JSON Schema; scalar or object).
- When a handle is taken, its payload becomes the traversed **edge's payload**,
  delivered to the target node as `variables._last` (its input). Scalar schema →
  `_last` is the scalar; object schema → reach in via `_last.field`.
- Optionally persist it to a named variable (`var` / `output_var`) so it survives
  in the bag for later nodes or checkpointing — `_last` is only the immediate hop.
- Which nodes produce typed payloads:
  - **`branch`** — each `output` is a **tool definition** (`handle` = tool name,
    `schema` = its parameters). The LLM does forced tool-choice: it picks one
    handle **and fills its typed args**. Chosen handle routes the edge; the args
    are the edge payload. (This is vix `tool_call`, generalized: vix built
    `tool_defs` from `data.tools`, then wrote the chosen tool's args to
    `_edge_payload` and routed by tool name.)
  - **`prompt`** — `ok` carries the final answer (or parsed `output_schema`
    object); `error` carries the error string.
  - **`tool`/`http`** — `ok` carries the tool/HTTP result; `error` the failure.

Because handles are typed, `validate.rs` can **type-check edges** (best-effort):
if `report_temp`'s schema is `integer` and it feeds a `conditional` doing
`gt 50` on `_last`, that's coherent; wiring the `error` (string) edge into the
same numeric conditional is a flaggable mismatch.

**Worked shape** (the "temperature in Madrid" story):

```json
{ "id": "get_temp", "node_type": "branch",
  "data": {
    "query": "What is the temperature in Madrid right now?",
    "persona": "weather-agent",
    "outputs": [
      { "handle": "report_temp", "description": "got it", "schema": { "type": "integer" } },
      { "handle": "error",       "description": "failed", "schema": { "type": "string"  } }
    ],
    "default_handle": "error" } }
```
```json
{ "id": "check_hot", "node_type": "conditional",
  "data": { "conditions": [ { "handle": "hot", "variable": "_last", "operator": "gt", "value": 50 } ],
            "default_handle": "cold" } }
```
Edge `get_temp --report_temp--> check_hot` carries the `i64`; at `check_hot`,
`_last == 18`, `18 > 50` is false → `cold` handle. The `error` edge (string) would
instead route to an error-handling node with `_last` = the message.

### 2.5 Templating / interpolation (new, spec'd)

Define a single interpolation syntax for **string fields** (`prompt`, `tool`
`args`, `http.url/body`, `set_variable.value`, `approval.message`):

```
"Summarize commits in {{repo}} since {{since}}. Prior summary: {{_last.summary}}"
```

- `{{name}}` / `{{name.path.to.field}}` → resolved from `variables` (JSON path).
- Missing → empty string (vix behavior) OR error in strict mode (flag).
- Reference implementation: a `metalcraft_flows::template::resolve(tmpl, vars)`
  helper mirroring vix `resolve_handlebars` (which already supports
  `session.` / `edge.` scopes — we generalize to the `variables` namespace).

### 2.6 Edge handles become first-class

Today `walk_bfs` **ignores** handles. v2 adds a handle-aware resolver:

```rust
// new in metalcraft-flows
pub fn next_by_handle(def: &FlowDefinition, source: &str, handle: Option<&str>) -> Option<String>;
```

Semantics (copied from vix `get_next_node_from_edges`): prefer an edge whose
`source_handle == handle`; fall back to an edge with no handle. Nodes that emit
named handles: `conditional` (condition handles + default), `branch` (typed
output handles, §2.4), `prompt`/`tool`/`http` (`ok`/`error`), `approval`
(decision handles).

### 2.7 Crate changes (`metalcraft-flows` 0.1 → 0.2)

- `model.rs` — extend `CoreNodeType` with the new variants; keep `from_wire`
  back-compat (`branch_tool` still parses). `FlowNodeType::Deserialize` already
  degrades unknown bare types to `Custom`, so **old parsers reading v2 docs**
  treat new nodes as custom and refuse to execute — safe.
- `validate.rs` — per-node-type `data` schema validation (conditional conditions
  shape, prompt fields, `branch` `outputs` schemas, handle/edge coherence: every
  emitted handle needs an edge; referenced `variable`s SHOULD exist;
  unreachable-node warnings; **best-effort edge type-checking** between a handle's
  declared `schema` and how the target consumes `_last`, §2.4).
- `walk.rs` — add `next_by_handle` + a `step`-oriented helper; keep `walk_bfs`
  for validation/reachability.
- **New** `state.rs` — `Variables` newtype + JSON-path get/set + reducer for
  `_last` (the edge payload).
- **New** `eval.rs` — operator evaluation for `conditional`, incl. **numeric
  coercion** for `gt`/`lt` (§2.3).
- **New** `template.rs` — `{{…}}` resolver.
- `SPEC.md` → bump to v2; document §2.1–§2.6; add examples; bump conformance.
- `CHANGELOG.md` — v2 is additive except the `branch` wire-name reassignment and
  the new `conditional` `data` shape (both gated by `spec_version`).

### 2.8 Versioning & compatibility

- `spec_version: "2"` for docs using new nodes/state. `"1"` still parses/runs.
- A v1 linear `entry→prompt→prompt` flow runs **identically** under the new
  executor (prompts fire in order; no `output_var` → nothing stored).
- The only behavior change for old docs: a v1 doc that (illegally today)
  contained `branch`/`branch_tool` nodes was *rejected* and stays rejected under
  v1 semantics. A v2 doc's `branch` is the new classifier (disjoint `data`
  shape), so there's no silent reinterpretation — see the collision note in §2.2.
  Deterministic routing in v2 is the new `conditional` node.

---

## 3. Runtime rework — `metalcraft-agent`

Replace the **plan-then-run** model (`collect_reachable_prompts` + `run_flow`)
with a **stepwise executor** analogous to vix `walk_from_node`.

### 3.1 `FlowExecutor`

```rust
struct FlowExecutor<'a> {
    ctx: &'a AgentRuntimeContext,
    def: FlowDefinition,
    run_id: Uuid,
    variables: serde_json::Value,     // the shared state
    current: String,                  // current node id
    step_budget: u32,                 // e.g. 100, prevents runaway cycles
    logger: Option<Arc<DiagnosticsLogger>>,
    persona_slug: String,             // flow-level default
    model_name: String,
    approval_mode: ApprovalMode,
}

enum StepOutcome {
    Goto(String),                     // route to node id (conditional/branch)
    Continue(Option<String>),         // pass-through done → next by handle
    Pause { reason: PauseReason, resume_handles: Vec<String> },
    Complete { status: String },
    Fail { node: String, error: String },
}
```

Loop: load `current` node → dispatch on `node_type` → apply `StepOutcome`.
`Pause`/`Complete`/`Fail` persist and return; `Goto`/`Continue` advance. Bounded
by `step_budget` (vix's `max_steps` generalized).

### 3.2 Node executors

- **entry** — seed `variables` from `inputs`; `Continue(None)`.
- **prompt** — interpolate `prompt`; run `run_one_shot_task` with effective
  persona; on success store final answer into `output_var` (+ `_last`), parse
  `output_schema` if present, `Continue("ok")`; on failure `Continue("error")`
  or `Fail` if no error edge. (Reuses today's `run_flow` inner logic per node.)
- **set_variable** — pure assign from `value`/`from`; `Continue(None)`.
- **conditional** — evaluate conditions via `eval.rs` (reading `_last` / named
  vars, numeric-aware) → `Goto(edge for handle)`.
- **branch** — forced tool-choice over the typed `outputs`: LLM picks one handle
  **and produces its typed args**; set `_last` (and `var` if declared) to those
  args, then `Goto`. This is vix `tool_call`, domain-neutral (§2.4).
- **tool** — resolve a single tool from the registry, interpolate `args`, call
  it directly (no agent loop), store `output_var`; `Continue("ok"/"error")`.
- **http** — direct request (reuse `HttpApiTool` / webhook validation incl.
  SSRF guard already in vix `validate_webhook_url`).
- **sub_agent** — call existing `SubAgentTool` path; store result.
- **approval / wait** — return `Pause` (see §4).
- **foreach** — Phase 3; iterate `list`, run body per item (sequential, or
  concurrent reusing the sub-agent concurrency cap).
- **end** — `Complete` with optional `outputs` copied out of `variables`.

### 3.3 State threading & interpolation

- `variables` starts from entry inputs; each node reads/writes it.
- `_last` set to each node's primary output.
- All string fields resolved through `template::resolve(field, &variables)`
  immediately before use.
- Structured-output prompts (`output_schema`) enable **downstream branching on
  fields** — e.g. prompt returns `{severity, needs_human}`, a `conditional` routes
  on `variables.triage.needs_human`.

---

## 4. Checkpoint / pause-resume — generalized beyond telephony

This is the heart of vix (`flow_executions` + Twilio webhook `continue_flow`).
We keep the *mechanism*, swap the *transport*.

### 4.1 Execution store (new persisted entity)

```
FlowRun {
  id, flow_id,
  status: running | paused | completed | failed,
  current_node_id,
  variables: Value,            // the checkpoint
  pause: Option<{ reason, resume_handles, wake_at? }>,
  created_at, updated_at,
}
```

**Decision (locked): fs `runs/` store.** One JSON per run under a `runs/`
directory, mirroring the existing `flows/` fs backend and `paths::flows_dir()` —
zero new dependencies, consistent with today's file-based storage. Run-history
querying is weaker than a DB; revisit SQLite only if/when we need queryable run
history at scale.

`update_execution` (checkpoint) and `complete_execution` map straight from vix,
writing/removing `runs/{run_id}.json`.

### 4.2 Resume triggers (transport-agnostic — the key generalization)

vix has exactly one resume path: a phone caller hitting a Twilio webhook. We
generalize to several:

| Pause node | Resumed by | Handle chosen from |
| --- | --- | --- |
| `approval` | CLI `flow_resume <run_id> <decision>`, workshop UI button, or a `flow_resume` tool | the decision |
| `wait` | the **scheduler daemon** — extend its tick to also scan `runs/` for `paused` runs whose `wake_at` is due | `after` |
| `wait_for_event` (opt.) | inbound workshop API endpoint `/flows/runs/{id}/signal` (the domain-neutral analog of the Twilio callback) | event name |

So "external input" is no longer "the caller pressed 1" — it's a human
approving in the workshop, a durable timer firing, or a webhook/event arriving.
Same persist-return-resume loop, no telephony.

### 4.3 Daemon integration

The daemon already loads and fires **enabled scheduled flows**. Extend it to
also, each tick: load `paused` runs, resume any whose `wake_at ≤ now`
(`wait` nodes) — reusing the same `FlowExecutor::resume(run_id)` entry point the
CLI/API use.

---

## 5. Agent-facing tooling & skills (`meta_flow.rs`, seed skills)

- `flow_validate` — validate v2 node `data` schemas + handle/edge coherence.
- `flow_run` — returns `{ run_id, status }`; status may be `paused`.
- **New** `flow_resume { run_id, handle, data? }` — supply a human decision /
  signal.
- **New** `flow_run_status { run_id }` and `flow_runs_list { flow_id? }`.
- `flow_templates` — add stateful examples (conditional, branch, approval, tool effector).
- `authoring-flows.md` skill — rewrite for v2: state model, node table,
  interpolation, a worked stateful example.
- `SPEC.md` (crate) — v2.

---

## 6. Visual editor (optional, later)

The wire format was designed for a visual editor (node `position`, handles).
vix already ships a React-Flow editor (`frontend/src/components/flow/*`,
`FlowEditor.tsx`, `NodePalette.tsx`) with per-node-type config panels. If/when we
want a GUI, **port that editor into `metalcraft-workshop`**, driving it off the
v2 node catalog. Not required for the engine to work (the agent authors flows as
JSON via `meta_flow`). Track as a separate initiative.

---

## 7. Phased rollout

**Phase 1 — Stateful linear + effectors (no branching).**
Crate: add `set_variable`, `tool`, `http`, `sub_agent` core types + `state.rs` +
`template.rs`. Runtime: `FlowExecutor` replacing `run_flow`, threading
`variables`, `output_var`, interpolation. v1 flows run unchanged. *Ships value
immediately: prompts can now pass data to later prompts/tools.*

**Phase 2 — Conditional routing + cycles.**
`conditional` + `branch`, `next_by_handle`, `eval.rs`, handle-aware edges,
step budget. Now "run B only if A found X" is expressible at flow level.

**Phase 3 — Durability (pause/resume).**
Execution store (`runs/`), `approval` + `wait` nodes, `flow_resume` /
`flow_run_status` tools, daemon resume of due `wait`s. Human-in-the-loop.

**Phase 4 — Fan-out + editor (optional).**
`foreach`/`join`; port the vix React-Flow editor into workshop.

Each phase is independently shippable and back-compatible.

---

## 8. Risks & open questions

- **Crate ownership / release** — v2 is a breaking-ish `metalcraft-flows` bump;
  coordinate `0.2.0` publish + agent `Cargo.toml`. (You own `rust4ai/…`.)
- ~~State store location~~ — **decided: fs `runs/` store** (§4.1). Revisit
  SQLite only if queryable run history becomes a need.
- ~~`route`/LLM branching shape~~ — **decided: dedicated `branch` node** (the
  LLM classifier; renamed from `route`) with a strict "pick exactly one handle"
  contract + `default_handle`/`timeout` fallback (vix retry+timeout+default is
  the template). Determinism of the classifier itself remains the thing to nail
  down in implementation.
- **Interpolation safety** — templating into `tool`/`http` args is an injection
  surface; keep `validate_webhook_url`-style guards and consider a strict mode.
- **Approval UX** — where humans approve (CLI vs workshop) determines how much
  Phase 3 depends on workshop work.
- **Concurrency semantics for `foreach`** — reuse sub-agent cap; define
  join/error aggregation before building.

---

## Appendix — worked v2 example (conditional + effector + approval)

```json
{
  "spec_version": "2",
  "id": "triage-and-notify",
  "name": "Triage issue and notify",
  "enabled": false,
  "flow": {
    "nodes": [
      { "id": "entry", "node_type": "entry",
        "data": { "schedule_type": "manual",
                  "inputs": { "issue_url": { "type": "string", "required": true } } } },

      { "id": "triage", "node_type": "prompt",
        "data": { "prompt": "Read {{issue_url}} and classify it.",
                  "persona": "github-agent",
                  "output_var": "triage",
                  "output_schema": { "severity": "string", "needs_human": "boolean" } } },

      { "id": "sev_check", "node_type": "conditional",
        "data": { "conditions": [
                    { "handle": "p0", "variable": "triage.severity", "operator": "equals", "value": "P0" } ],
                  "default_handle": "normal" } },

      { "id": "ask_human", "node_type": "approval",
        "data": { "message": "P0 issue {{issue_url}} — page on-call?",
                  "choices": ["yes", "no"] } },

      { "id": "page", "node_type": "tool",
        "data": { "tool_name": "discord_send_message",
                  "args": { "channel": "oncall", "text": "P0: {{issue_url}}" } } },

      { "id": "log_normal", "node_type": "set_variable",
        "data": { "variable": "outcome", "value": "filed:{{triage.severity}}" } }
    ],
    "edges": [
      { "id": "e0", "source": "entry",    "target": "triage" },
      { "id": "e1", "source": "triage",   "target": "sev_check", "source_handle": "ok" },
      { "id": "e2", "source": "sev_check","target": "ask_human", "source_handle": "p0" },
      { "id": "e3", "source": "sev_check","target": "log_normal","source_handle": "normal" },
      { "id": "e4", "source": "ask_human","target": "page",      "source_handle": "yes" }
    ]
  }
}
```

This single flow uses: typed inputs, a structured-output prompt, a deterministic
`conditional` on a nested field, a human-approval pause/resume, and a direct tool
effector — none of which the v1 runtime can execute today.

---

## Appendix B — Authoring walkthrough: the Madrid weather flow

The canonical user story, authored end to end. A `branch` node runs the
`weather-agent` persona; the LLM resolves the query and terminates by calling one
of two typed output handles; the typed payload flows down the chosen edge into a
`conditional` that compares it numerically.

```json
{
  "spec_version": "2",
  "id": "madrid-weather",
  "name": "Madrid weather check",
  "created_at": "2026-07-27T00:00:00Z",
  "updated_at": "2026-07-27T00:00:00Z",
  "enabled": false,
  "flow": {
    "nodes": [
      { "id": "entry", "node_type": "entry",
        "data": { "schedule_type": "manual" } },

      { "id": "get_temp", "node_type": "branch",
        "data": {
          "query": "What is the temperature in Madrid right now, in °F?",
          "persona": "weather-agent",
          "outputs": [
            { "handle": "report_temp",
              "description": "Temperature was determined successfully",
              "schema": { "type": "integer", "description": "temperature in °F" } },
            { "handle": "error",
              "description": "Could not determine the temperature",
              "schema": { "type": "string", "description": "what went wrong" } }
          ],
          "default_handle": "error"
        } },

      { "id": "check_hot", "node_type": "conditional",
        "data": {
          "conditions": [
            { "handle": "hot", "variable": "_last", "operator": "gt", "value": 50 }
          ],
          "default_handle": "cold"
        } },

      { "id": "say_hot",  "node_type": "prompt",
        "data": { "prompt": "Tell the user it's warm in Madrid: {{_last}}°F." } },
      { "id": "say_cold", "node_type": "prompt",
        "data": { "prompt": "Tell the user it's chilly in Madrid." } },
      { "id": "handle_err", "node_type": "prompt",
        "data": { "prompt": "Report that the weather lookup failed: {{_last}}" } }
    ],
    "edges": [
      { "id": "e0", "source": "entry",     "target": "get_temp" },
      { "id": "e1", "source": "get_temp",  "target": "check_hot",  "source_handle": "report_temp" },
      { "id": "e2", "source": "get_temp",  "target": "handle_err", "source_handle": "error" },
      { "id": "e3", "source": "check_hot", "target": "say_hot",    "source_handle": "hot" },
      { "id": "e4", "source": "check_hot", "target": "say_cold",   "source_handle": "cold" }
    ]
  }
}
```

**How `get_temp` resolves (the branch mechanics).** The executor builds a one-shot
agent as the `weather-agent` persona, whose registry contains **both** the
persona's real tools (so it can actually look up the weather) **and** two
synthetic "handle tools" — `report_temp` (params = `integer`) and `error`
(params = `string`) — marked as `terminal_tools`, with `tool_choice: Required`.
The model gathers the temperature (calling the persona's weather tool), then ends
the turn by calling exactly one handle tool with its typed argument, e.g.
`report_temp(18)`. The executor reads that terminal `ToolCall { name, args }`:
`name` → the taken edge (`report_temp`), `args` → the edge payload (`18`), which
becomes `_last` at the next node.

**Trace (warm-day path):** `entry` → `get_temp` (LLM calls `report_temp(18)`) →
edge `report_temp` carries `18` → `check_hot` sees `_last == 18`, `18 > 50` is
false → `cold` handle → `say_cold`. On a lookup failure the model calls
`error("no data for Madrid")`; the `error` edge carries the string to
`handle_err`, which prints it via `{{_last}}`.

**Design note.** Because "temperature in Madrid" is a factual lookup, an equally
valid authoring is `tool` (weather API) → `conditional` — no LLM in the decision
path, fully deterministic. The `branch` version is right here because we want the
model to *decide success vs. error* and emit the typed result; the typed-payload
mechanism is identical either way.
