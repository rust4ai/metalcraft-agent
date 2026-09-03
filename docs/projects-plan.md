# Projects

> Supersedes `goal-agent-plan.md`. A goal is not a thing you run — it is a
> *string* the thing you run is aimed at. The thing you run is a **project**.

## 1. The shape

```
Project  ── state: goal, heartbeat (15m), status, rails, counters, workspace
   │
   ├── conductor  ─ agent instance. Strong model, few tools, runs every heartbeat.
   │                 Owns the PLAN and the VERDICT. Never does the work.
   │
   └── worker     ─ agent instance + session. Cheap model, full tools.
                     System prompt written by the conductor at boot, from the goal.
                     Fresh context each heartbeat; the session keeps every turn.
```

Two agents, and the split between them is the design:

| | conductor | worker |
| --- | --- | --- |
| runs | every heartbeat, briefly | every heartbeat, at length |
| model | strong | cheap |
| sees | goal, scratchpad, task list, journal, runs in flight | its briefing, the contract, the tasks, the scratchpad |
| owns | the plan (`task_*`), the verdict (`project_complete` / `project_block`), the pace | the work, and the evidence for it |
| never | does the work | decides whether the project is done |

The last row is the point. Today one agent plans, works, and then judges whether
its own work met its own goal — which is the weakest possible arrangement for the
one decision that matters most. Splitting them means the thing that says "done"
is not the thing that wants to be finished.

## 2. Boot

A project is created by a person (the consent point — unchanged; committing a pod
to days of unattended spend is a decision a human takes on a screen built for it).
Creating one:

1. Creates the **conductor** instance.
2. The conductor's first act: **write the worker's system prompt from the goal.**
   A project aimed at a Rust repo and one aimed at a docs site should not get the
   same worker, and a generated prompt can say what a generic persona cannot.
   Stored on the project as state, editable by hand like the scratchpad — it is
   written once, not re-derived every boot, or it is not stable enough to debug.
3. Creates the **worker** instance and its **session**.
4. Seeds the scratchpad. Sets `status = active`. The first heartbeat is due
   immediately: somebody just asked for this, and waiting fifteen minutes to start
   reads as broken.

## 3. The heartbeat

Default **15 minutes**, floor 5, per-project override, shorter fuse while a run is
in flight. (Thirty was chosen when every wake-up cost a model call. It does not:
the pre-flight answers "is the build done?" with an HTTP GET, so an idle wake-up
is nearly free, and fifteen buys twice the responsiveness for almost nothing.)

One heartbeat, in order:

```
1. pre-flight        no model. Poll every task's run, reconcile the workspace,
                     read compute minutes. Nothing landed and nothing ready
                     ⇒ stop here, spend nothing, look again in 5.

2. conductor turn    sees: goal · scratchpad · task list · last N journal lines ·
                           what landed in the pre-flight · rails and counters
                     does: grooms the plan (task_add / task_update / task_drop),
                           then writes THE BRIEFING for this tick.
                     may:  project_complete, project_block, change the pace.

3. worker turn       fresh context, assembled by code:
                       [system prompt]  generated at boot, constant since
                       [briefing]       from the conductor, this tick only
                       [contract]       constant, never generated  ← §4
                       [task list]      rendered from records
                       [scratchpad]     State / Log / Blockers / Questions
                     posted as a turn in the project's session.

4. fold              runs started are recorded on their tasks; the journal gets a
                     line; counters move; the workspace is hibernated.
```

### Why the conductor writes the plan

Because it is the only one that can see the whole thing. The worker gets a fresh
context every tick precisely so it does not carry the project around — which
makes it a bad planner and a good executor. Moving the plan to the conductor also
retires `TickKind`: there is no "planning tick" or "review tick" any more, because
every tick is planned and reviewed by the conductor before the worker wakes.

## 4. Generated briefing, constant contract

**Decided.** The conductor writes the situational half. The runner always appends
the invariant half, and the invariant half is code:

```
- Do one slice of work, not the whole project.
- Verify before you claim: task_done wants the commit, the run and its exit code,
  or the file.
- Uncommitted work does not exist.
- Decide, don't stall. task_block parks one task; project_block stops everything.
- Finish by rewriting the scratchpad — State, Log, Blockers, Questions. Leave the
  plan alone; the task list is the plan.
```

A generated frame that quietly drops one of those is the same failure class as a
model rewriting its own plan and losing a row — which is exactly what the task
list was built to make impossible. Do not reintroduce it one layer up.

Fail-open: if composing the briefing fails (bad response, model down), the tick
runs on a templated briefing rather than not running. A broken composer must
never wedge a project.

The briefing is stored on the journal entry. When a tick goes wrong the first
question is "what did it actually say?", and it should be answerable without SSH.

## 5. The session

**Decided.** One session per project, created at boot, and the context is reset
each heartbeat.

This is not a compromise — `ChatSession` already separates `transcript` (the
conversation, durable) from `state` (the context a model sees), and
`reset_context` ends one while keeping the other. So:

- **Cost per tick stays flat.** A project can run for a month. An ever-growing
  context makes every tick cost more than the last until compaction eats the
  detail the project depends on.
- **The history is complete and readable.** Every tick is a turn in one
  conversation, in the workshop, rather than a journal line pointing at a
  transcript nobody can find.
**The session is a window, not a chat.** A person watches it; nobody types into
it. A project that could be interrupted mid-thought by a message would need every
tick to reconcile "what I was doing" with "what somebody just said", and the
whole premise is that the person who set it has gone away. Outbound still works —
a blocked project says so, wherever its binding reaches.

## 6. What a person can do — three levers, and no more

| Lever | What it is |
| --- | --- |
| **Edit the goal** | The steering wheel. The goal is a string in state, so re-aiming is an edit rather than a deletion. |
| **Force a heartbeat** | Apply a change now instead of within fifteen minutes. |
| **Change the period** | How often it wakes, floored at 5 minutes. |

Plus the ordinary lifecycle: pause, resume, archive.

That is deliberately small. Every other way of steering a long-running agent —
chat into it, edit its memory by hand, add instructions to a queue — is a way of
saying something the *next* tick has to reconcile against what it was already
doing. Re-aiming does not have that problem: the goal is read fresh at the top of
every briefing, so a changed goal is simply the goal from then on.

**Editing the goal does not regenerate the worker's system prompt.** That prompt
describes how to work in this project's domain; the goal is injected into every
briefing anyway, so a re-aimed goal takes effect on the very next tick without
anything being rebuilt. A goal so different that the *domain* changed is a
different project.

**Forcing a heartbeat is a request, not a preemption.** If a tick is already
running, the force is recorded and honoured when that tick ends — two turns of the
same worker running at once is the one thing a project must never do.

### Goal as state

Because the goal is a field, a project survives being re-aimed: it keeps its
instances, its memory, its session, its tasks, its workspace and its history.
Today re-aiming means deleting the goal and losing all of it.

A project whose goal is met goes `done` — and can be given the next goal instead,
arriving with everything it learned about the repo still in hand.

## 7. What stays exactly as it is

Nothing below changes; it is listed so a reader does not go looking.

- **The scratchpad** — still the worker's memory, still the only thing carried
  between contexts, still prose. Only `## Plan` is rendered rather than written.
- **The task list** — records, evidence-gated, deps derived, `task_dispatch` for
  parallel work, at most one writer in flight.
- **The pre-flight** — the cheapest tick is the one that never calls a model.
- **Rails** — max ticks, no-progress streak, compute budget, deadline, open PRs.
  Every one of them blocks rather than ends: running out of rope is a reason to
  ask a person, not to disappear.
- **The workspace** — a cache, hibernated by the runner every tick. The branch and
  the scratchpad are the truth.

## 8. Order of work

| # | Step | Notes |
| --- | --- | --- |
| 1 | **Rename** `Goal` → `Project` across store, tick, tools, API, personas, docs, and `metalcraft-front` | Nothing has shipped — the branch is unmerged, so this costs no migration. Frees `ProjectSnapshot` → `PodSnapshot`, which is what it always was. |
| 2 | **Conductor**: instance, persona, its own tool surface (`task_*`, `project_complete`, `project_block`, pace) | The plan moves off the worker |
| 3 | **Boot**: conductor writes the worker's system prompt; store it as state | |
| 4 | **Session**: one per project, context reset per heartbeat, turns posted into it | Uses `ChatSession`'s existing transcript/state split |
| 5 | **Briefing**: conductor composes it each tick; runner appends the constant contract; store it on the journal entry | Retires `TickKind` |
| 6 | **15m default**, and let the conductor adjust the pace | |
| 7 | Front: projects view, the session read-only as the transcript, and the three levers — edit goal, force heartbeat, set period | |
