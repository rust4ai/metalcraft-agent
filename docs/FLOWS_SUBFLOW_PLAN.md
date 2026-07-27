# Flows v2 — `subflow` node (nest a flow as a node)

**Status:** Plan only — not implemented.
**Goal:** Let a flow invoke another saved flow as a single node, so flows compose.
This is the metalcraft analog of LangGraph **subgraphs**, and (per the
architecture audit) the highest-value next feature: cheap, reuses the whole
`FlowExecutor`, and adds real composition without touching the single-token core.

---

## 0. Why

Today the only nesting primitives are `sub_agent` (spawns a *ReAct agent*, not a
flow) and inlining everything into one graph. There's no way to package a
reusable flow (e.g. "triage-and-notify") and call it from several parents. A
`subflow` node fixes that:

- **Reuse** — author a flow once, call it from many flows.
- **Encapsulation** — the child runs in its *own* `variables` state, seeded only
  by declared inputs and returning only declared outputs. No shared-bag leakage.
- **Testability** — a subflow can be run and tested standalone.

Contrast with LangGraph subgraphs: same intent. LangGraph shares state channels
across parent/child (with key mapping); we deliberately keep child state
**isolated** (inputs in, outputs out), which fits our single-token model.

---

## 1. Wire format (`metalcraft-flows`)

Add a core node type. This is a `metalcraft-flows` change → **bump to 0.2.1**
(additive: old parsers already degrade an unknown bare `node_type` to `Custom`,
so they round-trip it; only runtimes that execute it need the new version).

**`CoreNodeType::Subflow`** (wire `"subflow"`), added to `model.rs` `as_str` /
`from_wire` / `is_v2` (it's v2). Typed view in `nodes.rs`:

```rust
/// `data` for a `subflow` node.
pub struct SubflowData {
    /// The saved flow to invoke (its `id`).
    pub flow_id: String,
    /// Inputs to seed the child with. Values support `{{…}}` interpolation
    /// against the *parent's* state; keys must match the child entry's
    /// declared `inputs`.
    #[serde(default)]
    pub inputs: Option<serde_json::Value>,
    /// Where to store the child's result in the parent (also becomes `_last`).
    #[serde(default)]
    pub output_var: Option<String>,
}
```

**Handles:** emits `ok` / `error` (like other effectors), so the parent can
branch on child success/failure.

**Validation additions** (`validate.rs`, crate) — best-effort:
- `flow_id` non-empty.
- (advisory, agent-side `lint_flow`) referenced `flow_id` exists on disk; static
  self/mutual recursion warning where detectable.

---

## 2. What crosses the boundary

**In:** the `subflow.inputs` object is interpolated against the parent's
`variables` (reusing `interpolate_value`, so types are preserved), then passed as
the child's invocation `args`. The child seeds its state via the existing
`Variables::seed_from_inputs` against its own entry `inputs` — so a missing
required child input is a clean error routed to the parent's `error` handle.

**Out:** the child's result is whatever its terminal `end` node publishes via
`EndData.outputs` (already in the model), falling back to the child's `_last` if
no `end.outputs`. That value becomes the parent's `_last` and, if set,
`output_var`. **The child's full `variables` bag does NOT leak into the parent** —
only its declared/returned output.

```
parent: … → [subflow flow_id=triage inputs={item:"{{ticket}}"} output_var=triage_result] → (ok) …
child (triage flow): entry(inputs.item) → … → end(outputs={severity, summary})
                                              ↑ becomes parent's _last / triage_result
```

---

## 3. Runtime (`flow_exec.rs`)

New `run_subflow(&mut self, node)`:

1. Parse `SubflowData`; interpolate `inputs` against `self.variables`.
2. Load the child flow (`metalcraft_flows::load_flow`). Missing → `error` route
   with a clear message.
3. **Depth guard** (see §4). Exceeded → `error`.
4. Build a child `FlowExecutor::new(context, child_flow, cwd, default_persona,
   model, &inputs, logger)` — reuse the parent's context/persona/model/logger.
   Thread `extra_tools` down (tests inject mocks through the tree).
5. `child.run().await`:
   - `completed` → extract child output (`end.outputs` or `_last` from the child
     summary's `variables`), set parent `_last`/`output_var`, route `ok`.
   - `failed` → set `_last` to a failure summary, route `error`.
   - `paused` → **§5** (nested pause). Phase 1: treat as `error` with an explicit
     "subflows cannot pause yet" message.

Signature note: `FlowExecutor::run` currently consumes `self`; `run_subflow`
needs to build and run a *child* executor while the parent's `&mut self` is
borrowed — fine, the child is a separate owned value. The child's `FlowRunSummary`
carries `variables`, from which the parent reads the child's `end` outputs
(the drive loop already threads `end.outputs` into the summary variables, or we
add a dedicated `outputs` field to `FlowRunSummary`).

---

## 4. Recursion / budget

- **Depth counter** on `FlowExecutor` (`subflow_depth: u32`, default 0), passed to
  each child as `parent_depth + 1`. Cap at a constant (e.g. `MAX_SUBFLOW_DEPTH =
  8`); exceeding routes `error`. Prevents A→B→A blowups.
- **Step budget** is per-executor today. Keep child budgets independent (each
  child gets its own from its entry `max_steps` / default) — the parent's budget
  is unaffected by child steps. Total work is still bounded by depth × per-flow
  budget.
- Add a runtime cycle note: static cycle detection across flows is out of scope;
  the depth cap is the backstop.

---

## 5. The hard part — pause/resume through subflows

A child that hits an `approval`/`wait` must suspend the **whole tree**, and resume
must re-enter the deepest paused child, finish it, then pop back up.

**Phase 1 (ship first): pause-free subflows.**
If a child pauses, the parent's `run_subflow` routes `error` with
`"subflow '<id>' paused; nested pause is not supported"`. Simple, correct, and
covers the common case (pure-logic subflows). Document the limitation.

**Phase 2 (later): a run stack.**
Generalize the `FlowRun` checkpoint into a **stack of frames**, innermost last:

```
FlowRun {
  …,
  frames: Vec<Frame>,   // NEW; each = { flow snapshot, variables, current_node_id, steps }
  pause: PauseInfo,     // the deepest frame's pause
}
```

- On a nested pause: persist a frame per level (child's pause + each ancestor's
  "waiting at the subflow node" state), with the child's snapshot per frame
  (reusing §_flow-snapshot_ from 0.9.2).
- `resume_flow` pops to the deepest frame, resumes that child; on the child's
  completion, map its output into the parent frame's `_last`/`output_var` and
  continue the parent from *after* its subflow node; repeat until the stack
  drains or a frame pauses again.
- The workshop's pause/resume UI already renders `resume_handles` from the run's
  `pause`; with the stack it shows the deepest frame's handles plus a breadcrumb
  of the frame path. Minor UI addition.

Phase 2 is a bounded but real chunk (checkpoint schema + resume drill-down). Do
it only when a real flow needs to pause inside a reused subflow.

---

## 6. Observability

- Child turns log into the parent's flow-tagged diagnostics session (pass the
  parent `logger` down), so one session shows the whole tree. Optionally prefix
  child steps in the parent trace (`FlowStep` for the subflow node carries the
  child's `run_id` in `detail`, and the child's own steps are visible via its
  summary).
- `FlowRunSummary` for a parent already lists the `subflow` node as one step
  (`routed:ok`); consider embedding the child summary in `detail`.

---

## 7. Editor (`metalcraft-workshop`)

- `V2Node.tsx`: register `subflow` (accent color, `ok`/`error` handles, summary =
  the target `flow_id`).
- `FlowsView.tsx`: add `subflow` to `ADDABLE_NODE_TYPES` + `NODE_KIND_COMPONENTS`;
  a `SubflowFields` panel = a flow picker (dropdown from `list_flows`) + an
  inputs key/value editor + `output_var`. `defaultDataFor("subflow")` →
  `{ flow_id: "", inputs: {} }`.
- Nice-to-have: a "open child flow" affordance that navigates to the referenced
  flow in the editor.

---

## 8. Phasing

1. **Phase 1 — pause-free subflows (MVP).** Crate 0.2.1 (`Subflow` type +
   `SubflowData`); `run_subflow` with input mapping, output extraction, depth
   guard, `ok`/`error`; child pause → `error`. Tests: nested completed flow maps
   outputs; missing flow → error; depth cap; a child `end.outputs` surfaces to
   the parent. A live-LLM test optional (pure-logic child needs none).
2. **Phase 2 — nested pause/resume.** Run-stack checkpoint + resume drill-down.
3. **Phase 3 — editor.** Workshop palette + `SubflowFields` + flow picker.

Each phase is independently shippable. Phase 1 alone delivers reuse +
encapsulation and is the bulk of the value.

---

## 9. Edge cases to get right

- **Missing / renamed child flow** at run time → `error` route, not a panic.
- **Child input validation failure** (missing required) → `error` route with the
  missing-input names.
- **Recursion** (self or mutual) → depth cap → `error`.
- **Child failure** → parent `error` handle (respecting the 0.9.2 rule: an
  unwired parent `error` fails the parent run loudly).
- **Type fidelity** — inputs go through `interpolate_value` (whole-value `{{x}}`
  keeps its type); outputs are raw JSON, no stringification.
- **`extra_tools`** must thread through the whole tree so branch-in-subflow tests
  can inject mocks.
- **Determinism** — a pure-logic subflow (`conditional`/`set_variable`/`end`) is
  fully deterministic; only `prompt`/`branch`/`sub_agent` inside it aren't.

---

## Appendix — example

Parent `incident-intake`:

```json
{ "id": "route", "node_type": "subflow",
  "data": { "flow_id": "triage-and-approve",
            "inputs": { "item": "{{ticket_body}}" },
            "output_var": "triage" } }
```
Edges: `… → route`; `route --ok--> notify`; `route --error--> log_failure`.

The child `triage-and-approve` runs isolated on `{ item }`, and (Phase 2) can even
pause at its own `approval` node — suspending the parent — then resume straight
through `route` into `notify`.
