# Refactor Plan: Collapse all agent-turn execution onto a single call site

**Status:** ✅ implemented (branch `refactor/unify-turn-runner`).
**Author:** drafted during the gateway-compaction fix.
**Goal:** Have exactly one place in the codebase that "runs one agent turn" (build/resolve runtime → compact context → run executor → classify outcome), so behaviours like compaction, step limits, and guard config can never again be present in one path and missing in another.

> **Implementation notes (as built).** Two deliberate deviations from the plan below:
> 1. **The step guard is a `run(state, step_guard)` parameter, not a `TurnRunner` field.** Guard lifetime is genuinely caller-specific — the CLI reuses one session-long guard, while the workshop/gateway guard is rebuilt per turn because it captures that turn's SSE/reply sender to emit tool events. Making it a field would force one lifetime on both. `TurnRunner` owns only the prebuilt runtime + compaction/max-steps config; `run` returns `(compacted: bool, Result<RunOutcome, GraphError>)` so the interactive CLI can print a compaction notice while daemon callers ignore the flag.
> 2. **`run_one_shot_task` was folded in too** (plan §6 had it out of scope). It was the *one* turn path with no compaction at all, so routing it through `TurnRunner` closes that gap. Behaviour is unchanged for realistic one-shot tasks (compaction only triggers above ~76.8k tokens, which single tasks and tests never reach). `max_steps` is now the shared `runtime::MAX_TURN_STEPS` constant (still 90) everywhere.
>
> Not folded in: `sub_agent.rs` (nested tool executor, no guard, `max_steps 90`) and `flow_exec.rs` (`max_steps 30`) — these run structurally different graphs and are intentionally left separate.

---

## 1. Why

The context-compaction bug that prompted this existed *because* turn execution lives in more than one place. Today there are **three** entry points that each run the executor:

| Entry point | File | I/O shape | Approval | Output | Runtime lifetime |
|---|---|---|---|---|---|
| `post_chat_turn` (workshop UI) | `src/workshop_api.rs:1250` | HTTP + SSE event stream | `AutoApprove` | `ChatEvent`s over SSE | rebuilt per turn |
| `run_one_gateway_turn` (WhatsApp/gateway) | `src/workshop_api.rs:2356` | webhook + adapter reply sink | `AutoApprove` | `say_to_user` → adapter | rebuilt per turn |
| CLI REPL loop | `src/main.rs` (~470–520) | interactive terminal (readline) | **interactive** | prints `final_answer()` | **built once**, reused across turns |

The two daemon paths were already harmonized: both funnel through the shared **`run_chat_turn`** (`src/workshop_api.rs:1659`), distinguished only by `SessionPreset` / `RuntimeOptions` (reply sink, tool-choice). That shared function is where compaction now lives, so gateway and workshop are covered by one edit.

The **CLI is the odd one out**: it does not call `run_chat_turn`. It builds its own `Executor` inline and had its own copy of the compaction call (`main.rs:488`). The compaction *logic* is already shared (`context::compact_if_needed`), but the *invocation* is duplicated. This refactor removes that last duplication.

---

## 2. What actually differs between the CLI and `run_chat_turn`

These are the real obstacles — each must be made a parameter rather than a hardcoded choice:

1. **Approval mode.** `run_chat_turn` hardcodes `ApprovalMode::AutoApprove`. The CLI needs interactive approval (and can switch via flags). → must become a parameter.
2. **Runtime options.** CLI uses `RuntimeOptions::default()` (free-text agent; model decides text-vs-tool; a free-text answer ends the turn). Daemon uses tool-only (`ToolChoice::Required`, `terminal_tools = ["say_to_user"]`, a reply sink). → already a parameter on `run_chat_turn`; CLI just passes its own.
3. **Runtime lifetime.** CLI builds the graph + compaction model **once** and reuses it for the whole session (cheap, no per-turn rebuild). `run_chat_turn` **rebuilds every turn** (acceptable for the daemon's spawn-per-turn, independent-session model). → the unified primitive must support *both* "bring your own prebuilt runtime" and "build one per call".
4. **Guard config.** CLI uses `GuardConfig::default()`; one-shot uses a `poll_tools`-aware config (`runtime.rs:165`). → must be a parameter.
5. **Outcome handling / output.** CLI prints `final_answer()` and updates local `state`. Daemon emits `ChatEvent`s / drives a reply sink and persists. → stays *outside* the shared primitive; the primitive returns `RunOutcome`, each caller renders it.
6. **Between-turn concerns (CLI only).** `/`-commands, model switching (rebuilds runtime), readline history. → stays in the CLI loop; orthogonal to running a turn.

---

## 3. Proposed design

Introduce one primitive that owns **build-or-reuse runtime → compact → execute**, and nothing else. Everything I/O- and session-specific stays with the callers.

### 3a. Extract a `TurnRunner` (preferred)

A small struct that holds a *prebuilt* runtime and the per-turn knobs, with a single `run` method:

```rust
// src/runtime.rs (or a new src/turn.rs)
pub struct TurnRunner<M: CompletionModel + 'static> {
    graph: SharedAgentGraph,
    compaction_model: M,
    compaction_config: CompactionConfig,
    step_guard: StepGuard<AgentState>,
    max_steps: usize,
}

impl<M: CompletionModel + 'static> TurnRunner<M> {
    /// Compact the state to fit the window, then run one turn to completion.
    pub async fn run(&self, mut state: AgentState)
        -> Result<RunOutcome<AgentState>, ExecutorError>
    {
        match context::compact_if_needed(&mut state, &self.compaction_model, &self.compaction_config).await {
            Ok(true)  => log::info!("Context compacted -> ~{} tokens", context::estimate_tokens(&state)),
            Ok(false) => {}
            Err(e)    => log::warn!("Context compaction failed, proceeding uncompacted: {e}"),
        }
        Executor::new_from_arc(self.graph.clone())
            .max_steps(self.max_steps)
            .with_step_guard(self.step_guard.clone())
            .run(state, "agent")
            .await
    }
}
```

A constructor wraps `build_agent_runtime` so callers don't duplicate the build:

```rust
pub fn build_turn_runner(
    context: &AgentRuntimeContext,
    persona: &Persona,
    cwd: &str,
    model_name: &str,
    approval_mode: ApprovalMode,
    options: RuntimeOptions,
    hooks: TurnHooks,            // llm_call_hook, llm_response_hook
    guard_config: GuardConfig,
    diagnostics: Option<Arc<DiagnosticsLogger>>,
) -> Result<TurnRunner<impl CompletionModel>, Box<dyn std::error::Error>>;
```

### 3b. Rewire the three callers

- **`run_chat_turn`** becomes a thin shim: build a `TurnRunner` (per turn, as today), call `.run(state)`. The compaction block I just added moves into `TurnRunner::run`. Net: a few lines shorter; behaviour identical.
- **CLI** builds the `TurnRunner` **once** before the loop, calls `.run(state)` each turn, and rebuilds it only on `/model` switch. The inline compaction at `main.rs:488` and inline `Executor::new_from_arc(...)` at `main.rs:498` are deleted. The CLI keeps owning readline, `/`-commands, and `final_answer()` printing.
- **`run_one_gateway_turn`** is unchanged (already routes through `run_chat_turn`).

### Why a struct, not just a free `fn run_turn(...)`

The CLI's win is **reusing a prebuilt runtime** across turns. A free function that builds-per-call would force the CLI back to per-turn rebuilds (a perf/cost regression: new registry, new client, new graph each turn). The struct lets the daemon build-per-turn (construct, use once, drop) and the CLI build-once (construct, reuse), from the *same* `run` body.

---

## 4. Migration steps (incremental, each compiles + tests green)

1. Add `TurnRunner` + `build_turn_runner` in `runtime.rs`. No callers yet.
2. Port `run_chat_turn` to construct a `TurnRunner` and delegate. Run the gateway spice suite + workshop tests. (Behaviour must be byte-identical — same `max_steps(90)`, same compaction config.)
3. Port the CLI loop: build the runner once, delegate per turn, delete the inline compaction + executor. Manually verify a REPL session (interactive approval still prompts; `/model` switch rebuilds the runner).
4. Delete now-dead helpers if any (e.g. if `BuiltAgentRuntime` is no longer used directly outside `build_turn_runner`).
5. Add a unit/integration test asserting both presets go through `TurnRunner::run` (e.g. a test double counting compaction invocations).

Keep each step as its own commit so a regression bisects cleanly.

---

## 5. Risks / things to watch

- **Approval mode regression.** Easiest way to break the CLI is to leak `AutoApprove` into it. Make `approval_mode` a required `build_turn_runner` arg with no default.
- **Runtime rebuild cost.** Don't accidentally make the CLI rebuild per turn. The whole point of the struct is build-once for the CLI.
- **Hook lifetimes.** `LlmCallHook` / `LlmResponseHook` are `Arc`-wrapped closures; the daemon recreates them per turn (they capture per-turn diagnostics). Confirm the CLI's once-built hooks still behave (CLI currently builds the hook once — fine).
- **`Send` boundaries.** `run_chat_turn` is careful about not holding non-`Send` `Box<dyn Error>` across `.await` (see comments at `workshop_api.rs:1321`). `TurnRunner::run` is `async` and spawned inside `tokio::spawn` on the daemon — keep its error types `Send + Sync` or convert before the await.
- **Guard `poll_tools`.** If the CLI should also get poll-tool-aware guarding (it currently uses `default()`), this refactor is a chance to align it — but treat that as a deliberate behaviour change, not a silent side effect.
- **Outcome classification stays at the edge.** Don't pull the `match outcome { Completed | Interrupted | Failed | Err }` blocks into `TurnRunner` — each caller does different things (SSE event vs reply sink vs print vs persist/rollback). The primitive returns `RunOutcome`; callers classify.

---

## 6. Out of scope

- Caching runtimes **per gateway/workshop session** (so a chat doesn't rebuild every turn). Worth doing eventually, but it's a separate optimization with its own cache-invalidation questions (persona edits, model switches). Note it; don't bundle it.
- Changing compaction thresholds, the `chars/4` token heuristic, or the `128_000` window — tracked separately (see the gateway-compaction fix notes).
- Routing `run_one_shot_task` through the same primitive. It already shares `build_agent_runtime`; folding it in is a nice-to-have, not required for "single turn call site".

---

## 7. Expected end state

- One function body (`TurnRunner::run`) executes every agent turn in the process.
- Compaction, `max_steps`, and step-guard wiring exist in exactly one place.
- The three entry points shrink to: *resolve persona/model/options → build or reuse a `TurnRunner` → `.run(state)` → render the outcome in their own I/O.*
- A behaviour present in one path cannot silently be absent from another — which is the class of bug this whole exercise was about.
