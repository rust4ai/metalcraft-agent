# Planner — an intrinsic persona for decomposition

Status: **design**, with a prerequisite experiment already shipped (§0). Phase 1
is content-only (JSON + markdown + three version bumps); no Rust changes are
required to ship it.

---

## 0. Shipped first: the text-only fix (2026-08-27)

The persona is only worth building for what prompt text *cannot* do (context
isolation, §1.1). Everything else the orchestrator was missing was text, so that
went first, alone, to see how much of the symptom it accounts for:

- **`seed/skills/decomposition.md`** (new) — a decomposition method written for a
  *delegator*. The existing `planning` skill is a solo-doer checklist ("do one
  step, verify it works, then move to the next") shared with `coding-agent` and
  `devops-agent`, and it never mentions sub-agents, personas, or briefs; loading
  it pushed the orchestrator toward doing the work in-context.
- **`orchestrator-agent` → v1.6.0** — the workflow's "Decompose" line got a
  threshold (3+ dependent steps / more than one subsystem / can't name the steps
  in one breath) and a new **worked four-step fan-out example**. The prompt
  previously carried six concrete 1:1 *routing* examples and zero fan-out ones,
  which is the most likely reason it routes instead of decomposing.
- **The two false concurrency claims deleted** — from the prompt and from the
  `sub_agent` tool description (`src/tools/sub_agent.rs:163`), replaced with the
  truth: sub-agents run one at a time and a turn costs the sum of its steps.

Verified: `cargo build` + `seed_consistency_test`, `persona_unit_test`,
`seed_templates_valid`, `seed::` unit tests green, and a real startup seeded
`decomposition.md` and force-upgraded the orchestrator to 1.6.0 on an existing
data dir.

**Read the result before building Phase 1.** If the orchestrator now fans out,
the persona's remaining claim is context isolation alone — real, but much
narrower than §1 argues, and possibly not worth a fifth roster entry. If it still
routes, that is strong evidence the behaviour needs its own agent run.

---

## 1. Verdict: yes, but as an *advisor*, not a second orchestrator

The value is real, and it is not "the agent can plan now" — the orchestrator
already decomposes (step 2 of its prompt) and already has the `planning` skill.
What a separate persona buys is three things the skill cannot:

1. **Context isolation.** Planning well means reading files, grepping, chasing
   how two subsystems actually connect. Today that exploration happens *in the
   orchestrator's own context window*, so by the time it starts delegating it is
   already carrying a few thousand tokens of half-relevant file contents — and
   the orchestrator is the one context that must survive the whole job. A
   sub-agent burns that exploration in its own window and returns ~40 lines.
   This is the single strongest argument, and it is the same argument that
   justifies `research-agent`.
2. **Plan-before-execute as a visible product moment.** A returned plan can be
   shown to the user *before* eight sub-agents spend eight minutes. Right now a
   complex request goes straight from "ok" to a wall of delegation.
3. **A plan that survives.** Chat compaction (`make_compaction_model`) and
   `schedule_followup` both mean the orchestrator can lose the thread of a long
   job. A plan artifact is the thing you re-read. (Phase 2.)

The honest costs:

- **An extra full agent run** per planned request — latency and tokens, before
  any real work starts. Mitigated by a threshold rule (§4.3): no planner for
  lookups, single-step asks, or anything one specialist persona covers.
- **Plan theater.** An LLM asked for a plan will always produce one, including
  for tasks that needed no plan. The prompt has to make "no plan needed — do X
  directly" a first-class, expected answer.
- **Overlap with `orchestrator-agent`.** If the planner also *executes*, you
  have two orchestrators and the user cannot tell which one is in charge. This
  is why the recommendation below is advisor-shaped.

---

## 2. What already exists (the delta is small)

| Piece | State |
|---|---|
| `planning` skill (`seed/skills/planning.md`, 22 lines) | Exists, unversioned, loaded by orchestrator/coding/devops |
| Orchestrator "Decompose" step | Exists in `seed/personas/orchestrator-agent.json` (v1.5.0) |
| Delegation to a named persona | `sub_agent` `persona` arg — inherits tools + prompt + skills (`src/tools/sub_agent.rs:249`) |
| Live persona list injected into any prompt with `sub_agent` | `src/persona.rs:265` — a new persona is auto-advertised, no prompt edit needed |
| Preset containment | `AgentPreset::delegation_roster` (`src/agent_preset.rs:299`) |
| Seed rollout on version bump | `write_versioned_seeds` (`src/seed.rs:244`) |
| Durable multi-step execution | metalcraft-flows v2: `SubAgent`, `Foreach`, `Approval`, `Wait`, `End` nodes + durable runs/resume |

So: the platform is already delegation-shaped. The planner is mostly a persona
file, a skill, and an output contract.

---

## 3. Shape: planner advises, orchestrator executes

Two candidate shapes:

**A — Advisor.** Planner explores read-only, returns a structured plan of
delegable steps. The orchestrator runs each step with `sub_agent`.

**B — Mini-orchestrator.** Planner plans *and* delegates each step itself.

**Recommend A**, on evidence from the code:

- **Timeout.** `DEFAULT_SUB_AGENT_TIMEOUT_SECS = 120` (`sub_agent.rs:22`). A
  planner in shape B runs N child agents *inside* its own 120s budget. It would
  need `max_run_secs` near the 1800s ceiling, and a plan cut off at step 3 of 7
  is indistinguishable from a failed task.
- **Containment escape.** A nested sub-agent is built with
  `preset_personas: None` (`sub_agent.rs:278`) — and `None` means *unscoped*,
  not *restricted*. So a persona that delegates can today reach any persona on
  the pod, escaping the preset's roster. That is a latent bug regardless
  (§8.1), but shape B walks straight into it.
- **No user channel.** A sub-agent gets `reply_sink: None` (`sub_agent.rs:287`),
  so a shape-B planner cannot tell the user anything until it finishes. Its
  entire multi-minute run is silent.
- **Product clarity.** One orchestrator, one plan, one narrator.

Shape B stays available later as an opt-in (§7), once §8.1 is fixed.

---

## 4. The design

### 4.1 The persona — `seed/personas/planner-agent.json`

```json
{
  "name": "Planner",
  "description": "Breaks a multi-step request into an ordered set of self-contained, delegable sub-tasks; reads and updates existing plans. Returns a plan — it never executes one.",
  "version": "1.0.0",
  "max_run_secs": 300,
  "tools": ["read_file", "list_files", "grep", "find_files", "load_skill", "mem_search", "mem_get"],
  "skills": ["decomposition", "planning", "explore-codebase", "research-methodology"],
  "system_prompt": "…see 4.2…"
}
```

Deliberate choices:

- **`max_run_secs: 300`.** The 120s default is too tight for a planner that
  greps a repo; 300 is well under the 1800 ceiling and under the operator's
  `SUB_AGENT_TIMEOUT_SECS` override. Bump to 600 if plans come back truncated.
- **No `bash`, no `write_file`/`edit_file`.** This makes "planning changes
  nothing" a structural guarantee, not a prompt promise. `research-agent` has
  `bash`; the planner deliberately does not. Cost: it cannot run `git log` or
  `cargo tree`. That is fine — the plan says "step 1: research-agent, run
  `git log …`", which is exactly the output we want anyway.
- **`mem_search`/`mem_get` but not `mem_remember`.** The planner reads prior
  decisions and preferences; it does not write memories. Plans are work
  artifacts with a lifecycle, and `MemoryKind` is a closed vocabulary with a
  decay pass (`src/memory/types.rs:14`) — a plan is not an episodic memory and
  must not be shoved into one.
- **No `integrations`.** Keeps `seed_consistency_test` green and lets every
  preset list it without declaring packs.

### 4.2 The output contract (the actual crux)

Everything else is plumbing; this is the part that decides whether the feature
works. The planner returns human-readable markdown **plus** a fenced JSON block,
so the orchestrator has something deterministic to iterate over:

```json
{
  "goal": "one sentence, concrete",
  "verdict": "plan" | "no_plan_needed",
  "assumptions": ["stated, not asked"],
  "open_questions": ["only blocking ones"],
  "steps": [
    {
      "id": "s1",
      "title": "short label",
      "task": "A SELF-CONTAINED delegation brief: paths, names, what 'done' means. Assume the reader has NONE of this conversation.",
      "persona": "research-agent",
      "tool_set": "read_only",
      "depends_on": [],
      "verify": "how the orchestrator knows this step actually worked",
      "risk": "what makes this step likely to fail"
    }
  ],
  "done_when": "the observable end state"
}
```

Rules baked into the prompt:

- `task` is written **for a stranger** — sub-agents inherit no conversation.
  Vague briefs are the #1 cause of useless sub-agent output today.
- Prefer `persona` over `tool_set`; pick from the live
  `{{available_personas}}` block, never from memory.
- `depends_on` is **ordering and result-passing**, not scheduling. Execution is
  sequential by design (§8.5); `depends_on` tells the orchestrator which earlier
  step's output has to be fed into this step's brief.
- 3–7 steps. More than ~9 means phase it: the last step is "re-plan phase 2".
- `verdict: "no_plan_needed"` with an empty `steps` array is a **success**, and
  the prompt says so explicitly.
- No step may be "ask the user" unless it is genuinely blocking — state an
  assumption instead.

### 4.3 Orchestrator changes — `orchestrator-agent.json` → v1.6.0

Add one section, and keep it narrow so the planner does not become a toll booth:

> **Plan first when the work is genuinely multi-step.** Delegate to
> `planner-agent` when the request needs three or more *dependent* steps, spans
> more than one subsystem or service, or you cannot name the steps yourself in
> one breath. Show the returned plan to the user in a few lines *before* you
> start executing, then run its steps with `sub_agent`, one at a time, in
> dependency order — feeding each step's result into the brief of any step that
> declares it in `depends_on`.
> If a step fails, re-delegate that one step; only go back to `planner-agent` if
> the failure invalidates the rest of the plan.
> **Do not plan** single lookups, one-file edits, pure conversation, or anything
> a single specialist persona already covers end to end. A planner call costs a
> whole extra agent run.

### 4.4 New skill — `seed/skills/decomposition.md`

**A new file, not an edit to `planning.md`.** Skills are seeded
*write-if-missing* with no version gate (`write_seeds`, `src/seed.rs:223`), so
editing `planning.md` would reach fresh pods only and silently skip every pod
already out there. A new filename lands everywhere.

Contents: step sizing (one sub-agent, one bounded outcome); writing a brief for
someone with no context; choosing persona vs `tool_set`; sizing steps to a
sequential wall-clock budget;
naming a verification per step; when to answer `no_plan_needed`; and how to
phase work rather than emit a 15-step plan.

### 4.5 Presets

- `general-agent` 1.2.0 → **1.3.0**: add `{"slug": "planner-agent", "role":
  "subagent", "description": "Breaks multi-step work into delegable steps"}`.
  It sets `delegates_to_any_persona: true`, so delegation would work without
  this — but the roster is what shows up in the UI and in the preset's own
  documentation of itself.
  (`metalcraft-assistant`, which used to need the same entry and had no
  `delegates_to_any_persona`, was removed on 2026-08-27 — it duplicated the
  `metalcraft-packs` agent pack's own preset.)

---

## 5. Phase 1 — ship the advisor (content-only)

| File | Change |
|---|---|
| `seed/personas/planner-agent.json` | new, v1.0.0 |
| `seed/skills/decomposition.md` | new |
| `seed/personas/orchestrator-agent.json` | prompt section + v1.6.0 |
| `seed/agent_presets/general-agent.json` | roster + v1.3.0 |
| `tests/planner_spice_test.rs` | new (§9) |

No Rust source changes. `include_dir!` picks the new seed files up
automatically — but note the caveat at `src/seed.rs:26`: a brand-new file can be
missed by a stale build, so `touch src/seed.rs` before testing.

---

## 6. Phase 2 — durable plans (`plans/`), only if Phase 1 earns it

"Read plans" splits in two:

- **Reading a human's plan doc** (`docs/*_PLAN.md`, `~/.claude/plans/*.md`) —
  already works today with `read_file`/`grep`. Phase 1 gets this for free; the
  skill should say so, and say to read the plan doc *first* when one exists
  rather than re-deriving it.
- **Re-reading a plan the agent made** — needs somewhere to put it.

Options considered:

| Option | Verdict |
|---|---|
| Memory (`mem_remember`) | **No.** Closed `MemoryKind` vocabulary + decay pass; a live work artifact is not a memory. |
| A markdown file via `write_file` | **No.** The planner is deliberately write-free, and there is no agreed home for it on a pod. |
| Express the plan as a **flow** | **Not for one-offs.** Flows v2 genuinely has the machinery (`SubAgent`/`Foreach`/`Approval` nodes, durable runs, resume, schedules, a workshop UI) — but asking a model to emit a valid node/edge graph for a one-shot task is a far bigger ask than a step list, and flow-run state is *execution* state, not *intent*. Keep it as the **promotion path**: when a plan proves repeatable, hand it to `workshop-agent` to turn into a flow. |
| A thin `plans/` store | **Recommended.** |

Shape, mirroring `flows`: `paths::plans_dir()` → `<data>/plans/<slug>.json`,
holding the §4.2 JSON plus per-step `status` (`pending`/`running`/`done`/
`failed`) and a short `result` note. Tools: `plan_write`, `plan_read`,
`plan_list`, `plan_set_step_status`, `plan_delete` — `plan_*` on the planner
(read + write), the status-setter on the **orchestrator** (it is the one that
knows a step finished). ~200 LOC plus an openapi entry and, later, a workshop /
metalcraft-front pane.

What this actually unlocks: a `schedule_followup` firing two hours later reads
the plan back instead of guessing; a stopped run resumes from the first
non-`done` step; a compacted conversation does not lose the job.

---

## 7. Phase 3 (optional) — let the planner execute

Grant `sub_agent`, raise `max_run_secs` toward the ceiling, and the planner
becomes a self-contained "do this whole multi-step job" call. Only worth it once
§8.1 is fixed, and only behind an explicit orchestrator instruction ("delegate
the whole job") so there is never ambiguity about who is narrating. Given the
silence problem (no `reply_sink`), I would not do this until plans are durable
(Phase 2) so the user can at least watch step statuses move.

---

## 8. Prerequisites and risks found in the code

1. **Nested delegation escapes the preset roster.** `sub_agent.rs:278` sets
   `preset_personas: None` with the comment "must not widen its own reach", but
   `None` is the *unscoped* case (`sub_agent.rs:~52`) — so a sub-agent that
   delegates can call any persona on the pod. Fix: thread the parent's roster
   into the nested `ToolConfig` instead of `None`. Independent of this feature;
   **required** before Phase 3.
2. **Seeded-persona edits get clobbered.** `write_versioned_seeds` overwrites a
   user's edits to a seeded persona on version bump. Bumping the orchestrator
   to 1.6.0 will blow away anyone's local tweaks to `orchestrator-agent.json`.
   Known trade-off, but call it out in release notes.
3. **Skills have no version gate** — see §4.4. New file, not an edit.
4. **Plan theater / latency regression.** Watch for the planner being invoked
   on trivial requests. If it happens, tighten the threshold in §4.3 rather
   than weakening the planner.
5. **Two false claims about concurrency, and one real consequence.**
   `ToolNode::run` iterates `pending_tool_calls` in a plain `for` loop, awaiting
   each in turn (`metalcraft-0.9.0/src/tools.rs:194`) — tool calls are executed
   **sequentially**. The `sub_agent` tool description ("Multiple sub-agents run
   concurrently") and the orchestrator prompt ("Independent sub-tasks can be
   delegated in parallel — multiple sub-agents run concurrently") both say
   otherwise. Sequential execution is the *intended* behaviour, so the fix is to
   delete the two claims, not to add concurrency: a model told its delegations
   are free in wall-clock will size plans as if they were.

   What sequential execution actually costs, and how the design absorbs it:

   - **Wall clock is the sum of the steps**, each bounded by the sub-agent
     timeout (120s default). A 7-step plan is a genuinely long turn. Step-count
     discipline (3–7, phase beyond that) stops being style advice and becomes the
     load-bearing constraint; `decomposition.md` must say so.
   - **No prose narration between steps.** In workshop/gateway sessions
     `say_to_user` is a *terminal* tool (`runtime.rs:747`) — calling it ends the
     turn — so the orchestrator physically cannot comment between step 3 and
     step 4. Progress is visible only as `ToolStarted`/`ToolCompleted` SSE cards
     (`workshop_api.rs:4773`). That is real feedback (each card carries the
     step's `task` text, so a good `title`/`task` reads as a progress line), but
     it means the plan shown *before* execution (§4.3) is doing more work than it
     first appears: it is the user's only prose account of what is about to
     happen.
   - **The upside:** sequential execution makes the orchestrator adaptive. It
     sees step N's result before committing to N+1, so it can re-scope, skip a
     now-unnecessary step, or abandon the rest of the plan. A parallel fan-out
     would have spent all of it up front. This is a good reason to keep the
     planner an advisor (§3) rather than a batch executor.

6. **No per-persona model.** `Persona` has no `model` field; a sub-agent
   inherits the parent's `model_name`. Running the planner on a stronger
   reasoning model would need a new optional `model` field plumbed through
   `sub_agent`'s `client.completion_model(...)`. Cheap (~20 LOC), but it is a
   separate decision with a real cost per call — out of scope here.

---

## 9. Testing

Mirror `tests/config_spice_test.rs`'s three tiers in a new
`tests/planner_spice_test.rs`:

1. **Wiring (always runs, no network).** Seed into an isolated data dir; assert
   `planner-agent` resolves, exposes exactly its read-only tool set, resolves
   its skills, declares no integrations, and that both presets list it in
   `delegation_roster`.
2. **Contract (no LLM).** Feed a canned planner output through the parser the
   orchestrator would use and assert the JSON block validates against §4.2 —
   ids unique, `depends_on` referencing real ids, no cycles. (In Phase 1 the
   "parser" is the model, so this tier really lands once Phase 2 adds
   `plan_write` validation; keep it as a fixture-driven schema test until then.)
3. **Live (gated on `OPENAI_API_KEY`).** Drive a real loop through
   `planner-agent` with a genuinely multi-step prompt and assert the result
   contains a JSON block with ≥3 steps, every step naming a persona that exists
   on the pod. Plus the negative case: a trivial prompt ("what does
   `src/paths.rs` do?") must come back `no_plan_needed`.

Also extend `tests/seed_consistency_test.rs` coverage implicitly — it already
walks `seed/` and will fail if the new persona names a skill or pack that isn't
shipped.

---

## 10. Rollout

Content-only, so it rides the normal path: merge to master → CI tags the image
with the Cargo version → bump a pod via the admin **Force update** action (or
`:X.Y.Z` override). On next pod start `write_versioned_seeds` writes
`planner-agent.json` (missing → written), `decomposition.md` (missing →
written), and force-upgrades the orchestrator and both presets on their version
bumps. Existing pods pick it up without any user action; the live
`{{available_personas}}` block means every already-installed orchestrator starts
advertising the planner the moment the file lands.

---

## 11. Open questions

1. **Threshold.** Is "3+ dependent steps" the right trigger, or should the
   planner be opt-in by phrasing ("plan this", "how would you approach…")
   until we see how often it fires? Opt-in is the safer launch.
2. **Does the user see the plan every time?** Showing it is the main product
   win; showing it on every mildly-complex request is noise. Proposal: always
   show, but as ≤6 short lines, not the raw JSON.
3. **Phase 2 now or later?** Durability is what makes plans more than
   pretty output — but it is also the only part with real code in it.
4. **Approval gate?** Flows have an `Approval` node. Should a plan above N
   steps pause for a yes before executing? Powerful, and annoying if wrong.
