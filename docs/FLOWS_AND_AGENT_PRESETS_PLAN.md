# Flows × Agent Presets — scheduled work belongs to an agent

> Companion to [AGENT_PRESETS_PLAN.md](AGENT_PRESETS_PLAN.md) and
> [FLOWS_ARCHITECTURE.md](FLOWS_ARCHITECTURE.md). Builds directly on
> [FLOW_SCHEDULES_PLAN.md](FLOW_SCHEDULES_PLAN.md), whose `SavedFlow.schedules`
> refactor (implemented 2026-08-11, `metalcraft-flows` 0.4.0, unpublished) is the hook
> everything here hangs on.
>
> **The change in one line:** a flow stops being a free-floating state machine that
> borrows personas, and becomes **an agent doing scheduled work** — bound to an agent
> preset, running as a persistent instance, with each firing a conversation.

---

## 1. What's wrong today

A flow's `prompt`, `branch`, and `sub_agent` nodes each name a persona. The executor
resolves a flow-level default from the entry node (`src/flow_exec.rs:104-105`,
`:133-164`) and lets any node override it (`:520`, `:671-672`, `:710-713`
`Persona::load`). Nothing constrains *which* personas a flow may name — it can reach any
persona on the pod. `flow_exec.rs:119` already carries a "this flow is missing
packs/personas it needs" warning, which is the symptom: a flow has dependencies it can't
express.

Three consequences:

1. **A flow is not owned by anything.** `seed/flow_templates/morning-brief.json` names
   `morning-briefer` in two places and hopes it exists. Install a flow, and whether it
   works depends on the pod's ambient persona set.
2. **Scheduled runs have no memory and no continuity.** Every firing is a fresh
   one-shot. The morning briefer cannot notice it said the same thing yesterday, cannot
   learn that you skip Tuesdays, cannot build on anything. With per-instance memory
   (`AGENT_PRESETS_PLAN.md` §3) that's now a fixable gap rather than a fact of life.
3. **Two execution universes.** Chat, CLI, gateway, and follow-ups all became one path
   in `REFACTOR_unify_turn_path.md` (everything runs through `TurnRunner`,
   `src/runtime.rs:112`). Flows technically pass through it too, but they build their own
   persona/system-prompt context on the way (`flow_exec.rs:710-713`) and carry no session
   identity. They're the last holdout.

---

## 2. The model

```
Agent Preset  "amy-kitchen"
    │  declares  flows: ["sunday-meal-prep"]
    │
    │  arm a schedule  ──────────────────┐
    ▼                                     ▼
Agent Instance  "Amy — Sunday prep"   (created at arm time, persistent)
    │  own memory, survives every run
    ├─ conversation   ← Sunday 8am firing
    ├─ conversation   ← Sunday 8am firing
    └─ conversation   ← the approval that came back Tuesday
```

Three bindings, each at a different moment:

| Binding | Set when | Meaning |
|---|---|---|
| **flow → preset** | authored / installed | which agent this workflow *belongs to*; scopes the personas it may name |
| **schedule → instance** | **armed** | which agent actually runs it, and therefore whose memory it accumulates in |
| **run → conversation** | each firing | one thread, inspectable, resumable |

The middle row is the one that matters: **arming is what creates the agent.** Installing
an agent pack ships flows disabled (`AGENT_PRESETS_PLAN.md` §5.5); arming a schedule is
the deliberate act that says "yes, run this in the background", and that's the natural
moment to mint the instance it runs as.

---

## 3. Data model

### 3.1 `SavedFlow` gains a preset

```rust
// metalcraft-flows
pub struct SavedFlow {
    pub spec_version: String,
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// The agent preset this flow belongs to. Every persona named anywhere in the
    /// graph must be in this preset's roster. `None` = `general-agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    pub schedules: Vec<FlowScheduleSpec>,   // from FLOW_SCHEDULES_PLAN.md
    pub flow: FlowDef,
}
```

Back-compatible: `preset` absent means `general-agent`, and every existing flow keeps
parsing. Validation gains one rule (§4).

> **Implemented as a pod-local sidecar, pending a crate release.** `SavedFlow` lives
> in the published `metalcraft-flows` crate (0.4.0), so this field cannot be added
> from the agent repo. Until `metalcraft-flows` ships it, the binding is stored in
> `<data>/flow_bindings.json` — see `src/flow_bindings.rs`. Behaviour is identical
> for a locally-authored flow; what is missing is **travel**: a flow exported in an
> agent pack does not yet carry its preset, so installing it leaves it bound to the
> default agent until someone rebinds. When the crate releases, `bind_preset` should
> write through to `SavedFlow.preset` and the sidecar keeps only `instances`.

### 3.2 `FlowScheduleSpec` gains an instance

`FLOW_SCHEDULES_PLAN.md` already gives each schedule its own name, toggle, inputs, and
persona. Add the binding:

```rust
pub struct FlowScheduleSpec {
    pub id: String,
    pub name: Option<String>,
    pub schedule_type: ScheduleType,   // manual | minutes | hours | cron
    pub interval: Option<u64>,
    pub cron: Option<String>,
    pub enabled: bool,
    pub inputs: Option<serde_json::Value>,
    pub persona: Option<String>,       // must be in the flow's preset roster
    /// The persistent instance this schedule runs as. Minted at arm time, never at
    /// install time — a published flow must not ship someone else's instance id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}
```

**`instance` is pod-local and never published.** `agentpack_export` and the flows
registry strip it, the same way they'd strip a key. A flow arrives with schedules
disabled and unbound; arming binds them here.

> **Implemented outside `FlowScheduleSpec`, permanently.** Unlike `preset`, this one
> should *not* move into the published type once the crate allows it. A field that
> must be stripped on every publish is a field that will one day be published by
> accident. Keeping instance ids in `<data>/flow_bindings.json` — a file no export
> path reads — makes the guarantee structural rather than procedural.

### 3.3 Default instance policy

Arming a schedule with no `instance`:

1. If another armed schedule **of the same flow** already has an instance → reuse it. Two
   crons on one flow (the 8am and 6pm case `FLOW_SCHEDULES_PLAN.md` exists to support) are
   the same agent by default, so the evening run remembers the morning one.
2. Else create a persistent instance of the flow's preset, named
   `"<Preset name> — <schedule or flow name>"` → *"Amy — Sunday prep"*, with
   `origin: "flow:<flow_id>"` and `persistent: true`.

Both are defaults, both overridable: the arm dialog offers "run as an existing agent"
with a picker, because *"run this flow as the same Amy I chat with"* is a reasonable
thing to want, and it's exactly how you'd get a briefer that knows what you discussed
yesterday.

---

## 4. Containment: a flow may only name personas from its preset

The rule that makes flows ownable, and the same rule already applied to `sub_agent`
(`AGENT_PRESETS_PLAN.md` §1.1) and to personas↔integration packs (§5.3 step 2):

> Every persona named in a flow — the flow-level default, any node's `data.persona`, any
> schedule's `persona` — must appear in the flow's preset `personas[]` roster.

Enforced in three places:

- **`metalcraft-flows::validate()`** — pure, no I/O: collect every persona reference and
  check it against a roster passed in. Unit-testable without a model, consistent with the
  crate's "no LLM, no I/O" contract.
- **Agent pack install** — a shipped flow naming a persona outside its preset fails the
  install, naming both (`AGENT_PRESETS_PLAN.md` §5.3).
- **Flow save / registry install** — same check, surfaced as an error the author can act
  on rather than `flow_exec.rs:119`'s runtime warning.

`flow_exec.rs:119`'s "missing packs/personas" warning becomes mostly dead: dependencies
are now declared by the preset and verified at install. Keep it as a belt-and-braces
runtime check.

**What this buys:** the consent summary shown before arming can be complete. "This flow
runs as Amy, using personas *amy* and *amy-shopper*, which can reach
calendar.metalcraftai.com and api.instacart.com." That sentence is only constructible
because the graph can't reach outside the preset.

---

## 5. Execution

### 5.1 A run is a conversation

`FlowExecutor::run` gains `(instance_id, conversation_id)`. Each firing opens a new
conversation in the bound instance, `origin: "flow:<flow_id>#<schedule_id>"`. Every
`prompt` / `branch` / `sub_agent` node runs its turn through `TurnRunner`
(`src/runtime.rs:112`) **with that instance handle**, which means:

- **recall works** — the node sees the agent's memory: the authored knowledgebase from
  the preset plus everything this instance has learned across previous runs;
- **capture works** — what happens during the run is captured into the instance's delta,
  so tomorrow's firing knows about today's;
- **compaction, the step guard, and diagnostics** all behave as they do everywhere else,
  because it's the same path.

This is the payoff, and it costs one threaded parameter — `flow_exec.rs` already routes
its prompt turns through `TurnRunner`; today it just has no session identity to give it.

`FlowRunSummary` records `instance_id` + `conversation_id` so a run in
`/api/v1/flow-runs` links straight to its transcript.

### 5.2 Pause and resume land in the same thread

`approval` and `wait` nodes are durable (`src/flow_runs.rs`, `/flow-runs/{id}/resume`).
A run that pauses Monday and resumes Thursday resumes **in the same conversation**, so
the model still has the thread it was mid-way through. Today it would resume into
nothing. For human-in-the-loop approval this is the difference between "approve this
thing I no longer have context for" and a coherent continuation.

### 5.3 Scheduled follow-ups

`schedule_followup` (`src/scheduled_tasks.rs`, fired by `daemon.rs`) currently binds to
an opaque `session_binding`. It becomes `(instance_id, conversation_id)`:

- fires within the conversation's TTL → resumes that conversation;
- fires later → a **new conversation in the same instance**, so the agent keeps its
  memory but doesn't pretend to still be mid-thought;
- the instance was reaped (ephemeral, §2.3 of the presets plan) → the follow-up is
  dropped with a diagnostic. **Arming a follow-up should promote its instance to
  persistent** — that's the cheapest fix and it matches intent: an agent that scheduled
  future work is by definition not disposable.

`reschedule_depth` is unchanged.

### 5.4 Daemon changes

`daemon.rs` (poll loop `:266`, `poll_seconds` `:474`) keeps
`state_by_flow: HashMap<flow_id, FlowRunState>` but keys due-checks by
`(flow_id, schedule_id)` — already required by `FLOW_SCHEDULES_PLAN.md`'s N-schedules
goal. Add:

- **Instance resolution before the run**, loading the memory handle (LRU, presets plan
  §3.3). A flow whose bound instance is missing is skipped with a diagnostic, not an
  error — never crash the poll loop on one bad flow.
- **A dream budget.** Nightly dreaming per instance now includes one instance per armed
  schedule. Dream only instances active since the last run, and cap runs per night. Ten
  cron flows should not mean ten LLM consolidation jobs every night regardless of whether
  anything happened.

---

## 6. The arm dialog — the consent moment for background work

Installing an agent pack has a consent moment (domains, credentials, personas). Arming a
schedule is the second one, and today nothing surfaces it. It should state, generated
from resolved data:

```
Arm "Sunday prep"  —  every Sunday at 08:00

  Runs as       Amy  (new agent: "Amy — Sunday prep")
                or run as an existing agent ▾
  Personas      amy → amy-shopper
  Can reach     calendar.metalcraftai.com, api.instacart.com
  Uses keys     METALCRAFT_TOKEN, INSTACART_TOKEN
  Will          place grocery orders (instacart write tools)
  Memory        starts from Amy's 214 knowledge entries, accumulates each run
```

The last two lines are the ones that matter. A scheduled flow acts **while nobody is
watching**, so a mutating tool in an armed flow is a materially bigger commitment than
the same tool in a chat where approval prompts exist. Worth surfacing which of the
reachable tools are non-read-only, using `approval.rs`'s existing `OperationKind`
classification.

---

## 7. Where flows live

| Source | Preset | Notes |
|---|---|---|
| Shipped in an agent pack (`flows/`) | the pack's preset, declared in `agent_presets/<slug>.json` → `flows[]` | installed disabled; validated against the roster at install |
| Installed from `flows.metalcraftai.com` | **chosen at install** — defaults to `general-agent` | the flows registry stays useful: a standalone flow is a *template* you attach to an agent |
| Authored locally | whatever the author picks; `<data>/flows/` | Workshop flow editor gains a preset selector |

`/api/v1/flows/{id}/install-dependencies` changes meaning: instead of installing
integration packs a flow needs, it reports **which preset can satisfy this flow's persona
references**, and offers the ones that can. Integration packs are no longer independently
installable (`AGENT_PRESETS_PLAN.md` §4), so the old behaviour has nothing to do.

---

## 8. API

```
GET   /api/v1/flows/{id}                        + preset, schedules[].instance
PUT   /api/v1/flows/{id}                        validates persona refs against the preset roster
POST  /api/v1/flows/{id}/schedules              create (unarmed)
POST  /api/v1/flows/{id}/schedules/{sid}/arm    { instance? } → mints or binds, returns the consent summary
POST  /api/v1/flows/{id}/schedules/{sid}/disarm keeps the instance and its memory
GET   /api/v1/flows/{id}/preview                the arm dialog's resolved content (§6)
GET   /api/v1/flow-runs                         + instance_id, conversation_id
GET   /api/v1/agents/instances/{id}/flows       what this agent is scheduled to do
```

Existing routes (`src/workshop_api.rs:402-418`) keep working; `PUT …/schedules` gains
`instance` as a read-only field (set by arm, not by the client).

**`GET /api/v1/agents/instances/{id}/flows` is the one worth building early.** "What is
this agent scheduled to do?" is currently unanswerable on a pod, and it's the question
someone asks right before they trust it.

---

## 9. Migration

1. Every existing flow gets `preset: null` → resolves to `general-agent`.
2. **Scan installed flows for persona references.** Any persona a flow names that isn't
   in `general-agent`'s roster is added to the synthesized `my-agent` preset (presets plan
   §7.2 step 2) and the flow is bound to that instead. This case is real, not theoretical:
   `seed/flow_templates/morning-brief.json` names `morning-briefer` twice, and
   `general-agent`'s seeded roster doesn't include it — so either the roster gains it (it's
   first-party, so: yes) or the flow migrates to `my-agent`. Both paths must work, because
   user-authored flows will name user-authored personas.
3. Every **enabled** flow with a schedule gets an instance minted at migrate time —
   `"<Preset> — <flow name>"`, persistent — so nothing that was running stops running.
   Report each one; a user who had six cron flows suddenly has six agents and should be
   told why.
4. Disabled flows stay unbound.

---

## 10. Open questions

1. **Does a flow-owned instance show up in the agent list?** It's a real agent with real
   memory, and hiding it makes "what is this thing doing" unanswerable. But six cron flows
   producing six agents next to your chats is clutter. Proposal: one list, a `background`
   filter on by default.
2. **Can a chat instance and a flow share one agent?** §3.3 allows it by picker, and it's
   the compelling case — a briefer that remembers your conversations. The risk is that a
   background run mutates the memory of the agent you're actively talking to, mid-chat.
   Recall is per-turn so there's no corruption, but the agent could surprise you by knowing
   something you never told it. Probably fine, probably worth a note in the UI.
3. **Does an armed flow keep running after its agent pack is uninstalled?** Uninstall
   refuses while a persistent instance exists (presets plan §5.3), and a flow instance is
   persistent — so today the answer is "uninstall is blocked". Is that the right friction,
   or should disarming be offered inline in the uninstall dialog?
4. **Per-schedule vs per-flow instance defaults.** §3.3 says schedules of one flow share an
   instance. The 8am/6pm case wants that. A "run for each customer" fan-out would want the
   opposite. Revisit if `foreach` lands (FLOWS_ARCHITECTURE §4, planned).
5. **Should `metalcraft-flows` know about presets at all?** §4 puts the roster check in
   `validate()`, passing the roster in as a parameter — the crate stays pure and learns one
   new concept. The alternative is validating entirely agent-side and leaving the crate
   ignorant. Leaning: pass it in, because a visual editor wants the same check.

---

## 11. Phasing

Slots into `AGENT_PRESETS_PLAN.md` §9:

| Phase | Scope |
|---|---|
| **AP2** | ✅ Flow→preset binding + the roster containment rule. Landed as `src/flow_bindings.rs` (see §3.1's note) rather than a `metalcraft-flows` field; checked at bind time *and* at run time, since a preset can lose a persona after the bind. |
| **AP3** | ✅ Schedule→instance binding, arm/disarm endpoints (`/flows/{id}/schedules/{sid}/arm`), instance minting at arm time, `/flows/{id}/binding`. Armed bindings are reconciled whenever schedules are edited, and deleting an agent that still runs a schedule is refused (409). |
| **AP4** | ✅ Executor threads the instance: `run_flow_v2_as` → `FlowExecutor::with_instance` → `RunOneShotRequest.instance_id` → `TurnRunner::with_instance`, so prompt and branch nodes recall from and capture into the flow's agent. Persisted in `FlowRun.instance_id`, so a paused run resumes as the same agent. **Remaining:** one conversation per firing (runs are captured against the instance but not grouped), and the daemon dream budget. |
| **AP6** | Registry flows choose a preset at install; `install-dependencies` reports preset coverage. |
| **AP7** | Arm dialog with the consent summary; `instances/{id}/flows`; flow editor preset selector. **Workshop repo.** |

---

## 12. Acceptance tests

- An existing flow with no `preset` runs unchanged after migration.
- A flow naming a persona outside its preset roster fails to save, fails to install, and
  names both.
- Arming a schedule mints exactly one persistent instance; arming a second schedule on the
  same flow reuses it.
- Two firings of a cron flow produce two conversations in one instance, and the second
  firing's prompt can recall something written during the first.
- A run that pauses on `approval` and resumes 3 days later resumes in the same conversation.
- A follow-up armed inside a flow run promotes its instance to persistent.
- `schedules[].instance` is absent from an exported `.agentpack` and from a registry-published
  flow.
- Disarming leaves the instance and its memory intact; re-arming reuses it.
- A flow whose bound instance is missing is skipped with a diagnostic; the poll loop survives.
