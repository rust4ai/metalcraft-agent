# Flows Architecture (v2)

How the metalcraft flow system works, end to end. This is the "how it works"
companion to the two planning docs (`FLOWS_V2_STATE_MACHINE_PLAN.md`,
`FLOWS_V2_IMPLEMENTATION_PLAN.md`).

A **flow** is a serializable graph that describes an agent workflow. v2 turns it
from a *scheduled list of prompts* into a real **state machine**: shared state,
conditional edges, cycles, typed dataflow, and (planned) pause/resume.

---

## 1. Two layers

The system is split across two crates with a clean seam:

| Layer | Crate | Responsibility |
| --- | --- | --- |
| **Wire format** | [`metalcraft-flows`](https://crates.io/crates/metalcraft-flows) (v0.2, spec v2) | The serializable format + pure helpers: types, validation, predicate eval, `{{…}}` templating, handle-aware routing, state model. **No LLM, no I/O.** |
| **Runtime** | `metalcraft-agent` (`src/flow_exec.rs`) | The `FlowExecutor` that *runs* a flow: threads state, dispatches nodes, calls the LLM/tools, routes by handle. |

The crate is the shared, publishable contract (a visual editor, the agent, and
any third-party runtime all agree on it). The agent is one runtime that
interprets it. The crate stays pure so its logic is unit-testable without a model.

---

## 2. The wire format (`metalcraft-flows`)

A saved flow is one JSON document — a `SavedFlow`:

```json
{
  "spec_version": "2",
  "id": "triage-and-notify",
  "name": "Triage and notify",
  "enabled": false,
  "flow": { "nodes": [ … ], "edges": [ … ] }
}
```

- **Nodes** — `{ id, node_type, data, position }`. `data` is a free-form object
  whose schema depends on `node_type` (typed views live in `nodes.rs`).
- **Edges** — `{ id, source, target, source_handle?, target_handle? }`. Directed;
  `source_handle` names *which* output of a multi-output node the edge leaves.
- **Cycles are allowed**; the runtime bounds them with a step budget.

### Versioning

`spec_version` gates the vocabulary. `"1"` is the original linear format; `"2"`
adds the state machine. A document that omits the field defaults to `"1"`
(back-compat), so **v2 nodes require `spec_version: "2"` explicitly**. Both
versions parse and validate; validation rejects a v2 node type used in a v1
document.

---

## 3. The execution model (`FlowExecutor`)

`FlowExecutor::run` is a stepwise walk — the heart of the state machine:

```
current = entry node
loop (bounded by step budget):
    node   = lookup(current)
    route  = run_node(node).await     // dispatch on node_type
    match route:
        End(status)   -> finish
        Handle(h)     -> current = next_by_handle(def, current, h)
                         (no matching edge -> finish "completed")
```

Three things flow through every run:

- **State** — a single `variables` JSON object (`state::Variables`). Nodes read
  and write it. Reserved keys:
  - `_last` — the payload of the **edge just traversed** into this node (its
    typed input).
  - `_inputs` — an immutable copy of the entry inputs the run was seeded with.
- **Handles** — a node returns a `Route::Handle(Some("hot"))` and the executor
  follows the edge whose `source_handle == "hot"`, falling back to an unlabeled
  edge. This is how `conditional` and `branch` route.
- **A trace** — each step appends a `FlowStep { node_id, node_type, outcome }` so
  a run yields a `FlowRunSummary { status, steps, variables }` for inspection.

### State seeding

The entry node may declare typed `inputs`; at run start the executor seeds
`variables` from the invocation `args` (or per-input defaults) and errors on any
missing required input. A scheduled run passes `{}`.

---

## 4. Node catalog

Every node is one of these. "Kind" describes how the executor treats it.

| `node_type` | Kind | What it does | Status |
| --- | --- | --- | --- |
| `entry` | control | Start; seed state from typed `inputs`. ≤1 per flow. | ✅ |
| `set_variable` | pure | Assign a literal/`{{template}}` value, or copy a field out of `_last` via `from`. | ✅ |
| `conditional` | control | **Deterministic** routing: first matching predicate wins → its handle. | ✅ |
| `branch` | agent | **LLM classifier** with typed outputs (see §6). | ✅ |
| `prompt` | agent | Run an agent one-shot; store answer in `output_var`/`_last`; `ok`/`error` handles. | ✅ |
| `tool` | effector | Call one registered tool directly (no agent loop); `ok`/`error`. | ✅ |
| `http` | effector | Direct HTTP request. | ⏳ planned |
| `sub_agent` | agent | Delegate a scoped subtask. | ⏳ planned |
| `approval` | pause | Human-in-the-loop checkpoint. | ⏳ phase 3 |
| `wait` | pause | Durable delay. | ⏳ phase 3 |
| `foreach` | control | Fan out over a list. | ⏳ phase 3 |
| `end` | control | Explicit terminal; may publish outputs. | ✅ |

Custom `vendor:name` nodes are preserved but not executed by this runtime.

### `conditional` operators

Predicates read a dotted path (`_last`, `triage.severity`) and compare with an
operator: `equals, not_equals, contains, starts_with, ends_with, gt, lt, exists,
truthy, matches` (regex). `gt`/`lt` compare **numerically** when both sides are
numbers — a deliberate fix for the string-compare bug where `"18" > "50"`.

---

## 5. Typed dataflow (edge payloads)

Handles aren't just control-flow labels — **an output handle can be typed, and
the value produced for it rides the edge as the next node's input.** This layers
a typed dataflow graph on top of the global `variables` bag:

- A node's output handle may declare a JSON-Schema `schema` (scalar or object).
- When taken, its payload becomes the traversed **edge's payload**, delivered to
  the target as `_last`. Scalar → `_last` is the scalar; object → `_last.field`.
- It can also be persisted to a named variable for durability/later reference.

`_last` is the immediate hop-to-hop handoff; named variables are the persistent
state. (Same split as the vix engine's `_edge_payload` vs `variables`.)

---

## 6. The `branch` mechanic (LLM classifier with typed outputs)

`branch` is the one genuinely novel node. It runs an agent whose **terminal
action is choosing one of N typed output handles**:

1. Build a tool registry = the node persona's tools **+** one synthetic
   `HandleTool` per output (its parameters are the output's `schema`).
2. Run a **tool-only** agent (`ToolChoice::Required`) with the handle names as
   `terminal_tools`, so the turn ends the moment the model calls one.
3. The model may first call real tools (e.g. a weather lookup), then calls
   exactly one handle tool with typed arguments.
4. Read the terminal `ToolCall{name, args}`: `name` = the chosen handle (routes
   the edge), `args` = the payload (→ `_last`, and the output's `var`).

Scalar output schemas (`{"type":"integer"}`) are auto-wrapped as
`{"type":"object","properties":{"value":…}}` because LLM function parameters must
be an object, then unwrapped on the way out. A `default_handle`/`timeout`
provides the fallback if no valid choice is made.

This generalizes the vix `tool_call` node: same "LLM picks the edge and produces
the payload" machinery, with the telephony stripped out.

---

## 7. Worked example — the "temperature in Madrid" flow

```
entry
  └─▶ get_temp  (branch: "look up Madrid's temp, then report it")
        ├─ report_temp (integer) ─▶ check_hot (conditional: _last > 50)
        │                              ├─ hot  ─▶ say_hot
        │                              └─ cold ─▶ say_cold
        └─ error (string) ──────────▶ handle_err
```

Trace for a cold day: `get_temp` runs the agent → it calls the weather tool →
gets `18` → calls `report_temp(18)`. The `report_temp` edge carries `18` →
`_last == 18` at `check_hot` → `18 > 50` is false → `cold` → `say_cold`. On
failure the model calls `error("…")`, routing the string to `handle_err`.

This exact flow is proven end-to-end against a real model in
`tests/flow_branch_llm.rs` (with a **mock** weather tool, gated on
`OPENAI_API_KEY`).

---

## 8. How it relates to the rest of metalcraft

- **`prompt` / `branch` / `sub_agent` reuse the agent runtime** — each is a real
  ReAct agent turn (`run_one_shot_task` / `create_react_agent_with_options`),
  driven by a **persona** (its system prompt + resolved tools + skills). A node
  can override the persona; otherwise the flow-level default applies.
- **`tool` nodes reuse the tool registry** — any registered tool (including
  integration-pack HTTP tools) is callable directly, so most integrations need no
  custom node type.
- **Lineage** — the `metalcraft` core crate is a LangGraph-style graph engine;
  flows sit *above* it as an author-facing state machine. The design borrows the
  execution model (shared state + conditional edges + checkpointing) from the
  vix-automation voice engine, generalized away from telephony.

---

## 9. Status & roadmap

**Built** (crate v0.2 + agent branch `flows-v2`):
- Wire format v2: node catalog, structured `conditional`, typed `branch` outputs,
  `{{…}}` templating, numeric-aware eval, handle routing, state model, validation.
- `FlowExecutor` with runners for `entry`, `set_variable`, `conditional`,
  `prompt`, `tool`, `branch`, `end`.
- Unit tests (pure routing/state) + a live LLM end-to-end test.

**Pending**:
1. **Wiring** — point `flow_run` (`meta_flow.rs`), the daemon (`daemon.rs`), and
   the workshop endpoint at `FlowExecutor`, replacing the legacy linear path.
2. **Remaining runners** — `http`, `sub_agent`.
3. **Phase 3 durability** — an fs `runs/` execution store, `approval`/`wait`
   pause-resume, and `flow_resume` / `flow_run_status` tools.
4. **Editor** — optionally a visual node editor in the workshop.

Until wiring lands, v2 flows run via `FlowExecutor` directly (and in tests); the
legacy `flows::run_flow` still serves v1 linear flows.
