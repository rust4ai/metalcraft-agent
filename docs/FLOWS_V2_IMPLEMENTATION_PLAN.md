# Flows v2 — Implementation Plan (applying the design to the codebase)

Companion to `FLOWS_V2_STATE_MACHINE_PLAN.md` (the design). This doc is the
concrete engineering plan: which files change, in what order, what to reuse, and
how to test. Grounded in the code as it exists today.

---

## 0. Scope: three codebases, but only two change

| Codebase | Role | Changes? |
| --- | --- | --- |
| **`metalcraft-flows`** (crate `rust4ai/metalcraft-flows`, 0.1.0 → **0.2.0**) | Wire format, validation, pure helpers. **No async, no LLM.** | **Yes** — new node types + `eval`/`template`/`state` modules + handle-aware walk + validation. |
| **`metalcraft-agent`** (this repo, 0.8.2) | The executor, node runners, run store, tool/daemon/workshop wiring. | **Yes** — the bulk of the work. |
| **`metalcraft`** (core crate 0.8.0) | ReAct engine: `create_react_agent_with_options`, `ToolChoice`, `terminal_tools`, `AgentToolCall`. | **No change needed.** Everything the typed-`branch` requires already exists — confirmed in `prebuilt.rs` (`ToolChoice::Required` §423, `AgentToolCall{name,args}` §103) and surfaced via `runtime.rs::RuntimeOptions` (`tool_choice`, `terminal_tools`). |

**Scoping win:** no core-crate fork/publish is on the critical path.

### Reuse inventory (what we build *on top of*, not from scratch)

| Need | Existing primitive | Location |
| --- | --- | --- |
| Run a prompt as an agent | `run_one_shot_task` / `build_agent_runtime` | `src/runtime.rs:145` / `:84` |
| Force tool-only + terminal tools | `RuntimeOptions { tool_choice, terminal_tools }` + `ToolChoice::Required` | `src/runtime.rs:66`, core `prebuilt.rs` |
| Build a scoped tool registry | `create_registry_for` / `create_registry_for_with_config` | `src/tools/mod.rs` |
| Delegate to a sub-agent | `SubAgentTool` | `src/tools/sub_agent.rs` |
| Call an integration / HTTP tool | `HttpApiTool` (+ `native_pack_tool_names`) | `src/tools/http_api.rs` |
| Load a persona (prompt + tools + skills) | `Persona::load` / `resolved_tool_names` | `src/persona.rs` |
| One flow-tagged session log | `DiagnosticsLogger` + `log_config_change` | `src/diagnostics.rs`, used in `flows.rs:198` |
| Flow CRUD + fs store | `save/load/list/delete_flow` | crate `store.rs` |
| Scheduler loop + due-check | daemon flow-polling loop, `is_due`, `FlowRunState` | `src/daemon.rs:167` |
| Paths | `paths::flows_dir()` / `flow_templates_dir()` | `src/paths.rs` |

---

## Workstream A — `metalcraft-flows` 0.2.0 (wire format & pure logic) — ✅ DONE

**Status: complete** (branch `flows-v2`, commit `4e12cd9`). All items below built,
`cargo test --all-features` green (60 tests), clippy + docs clean, MSRV 1.91 /
edition 2024. Not yet published to crates.io — the agent should consume it via a
git/path dep during Workstream B.

Keep the crate **pure** (its current character: no tokio, no rig). It owns types,
validation, and side-effect-free evaluation; the agent owns execution.

**A1. `model.rs` — node type enum.**
- Extend `CoreNodeType`: `Entry, Prompt, Conditional, Branch, SetVariable, Tool, Http, SubAgent, Approval, Wait, Foreach, End`; **retain** `BranchTool` (deprecated, still parses). Update `as_str` / `from_wire`.
- `SPEC_VERSION = "2"` (still parse `"1"`; unknown-to-v1 bare types already degrade to `Custom` via existing `Deserialize`, so old parsers stay safe).
- Add typed `data` structs in a new `nodes` submodule for ergonomics + validation: `EntryData{schedule_type, interval?, cron?, inputs?}`, `Condition{handle, variable, operator, value}`, `BranchOutput{handle, description, schema, var?}`, etc. Nodes keep `data: Value` on the wire; these are parse-on-demand views.

**A2. `walk.rs` — handle-aware routing.**
- Add `pub fn next_by_handle(def, source, handle: Option<&str>) -> Option<String>` (prefer matching `source_handle`; fall back to the unnamed edge). Port of vix `get_next_node_from_edges`. Keep `walk_bfs` for reachability/validation.

**A3. `eval.rs` (new) — `conditional` predicate evaluation.**
- `Operator` enum + `evaluate(op, actual: &Value, expected: &Value) -> bool`.
- **Numeric coercion** for `gt`/`lt` (both operands parse as numbers → numeric compare), fixing the vix string-compare bug. `matches` = regex; `exists`/`truthy` on presence/truthiness.

**A4. `template.rs` (new) — `{{path}}` interpolation.**
- `resolve(tmpl: &str, vars: &Value) -> String`, dotted JSON-path lookups, missing → empty (strict-mode flag optional). Generalizes vix `resolve_handlebars`.

**A5. `state.rs` (new) — variables helpers.**
- `Variables` newtype over `Value`: get/set by dotted path, `set_last`, seed-from-inputs. Used by the agent executor (pure logic lives here so it's unit-testable without the runtime).

**A6. `validate.rs` — per-node validation.**
- Schema checks per node type (conditional conditions, branch `outputs`, prompt fields); handle/edge coherence (every emitted handle has an edge); `spec_version` gating (v2 nodes require `"2"`); **best-effort edge type-check** (handle `schema` vs. how target consumes `_last`); reachability warnings.

**A7. Docs/release.** `SPEC.md` → v2 (§2.1–§2.6 of the design); `CHANGELOG.md`; conformance tests + `examples/` (incl. the Madrid flow). Publish `0.2.0`. **During dev**, the agent can point at a git/path dep to avoid blocking on a crates.io publish.

**Tests (A):** node-type round-trip; `eval` numeric/regex/exists; `template` paths; `next_by_handle` precedence+fallback; validation accept/reject; v1 docs still parse.

---

## Workstream B — `metalcraft-agent` executor (the heart)

New module `src/flow_exec.rs` (or a `src/flow_exec/` dir). Replaces the
plan-then-run pair in `flows.rs` (`collect_reachable_prompts` + `run_flow`).

**B1. Executor skeleton.**
```rust
struct FlowExecutor<'a> { ctx, def, run_id, variables: Value, current: String,
                          step_budget: u32, logger, persona_slug, model_name, approval_mode }
enum StepOutcome { Goto(String), Continue(Option<String>), Pause{..}, Complete{status}, Fail{node,error} }
```
`run()` loop: fetch `current` node → dispatch → apply outcome (Goto/Continue advance via `next_by_handle`; Pause/Complete/Fail persist+return); bounded by `step_budget`. `resume(run_id, handle, data)` for Phase 3.

**B2. Node executors** (one fn each; Phase-1 set first):
- `entry` — seed `variables` from `inputs`; `Continue(None)`.
- `prompt` — `template::resolve` the prompt → `run_one_shot_task` (reusing the exact inner logic already in `flows.rs::run_flow` lines 223–273) → store final answer into `output_var` + `_last`; parse `output_schema` if present; `Continue("ok")` / `Continue("error")`.
- `set_variable` — pure assign (literal/template `value`, or JSON-path `from` into `_last`).
- `tool` — resolve **one** tool via `create_registry_for(&[tool_name])`, interpolate `args`, call `.call()` directly (no agent loop), `_last = result`, `ok`/`error`.
- `http` — reuse `HttpApiTool` path (keep its SSRF/url validation).
- `sub_agent` — reuse `SubAgentTool::call` semantics; store result in `output_var`.
- `conditional` — `eval.rs` over `variables` (numeric-aware) → `Goto(next_by_handle)`.
- `branch` — **the typed-classifier node** (B3).
- `approval` / `wait` — `Pause` (Phase 3, Workstream C).
- `end` — `Complete` with optional `outputs`.

**B3. `branch` executor + `HandleTool` (new).**
- New `HandleTool` implementing `metalcraft::Tool`: `name()` = handle, `description()` = handle description, `parameters_schema()` = the handle's declared JSON `schema`, `call()` echoes its args as JSON.
- Build a registry = **persona's real tools** (via `Persona::load` + `resolved_tool_names`, so `weather-agent` can look things up) **+** one `HandleTool` per `output`.
- Run `build_agent_runtime` / `run_one_shot_task` with `RuntimeOptions { tool_choice: ToolChoice::Required, terminal_tools: <handle names>, ..default }` and the `query` as the task.
- On completion, read the terminal `AgentToolCall { name, args }` from `state` (scan `state.turns()` / `messages` for the last `ToolCall` whose `name` is a handle): `name` → taken handle, `args` → payload. Set `_last` (and `var` if declared), `Goto(next_by_handle(handle))`. Fallback to `default_handle` on timeout/no-call.

**B4. State threading.** `variables` seeded at entry; every node reads/writes it; `_last` = payload of the traversed edge; all string fields resolved through `template::resolve` immediately before use.

**Tests (B):** linear stateful flow (prompt→set_variable→prompt reads `_last`); `conditional` numeric route; `branch` typed-payload happy/error paths (integration test gated on `OPENAI_API_KEY`, plus a unit test that stubs the terminal `ToolCall` extraction); v1 linear flow runs identically.

**B5. LLM-in-the-loop end-to-end test — the Madrid user story.** Prove a real flow
run conducts the story, not just that the wire format validates (the crate already
covers the deterministic routing half in `examples/madrid_weather.json` +
conformance tests). Approach:
- Register a **mock `weather` tool** (a `metalcraft::Tool` returning a fixed temp,
  e.g. 18°F — no real API) and a `weather-agent` test persona scoped to it.
- Run `FlowExecutor` on the Madrid flow with a live model (gated on
  `OPENAI_API_KEY`, `#[ignore]` by default in CI without a key).
- Assert: the `branch` terminates by calling `report_temp(<int>)`; `_last` holds
  the int; the `conditional` routes to `say_cold` for 18 and `say_hot` when the
  mock returns 75; a failing mock drives the `error` handle → `handle_err`.
- Use **`spice-framework`** (already a dev-dependency) for the model-in-the-loop
  harness / assertions where it fits; otherwise a plain `#[tokio::test]`. This is
  the single most important behavioral proof of the design — it validates the
  novel terminal-tool-choice → typed-payload → numeric-conditional chain end to end.

---

## Workstream C — run store + pause/resume (Phase 3)

**C1. `paths::runs_dir()`** — sibling of `flows_dir()`.

**C2. `src/flow_runs.rs` (new)** — `FlowRun { id, flow_id, status, current_node_id, variables, pause: Option<{reason, resume_handles, wake_at?}>, created_at, updated_at }` + `save/load/list/delete` mirroring crate `store.rs` (one JSON per run under `runs/`).

**C3. Executor checkpointing.** On `Pause`, write `FlowRun`; `resume(run_id, handle, data)` loads it, folds the decision/signal into `variables._last`, and continues from `current` via `next_by_handle`. Maps 1:1 to vix `update_execution` / `continue_flow`.

**C4. Pause nodes.** `approval` → `Pause{resume_handles: choices}`; `wait` → `Pause{wake_at}`.

---

## Workstream D — agent-facing tools + skill

- `src/tools/meta_flow.rs`: `flow_run` returns `{ run_id, status }` (may be `paused`); **add** `FlowResumeTool`, `FlowRunStatusTool`, `FlowRunsListTool`. Register them in `src/tools/mod.rs`.
- `seed/skills/authoring-flows.md`: rewrite for v2 (state model, node table, interpolation, the Madrid example).
- `seed/flow_templates/`: add stateful templates (conditional, branch, approval).

---

## Workstream E — daemon + workshop wiring

- `src/daemon.rs`: replace the `collect_reachable_prompts` + per-prompt loop (lines ~197–268) with `FlowExecutor::run`. Keep `is_due` / `FlowRunState` for scheduled `entry`.
- **Resume scan:** each tick, after `load_enabled_flows`, scan `runs_dir()` for `paused` runs with `wake_at ≤ now` and `FlowExecutor::resume` them (durable `wait`).
- `src/workshop_api.rs`: extend the existing run-flow endpoint to return `{run_id,status}`; add run-status + resume/signal endpoints (the domain-neutral analog of vix's Twilio callback).

---

## Ordering (dependency-aware, each step shippable)

1. **A (crate 0.2.0)** — ✅ **done** (branch `flows-v2`, commit `4e12cd9`). Agent depends on it; wire via git/path dep during dev until published.
2. **B Phase-1** (entry, prompt, set_variable, tool, http, sub_agent — **no branching**) + swap the daemon loop + `flow_run` over `FlowExecutor`. → *Ships stateful linear + effectors.* v1 flows unchanged.
3. **B Phase-2** (`conditional` + `branch` + `HandleTool` + handle routing). → *Ships branching + typed edge payloads — the Madrid flow runs.*
4. **C + D + E Phase-3** (run store, approval/wait, resume tools, daemon resume scan, workshop endpoints). → *Ships durability / human-in-the-loop.*
5. **Phase-4** (foreach/join; port vix's React-Flow editor into `metalcraft-workshop`). Separate initiative.

---

## Risks / watch-items

- **`branch` terminal-tool extraction** — confirm the terminal `ToolCall`'s `args` are readable from `AgentState` after the turn ends (they are in `messages`/`turns()`); cover with a unit test that feeds a synthetic state, plus one live integration test. This is the single most novel mechanic.
- **Crate release cadence** — develop against a git/path dep; publish `0.2.0` only when the format stabilizes, then flip `Cargo.toml`.
- **Back-compat** — route *all* flows through `FlowExecutor`; a v1 `entry→prompt→prompt` chain is a strict subset (prompts fire in order, no `output_var` ⇒ nothing stored). Regression-test an existing v1 flow.
- **Numeric coercion** — the one behavior we must *not* inherit from vix; unit-test `gt`/`lt` on numbers vs strings.
- **`tool` node vs. agent tools** — the `tool` node calls a tool *directly* (deterministic, no LLM); make sure arg interpolation + error surfacing match how the same tool behaves inside an agent loop.

---

## Rough effort

| Workstream | Est. |
| --- | --- |
| A (crate 0.2.0) | 1–2 d |
| B Phase-1 (executor + effectors + daemon swap) | 1–2 d |
| B Phase-2 (conditional + branch + HandleTool) | ~1 d |
| C + E (run store + resume + daemon/workshop) | ~2 d |
| D (tools + skill + templates) | ~0.5 d |
| Phase-4 (foreach + visual editor) | separate |

First concrete PR: **Workstream A** (the crate), since every agent-side change
compiles against its new types.
