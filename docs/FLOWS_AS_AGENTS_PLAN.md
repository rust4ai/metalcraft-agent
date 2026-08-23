# Flows as agents — the pod half

> Continues [FLOWS_AND_AGENT_PRESETS_PLAN.md](FLOWS_AND_AGENT_PRESETS_PLAN.md), which
> established the model and landed phases AP2–AP4. This plan closes the two things that
> stop a flow-owned agent from *behaving* like an agent, and builds the endpoints its
> client half needs.
>
> **Client half:** `~/ai/metalcraft-front` → `PLAN.md` §10.7 / §11 P9, `UI_PLAN.md` S9.
> Written together; either can land first, but §3 blocks the client entirely.
>
> **The change in one line:** an armed flow already *is* a persistent agent — make it
> look like one by giving every firing a conversation, and give the client a way to see
> flows at all.

---

## 1. Where this picks up

AP2–AP4 are done, and the model is not in question:

- `InstanceOrigin::Flow { flow_id }` is a first-class origin and
  `defaults_persistent()` (`src/agent_instance.rs:34`, `:47`).
- **Arming is what creates the agent** — `flow_bindings::arm()` mints a persistent
  instance named `"<Preset> — <schedule>"`, schedules of one flow share it, disarm keeps
  it and its memory (`src/flow_bindings.rs:198`).
- The daemon resolves `instance_for(flow, schedule)` and threads it through
  `run_flow_v2_as` → `FlowExecutor::with_instance` → `RunOneShotRequest.instance_id`, so
  a scheduled run recalls from and captures into that agent's memory
  (`src/daemon.rs:311`, `src/flow_exec.rs:1076`).
- Deleting an agent that still runs a schedule is refused with a 409 that names the
  flows (`src/workshop_api.rs:1642`).
- `GET /api/v1/agents/instances/{id}/flows` answers "what is this thing scheduled to do".

Two gaps remain, and they are different in kind:

| Gap | Kind | Effect |
|---|---|---|
| **No conversation per firing** — AP4's own "Remaining" note | half-built feature | A flow agent lists `conversation_count: 0` and opens to an empty transcript. It is an agent that has never visibly done anything. |
| **No `GET /api/v1/flows`** | never built | A client cannot enumerate flows *at all*. Only `/flows/{id}` exists — you must already know the id. |

Everything else in this plan is consequence or polish.

---

## 2. Decisions this plan is built on

Settled with the client half; recorded here so the pod does not re-litigate them.

### 2.1 The pod says *flow*; the UI says *Automation*

No rename on the wire. `SavedFlow`, `/api/v1/flows`, `flow_bindings`, `FlowRun` all keep
their names — they describe a graph, which is what they are. The desktop client labels
the surface **Automations**, because what a user arms is not a graph, it is a standing
instruction. Two words for two altitudes is correct here; one word forced across both
would make either the API vague or the UI jargon.

### 2.2 An armed flow is an agent — not a parallel kind of thing

This resolves open question §10.1 of the previous plan (*"does a flow-owned instance show
up in the agent list?"*): **yes, one list.** A flow-born instance has its own memory, its
own persona, its own conversations, and answers to `PATCH /agents/instances/{id}` like any
other. A second list of "active flows" beside the fleet would be two doors onto one room —
and would split provenance, so "why does this agent know that?" becomes unanswerable in
whichever surface you happen to be standing in.

What *is* a distinct object is a **run**: transient, has a status, and can be paused
awaiting a human. Runs get their own surface (`/api/v1/flow-runs`, already built and
already unread by anything).

### 2.3 A run is a conversation — including a manual run, when there is an agent to run as

The rule, stated once:

> **A run creates a conversation exactly when it has an instance to create it in.**

- Scheduled firing of an armed schedule → has an instance → conversation.
- `POST /flows/{id}/run` on a flow with an armed schedule → runs as that agent →
  conversation.
- `POST /flows/{id}/run` on an unarmed flow, or with no resolvable instance → no
  instance, no conversation, memoryless. **Exactly today's behaviour.**

That last line is what makes this safe. The worry about manual runs — *ad-hoc runs now
quietly mutate the memory of an agent you chat with* — only applies to flows somebody
already armed, which is the deliberate act that said "yes, this runs as that agent".
Testing an unbound flow stays a test.

---

## 3. A — `GET /api/v1/flows` *(blocks the client; do first)*

There is no list endpoint. `openapi.json` confirms it: `/api/v1/flows/{id}`,
`/flows/install`, `/flow-runs`, `/flow-templates` — no `/flows`.

```
GET /api/v1/flows  →  { flows: [FlowSummary] }

FlowSummary {
  id, name, enabled,
  preset:      String,            // flow_bindings::preset_for()
  schedules: [{
    id, name, trigger, enabled,   // from SavedFlow::effective_schedules()
    instance_id: Option<String>,  // flow_bindings::instance_for() — armed or not
    next_fire_at: Option<String>, // reuse schedules/preview's computation
  }],
  armed: bool,                    // any schedule bound
  node_count: usize,
  updated_at,
}
```

Notes for the implementer:

- `metalcraft_flows::list_flows(&paths::flows_dir())` already returns summaries; this
  handler is a join of that with `flow_bindings::get()` and the preview computation in
  `get_flow_schedules_preview` (`workshop_api.rs:2574`). Factor the next-fire calculation
  out of that handler rather than duplicating the cron parse.
- **Do not filter to enabled flows.** `load_enabled_flows` is the daemon's view; a client
  managing automations needs the disabled ones most of all — a pack ships flows disabled,
  and an unarmed flow is precisely what the arm dialog exists to act on.
- `instance_id` goes on the wire because the client's whole story is "this automation runs
  as *that* agent, click through to it". It is pod-local, which is fine: this endpoint is
  not an export path. (§3.2 of the previous plan bans it from *published* flows, not from
  the local API.)

**Size:** ~60 lines plus the extracted helper. No new storage, no new types beyond the
response shape.

---

## 4. B — one conversation per firing *(the load-bearing change)*

Today a flow run writes a diagnostics session and nothing else: `grep chat src/flow_exec.rs`
returns one comment. The run is real, the memory capture is real, and none of it is
*visible* as the thing the agent did.

### 4.1 What changes

`FlowExecutor` gains an `Option<String> chat_id`, created **lazily on the first node that
runs a turn** (`prompt`, `branch`, `sub_agent`) rather than at run start:

- A tool-only flow never opens a conversation. Do not create empty chats for a graph that
  never spoke.
- The chat is created with `instance_id` set, so it lands in `conversations_of(instance)`
  (`workshop_api.rs:1523`) and the agent stops reading as inert.
- Every turn in that run then publishes `ChatEvent`s to `chat_event_sender(chat_id)`.

The template already exists: `deliver_followup_to_chat` (`workshop_api.rs:4587-4693`) builds a
`ReplySink` over the chat's broadcast sender, emits `TurnStarted` with a synthetic user
message (`"⏰ scheduled follow-up: …"`), runs the turn, persists, and sends `Done`. A flow
firing is the same shape with `"▶ <flow name> · <schedule name>"` as the opening line.

### 4.2 Why this is worth its cost

Because the client is already written for it. `metalcraft-front/src/stores/sessions.ts:15`
says, today, before any of this exists:

> *"Live frames arrive on `session://{chat_id}` whether this client drove the turn or not,
> so a session opened while the agent is mid-turn (**fired by a schedule**, a gateway
> message, another device) simply joins in progress."*

So a 3am cron becomes something you can watch replay live in the session view, and the
client needs **no new streaming code, no new event type, no polling**. One chat per firing
converts the entire existing transcript stack — reducer, tool cards, trace collapsing,
right rail — onto flow runs for free.

### 4.3 Conversation volume — the one thing to get right

"A new conversation per firing" is correct for a daily briefer and wrong for a
five-minute cron, which would mint 288 chats a day and bury the agent's real threads.

**Rule: reuse the instance's most recent conversation if it is younger than the gateway
session TTL; otherwise start a new one.** This is not a new invention — it is the rule the
pod already uses everywhere else to decide whether something is still the same
conversation (`DEFAULT_GATEWAY_SESSION_TTL_SECS`, and the follow-up policy in §5.3 of the
previous plan: *"fires within the conversation's TTL → resumes that conversation; fires
later → a new conversation in the same instance"*). Applying it here makes flows consistent
with follow-ups and gateway messages rather than special.

Consequence worth stating: a fast cron produces one long rolling conversation, which is
what you actually want to read.

### 4.4 Also record it

- `FlowRun.chat_id` and `FlowRunSummary.chat_id` — so a run in `/api/v1/flow-runs` links
  straight to its transcript. `FlowRun` already carries `instance_id`
  (`src/flow_runs.rs:71`); this is the same field one level down.
- `SessionInfo` already carries `flow_id` + `instance_id` (`src/diagnostics.rs:33-37`);
  add `chat_id` so diagnostics and transcript cross-reference both ways.
- **Resume lands in the same conversation** (§5.2 of the previous plan). `FlowRun` snapshots
  the flow at pause time already; `chat_id` rides along, so a run that paused Monday and
  resumes Thursday continues the thread instead of resuming into nothing.

**Size:** medium. One threaded field, one lazy constructor, one sink — but it touches the
executor's three turn-running node types and the resume path.

---

## 5. C — run now, as the bound agent

`RunFlowRequest` (`workshop_api.rs:3235`) has `persona_slug`, `model_name`, `inputs` — no
instance. So `post_run_flow` calls `run_flow_v2` (the `None` variant) and a manually
triggered run of an armed flow is memoryless and invisible, unlike the same flow firing
itself sixty seconds later. That is a surprising difference for the most obvious button in
the UI.

```rust
struct RunFlowRequest {
    // …
    /// Run as this agent. Omitted → resolve from the flow's armed schedules;
    /// nothing armed → run memoryless, as today (§2.3).
    #[serde(default)]
    instance_id: Option<String>,
}
```

Resolution order: explicit `instance_id` → the flow's single armed instance → `None`.
Where a flow has several armed schedules pointing at different agents, require the
explicit field rather than guessing; the client has a picker for exactly this.

**Size:** small, once B exists. Without B it is half a feature — the run would recall from
memory but still leave no transcript.

---

## 6. D — the arm consent summary is **already built**

*Corrected 2026-08-22: the previous plan's §6 was implemented and this document's first
draft said otherwise. Recorded rather than quietly deleted, because "we should build X"
and "X exists and nobody consumes it" call for opposite work.*

`GET /api/v1/flows/{id}/binding` already returns `FlowBindingView`
(`workshop_api.rs:2340`), and it carries the whole dialog:

```rust
struct ArmConsent {          // workshop_api.rs:2361
    preset_name: String,
    domains: Vec<String>,          // origins its tools can reach
    requires_env: Vec<String>,     // credentials it will use
    missing_env: Vec<String>,      // ...that this pod does not have
    mutating_tools: Vec<String>,   // tools that change something on the other end
    tool_count: usize,
    base_memories: usize,          // seed memories; it accumulates more each run
}
```

…plus `personas: [FlowPersonaCheck]` (each with `allowed`, i.e. the containment rule
evaluated per persona) and `armed: [ArmedSchedule]` with `instance_id` + `instance_name`.
`missing_env` is the sharpest field on it — credentials whose absence would otherwise
surface "at 3am rather than at a moment anyone is looking", in the handler's own words.

**So D is client work, not pod work.** The one pod-side nicety left: `POST …/arm` returns
a bare `AgentInstance`, so a client that wants to show what it just permitted must re-`GET`
the binding. Echo the `FlowBindingView` in the arm response and that round-trip goes away.
*(small, optional)*

Its shape also settles a question for §3: `ArmedSchedule` already pairs `instance_id` with
`instance_name` ("absent if the instance was deleted out from under the binding"), so the
list endpoint uses the same pair rather than inventing a second way to say the same thing.

## 7. E — instance status (optional, unblocks a nicer fleet)

`metalcraft-front`'s fleet store admits it fakes status: the pod does not report busyness,
so "is this agent running?" means holding an SSE per chat. For flow agents this matters
more than for chats — the interesting moment is precisely when you are *not* looking.

`GET /api/v1/agents/instances?with_status=1` → `{ busy, last_run_at, last_status }`.
Already logged as `metalcraft-front` PLAN §12.5; repeated here because B makes it cheap
(the chat's broadcast sender knows whether a turn is in flight).

---

## 8. What this plan explicitly does not do

- **No separate "active flows" object.** §2.2.
- **No hiding flow agents from the fleet.** A background agent you cannot see is the
  failure mode this whole design exists to avoid.
- **No new SSE stream.** Flow runs ride the chat bus that already exists.
- **No `metalcraft-flows` crate release.** `SavedFlow.preset` still lives in the
  `flow_bindings.json` sidecar (§3.1 of the previous plan). Nothing here needs the field
  to move; when 0.5.0 ships, `bind_preset` writes through and the sidecar keeps only
  `instances` — which stays sidecar-only **permanently**, by §3.2's argument that a field
  which must be stripped on every publish will one day be published by accident.

---

## 9. API delta

```
GET   /api/v1/flows                       NEW   §3 — list; the client is blind without it
POST  /api/v1/flows/{id}/run              + instance_id (§5)
GET   /api/v1/flow-runs                   + chat_id (§4.4)
GET   /api/v1/agents/instances            + ?with_status=1 (§7, optional)
```

Unchanged and already sufficient — note `/flows/{id}/binding` **already serves the arm
consent summary** (§6): `/flows/{id}/schedules*`, `/flows/{id}/binding`,
`/flows/{id}/schedules/{sid}/arm` (POST + DELETE), `/flow-runs/{id}/resume`,
`/agents/instances/{id}/flows`, `/agents/instances/{id}/conversations`.

`openapi/openapi.json` is committed and must be regenerated with the router (it drifted
once already — see `687e996`).

---

## 10. Acceptance tests

- `GET /api/v1/flows` lists **disabled** flows, and reports `instance_id` for an armed
  schedule and `null` for an unarmed one.
- A scheduled firing of an armed flow produces a conversation in its instance; the instance's
  `conversation_count` goes 0 → 1 and the transcript holds the run.
- A second firing **within** the TTL appends to that conversation; a firing **after** it
  starts a second one (§4.3).
- A tool-only flow (no prompt/branch/sub_agent node) creates **no** conversation.
- A client subscribed to `GET /chats/{id}/events` receives the firing's frames without
  having initiated the turn.
- A run that pauses on `approval` and resumes three days later resumes **in the same
  conversation**.
- `POST /flows/{id}/run` on an armed flow runs as its agent and leaves a conversation;
  on an unarmed flow it runs memoryless and leaves none.
- `GET /flows/{id}/binding` names every persona in the roster path with its `allowed`
  verdict, and flags the non-read-only tools (**passes today** — the client just has to
  read it).
- Disarm → the conversations and memory survive; re-arm reuses the same agent.

Existing coverage to keep green: `tests/flow_binding_test.rs`, `tests/flow_pause_resume.rs`,
`tests/flow_preset_autobind_test.rs`.

---

## 11. Phasing

| | Scope | Size | Unblocks |
|---|---|---|---|
| **A** | `GET /api/v1/flows` (§3) | small | the entire client half — do first |
| **B** | conversation per firing (§4). Completes AP4's "Remaining" | medium | live transcripts, the fleet stops lying |
| **C** | run-now as the bound agent (§5) | small | the Automations view's primary action |
| **D** | ~~arm consent summary~~ — **already built** (§6); optionally echo `FlowBindingView` from `POST …/arm` | none / tiny | the arm dialog, which is client work |
| **E** | instance status (§7) | small | fleet status dots |

A → B is the recommended order even though B is bigger: with A alone the client can list
automations, but every one of them opens onto an agent with nothing to show.

---

## 12. Open questions

1. **Is the gateway TTL the right conversation boundary for flows?** (§4.3.) It makes flows
   consistent with follow-ups and gateway messages, which is the argument for it. The
   argument against: a daily briefer's TTL-expired firings each become a conversation, so a
   year of briefings is 365 threads. Alternative is an explicit per-schedule policy
   (`conversation: "per-run" | "rolling"`), which is more honest and more knobs.
2. **Should `POST /flows/{id}/run` on an unarmed flow offer to arm?** The client can ask
   ("run this as an agent so it remembers?"), but that turns a test run into a commitment
   and probably belongs in the arm dialog only.
3. **Uninstalling a pack whose flow is armed** is refused today, because the flow's instance
   is persistent (previous plan §10.3). Should disarm be offered inline in that refusal?
   The 409 currently names the flows; naming them and offering the fix is one step better.
4. **Fan-out.** §3.3's default — schedules of one flow share an agent — is right for the
   8am/6pm case and wrong for "run once per customer". Revisit if `foreach` lands.
