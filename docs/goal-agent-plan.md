# Goal Agents

**Status:** G1 built, and G2's workspace half, on branch `goal-agents` — the
store, the scratchpad, the four `goal_*` tools, the 30-minute heartbeat, the
journal, the REST surface, a small buildr.space client, the model-free
pre-flight, hibernate enforcement, tier escalation on a no-progress streak, and
announcing a blocked or finished goal to the person who set it. What remains is
the agent-facing half of G2 (provisioning and cloning a workspace, the
`goal-agents` pack), G4, G5, B1 and F1. Supersedes the first draft
(`GOAL_AGENTS_PLAN.md`, deleted).

Two things landed differently from the plan, both deliberately:

- **The journal is a structured `journal.jsonl` per goal, not a chat.** A tick
  summary is not a conversation turn — it has a tick number, a kind, a model, a
  duration and a progress pair, and the screens that render a goal draw a
  timeline from those fields rather than from prose. Questions to a person still
  go out through the goal's `IoBinding`; §11.2's "journal chat" proposal is
  answered by this rather than deferred.
- **The heartbeat bookmark is `counters.last_tick_at` on the goal record**, not
  the separate bookmarks file scheduled flows keep. A goal is one recurring thing
  with one interval, so its own record is the natural place — and it survives a
  restart for free, which is the bug that file exists to fix.

A third kind of agent, alongside the chat agent and the gateway agent: an agent
whose reason to exist is **one goal string**, which wakes on a **30-minute
heartbeat**, does the next slice of work, writes what it learned into a
**scratchpad**, and goes back to sleep. It stops when the goal is met, when it is
blocked on a human, or when it runs out of rope.

It does its work in a **buildr.space workspace**, not on the pod.

Two goals are expressible with the same primitive:

- **Build** — "ship the Stripe billing subsystem in `rust4ai/foo`, phase by phase."
- **Audit** — "review `rust4ai/bar` for correctness and cleanup, open one PR per
  accepted finding."

---

## 1. Why an agent, not a flow

The instinct is to reach for `metalcraft-flows`, because a flow is already a state
machine with cycles and a scheduler. It is the wrong fit, and the reason decides
the whole design:

**A flow graph is authored before the work is understood.** Its nodes are fixed at
write time. A goal's plan is *discovered* — you cannot draw the graph for "ship
billing" until the agent has read the repo, and the graph changes every time a
phase turns out to be three phases. Encoding that in a flow means the flow
rewrites itself every tick: a state machine pretending to be a scratchpad.

The graph-of-agents a goal needs already exists, and it is not the flow engine —
it is **`sub_agent` + the preset roster** (`src/tools/sub_agent.rs`,
`agent_preset::PersonaRole`). An orchestrator persona already fans work out to
specialists, already gets `handoff` blocks back reporting what was left undone,
and already cannot close a turn with open plan steps (`src/turn_plan.rs`). One
tick of a goal agent *is* one orchestrated turn. We are not building an
orchestration engine; we are building the thing that **keeps calling it**, and
the state that survives between calls.

Flows keep their job: deterministic, author-known pipelines. Goals are for open
work.

---

## 2. Where the work happens: buildr.space

The pod is the wrong place to build software. It has one filesystem shared with
the agent's own data dir, a `bash` tool capped at 300 seconds, no repo checkout,
and a PVC that was never sized for a `target/` directory. buildr.space exists
precisely to be the other end of this: it provisions a sprites.dev box, clones
repos into it, and exposes files/exec/git/build/test/serve over HTTP.

**So the pod agent holds no code.** It is a thin orchestrator that owns a goal, a
scratchpad, and a workspace id. Every byte of source lives in the workspace or on
GitHub. This removes three gaps the first draft had to open (a `cwd` bash never
honoured, a 300s ceiling, a per-goal workspace directory on the PVC) and replaces
them with one integration that is already built.

The `buildr-space` agent pack (`axoniac-seeded-agent-packs/packs/buildr-space`,
v0.5.0) ships 32 HTTP tools — `buildr_create_workspace`, `buildr_clone`,
`buildr_read_file` / `buildr_write_file`, `buildr_exec`, `buildr_git`,
`buildr_build` / `buildr_test` / `buildr_get_run`, `buildr_serve` / `buildr_fetch`,
`buildr_render` / `buildr_browser_act`, `buildr_hibernate_workspace` /
`buildr_wake_workspace` — plus a `buildr-space-agent` persona and skill. The goal
agent's roster includes that persona as a subagent; the goal agent itself mostly
decides *what* to do and delegates *doing* it.

### 2.1 Five facts about buildr.space that shape the design

1. **Billing is awake minutes.** Free 300/period, premium 2400 (40h), and the
   reaper hibernates an account that spends its allowance *while it is working*.
   Compute, not tokens, is a goal's scarcest resource.
2. **Idle hibernate is 10 min free / 30 min premium.** With a 30-minute
   heartbeat, a workspace left running is billed for the whole gap. **Every tick
   must hibernate its workspace before it ends** — see §4.2.
3. **Workspaces are capped at 1 free / 5 premium.** That is a hard ceiling on
   concurrently *workspaced* goals, and it must be enforced at goal creation with
   a real message rather than discovered as a 403 on tick one.
4. **`build`/`test` are background runs.** They open a `runs` row, follow the
   command in a background task, and answer with the row as it stands after
   `wait_secs`. A caller that hangs up does not lose the result. This is a gift
   to a heartbeat design — see §4.3.
5. **Git ops are `status | diff | commit | push | pull`.** There is no branch
   creation and **no pull-request endpoint**, and the GitHub App is installed with
   Contents/Workflows/Metadata only — *not* Pull requests. So buildr cannot open
   a PR today. See §6.3.

### 2.2 Credentials

`BUILDR_API_KEY` (a `bsk_` PAT) in the pod key store is the simple path. Better:
buildr.space accepts `Authorization: Bearer mck_…` for a **linked** Metalcraft
account (`docs/METALCRAFT_LINK.md` there) — link once from the account page and
no pod needs a `bsk_` copied into it. Recommend the link, keep the PAT as the
fallback the pack already declares.

---

## 3. Data model

### 3.1 `InstanceOrigin::Goal`

```rust
pub enum InstanceOrigin {
    Workshop,
    Cli,
    Gateway { channel: String, sender: Option<String> },
    Flow { flow_id: String },
    Goal { goal_id: String },      // new
}
```

One goal owns exactly one agent instance for its whole life. That is what makes
"it remembers what it tried last Tuesday" true — instance memory, `mem_*` tools
and `prompt_extras` all work unchanged, because they key off `instance_id`.

### 3.2 The goal record — `<data>/goals/<id>.json`

```jsonc
{
  "id": "goal_9f3…",
  "title": "Billing subsystem",
  "goal": "Ship Stripe billing in rust4ai/foo: checkout, webhooks, reconciliation.",
  "kind": "build",                        // build | audit — picks persona + tick frame
  "instance_id": "inst_…",
  "agent_preset": "goal-builder",

  "workspace": {                          // buildr.space, not a local path
    "id": "ws_…",                         // null until first provision
    "repos": [{ "full_name": "rust4ai/foo", "dir": "foo", "branch": "goal/billing" }],
    "last_provisioned_at": "…"
  },

  "status": "active",                     // active | blocked | paused | done | failed
  "heartbeat": { "every_minutes": 30, "timezone": "America/Detroit" },
  "io": { "kind": "workshop_chat", "chat_id": "chat_…" },

  "rails": {
    "max_ticks": 200,
    "max_consecutive_no_progress": 3,
    "compute_minutes_budget": 600,        // buildr awake minutes this goal may spend
    "max_open_prs": 3,
    "deadline": "2026-10-01T00:00:00Z"
  },
  "counters": {
    "ticks": 12, "no_progress_streak": 0,
    "compute_minutes_used": 87, "tokens_spent": 1840000,
    "last_tick_at": "…"
  },
  "pending_run": { "workspace_id": "ws_…", "run_id": "run_…", "what": "cargo test" },
  "created_at": "…"
}
```

`status` is the only field the daemon reads to decide whether to fire.

### 3.3 The workspace is a cache, not state

**The truth is the branch on GitHub plus the scratchpad. The workspace is a
convenience that may vanish.** On the free plan a workspace is deleted after 7
days hibernating; any plan can have one reaped, or the sprite can end up in a bad
state that is cheaper to throw away than to debug — which is buildr.space's own
stated posture.

So every tick begins by *reconciling*, not assuming: if `workspace.id` is null or
`buildr_get_workspace` 404s, the tick's job is to re-provision and re-clone at the
goal's branch, note it in the scratchpad, and continue. A goal that cannot
survive losing its workspace is a goal that dies over a weekend.

The corollary is a discipline, not a hope: **uncommitted work does not exist.**
A tick that leaves changes uncommitted has lost them. Commit and push at the end
of every tick, even mid-phase, on the goal's own branch (`goal/<slug>`).

### 3.4 The scratchpad — `<data>/goals/<id>/scratchpad.md`

**The load-bearing wall.** Not instance memory: memory is fuzzy, recall-ranked
and lossy on purpose. A goal needs state that is *verbatim, complete, and injected
every single tick*. Markdown, because the model maintains it and a checkbox list
is the cheapest plan format a model reliably keeps correct.

```markdown
## Goal
<the goal string, verbatim>

## Workspace
buildr ws_7c2… · repo `foo` at `/workspace/foo` · branch `goal/billing`
(if this is gone: create a workspace, clone rust4ai/foo, checkout goal/billing)

## Plan
- [x] 1. Read the repo, write the schema
- [ ] 2. Checkout + webhook endpoint      ← current
- [ ] 3. Reconciliation job

## State
Migration 0004 applied. `cargo test` green as of tick 11. Pushed through 3f9a1c2.

## Log
- t11 (2026-09-02T14:00Z): webhook handler + 3 tests, pushed, hibernated. Next: signature verification.
- t10: …                      (bounded — oldest entries roll off past ~40 lines)

## Blockers
(none)

## Questions for the human
(none)
```

Injected verbatim into every tick, hard-capped at ~12 KB; on overflow the `Log`
tail is trimmed first, then the tick is told to compact it itself.

Writer tools, and no more than these:

- `goal_note { section, text }` — append to `Log` / `Blockers` / `Questions`.
- `goal_scratchpad_write { markdown }` — replace the document (the tick's final act).
- `goal_block { reason, question }` — stop the heartbeat, ask the human.
- `goal_complete { summary }` — the goal is met.
- `goal_finding { … }` — audit goals only (§6.2).

---

## 4. The tick

One heartbeat = **one bounded agent turn, in a fresh conversation**, with the
scratchpad as the only carried state.

Fresh-per-tick is deliberate. Continuing one ever-growing chat means token cost
climbs every tick until compaction starts destroying exactly the detail the goal
depends on. Statelessness is what makes the scratchpad matter, and it keeps cost
per tick roughly flat — the difference between a goal you can leave running for a
week and one you cannot.

### 4.1 Shape

```
daemon iteration
  └─ goals::load_due()                    // schedule_timing bookmarks, key "goal:<id>"
       └─ tick(goal):
            1. load scratchpad
            2. run_one_shot_task(
                 persona  = goal.kind's persona,
                 instance = goal.instance_id,        // memory + prompt_extras
                 task     = TICK_FRAME + scratchpad)
            3. read the outcome:
                 goal_complete   → status = done, report through `io`
                 goal_block      → status = blocked, ask through `io`
                 scratchpad same → no_progress_streak += 1
                 else            → streak = 0
            4. rails: ticks / streak / deadline / compute exceeded → status = blocked
            5. append one line to the goal's journal chat
```

The tick frame, roughly:

> You are working toward a long-running goal. Below is your scratchpad — the only
> memory you carry between sessions. **Do one slice of work this tick, not the
> whole goal.** First reconcile your workspace (it may be gone; re-provision if
> so) and read any pending run. Then take the first unchecked plan step. If the
> plan is empty or stale, this tick's work is to write it. Do the work; verify it
> — a build that compiles is not a feature that works, and **you may never check
> a box you did not verify**. Commit and push before you finish: uncommitted work
> does not exist. Hibernate the workspace. Then rewrite the scratchpad so the next
> tick, which knows nothing you know now, can continue. If you are blocked on a
> human decision, call `goal_block` rather than guessing.

Two existing mechanisms keep this honest rather than drifting: `update_plan` plus
the `say_to_user` gate stop a tick from declaring victory one step in, and the
`handoff` block means a delegation that only got halfway says so instead of
reading as done.

### 4.2 Hibernate discipline

The last workspace action of every tick is `buildr_hibernate_workspace`. Not an
optimisation — with a 30-minute heartbeat and a 10-minute free-plan idle timer, a
tick that forgets bills the gap to the account, and the reaper's own hibernation
happens *after* the money is spent. The tick runner enforces it rather than
trusting the prompt: if the goal has a workspace and the turn ended without
hibernating it, the runner calls hibernate itself and notes it in the log.

Waking is the reverse and cheaper to get wrong: `buildr_wake_workspace` at the
top of a tick that needs the box. A **sweep tick on an audit goal needs no
workspace at all** (it reads GitHub contents directly) and should never wake one.

### 4.3 A tick may end on a pending run

`buildr_build` / `buildr_test` return a run row that keeps advancing after the
request returns. A cold `cargo build` outlives any sane tick. So:

- A tick that starts a long run records `pending_run` on the goal and **ends**.
- The next tick's *first* act is `buildr_get_run` on it.

This turns the heartbeat into the polling loop the platform otherwise lacks, and
it is why long builds do not need long ticks. A pending run also justifies a
**short-fuse re-tick** (5 min instead of 30) — the one case where the interval
should shrink, because the goal is genuinely waiting on something that will be
done soon.

### 4.4 Evaluation is a prompt swap

Every Nth tick (`review_every: 5`, and always after a phase is checked off) the
frame changes: re-read the goal, audit the work against it, uncheck anything not
actually finished, and add what was missed. Same primitive, different prompt.
That is the work → evaluate → work loop; it costs one `if`.

---

### 4.5 Grooming the scratchpad

The scratchpad is the only state a goal has, which means it is also the only
thing that can rot. It rots predictably: `Log` grows until it crowds out
everything else, `State` accumulates facts that stopped being true four ticks
ago, and `Plan` drifts from the branch — boxes checked optimistically, steps that
turned out to be three steps, work done that nobody wrote down. Left alone for
fifty ticks it becomes a document that costs a lot to read and misleads the tick
reading it.

So it gets **groomed**, and the model for that already exists here: this is what
`memory::dream` does nightly to `capture.jsonl` — distil the episodic into the
durable, merge duplicates, let the rest decay. The scratchpad wants the same
move on a shorter cycle.

**When.** Grooming rides the review tick (§4.4), because auditing the plan and
rewriting the document are the same act. Two hard triggers override the cadence:
the scratchpad passing ~10 KB, or `Log` passing ~40 lines. A goal that thrashes
grooms more often, which is the correct response to thrashing.

**What a groom does**, in order:

1. **Re-derive `Plan` from reality**, not from what previous ticks claimed —
   `git log` on the branch, the diff, the open PRs, the CI verdict. A box stays
   checked only if the work is on the branch and green. This is the step that
   makes optimistic self-reporting self-correcting.
2. **Fold the older half of `Log` into `State`.** Twenty lines of "what I did"
   become three lines of "what is true now". Episodic → semantic, which is
   exactly the dream's move, and it is where most of the size goes.
3. **Retire what is resolved** — answered questions, cleared blockers, findings
   that merged.
4. **Keep decisions taken** (§5) as a durable list. They are the thing a human
   reviews later and the thing a future tick must not re-litigate.
5. **Promote what outlives the goal into instance memory** via `mem_remember` —
   "this repo's tests need `LC_ALL=C`", "the staging deploy needs a manual
   approval". That is the seam between the two stores: the **scratchpad is this
   goal's working state**, verbatim and injected every tick; **memory is what the
   agent knows across goals**, recalled when relevant. A groom is where the
   second is fed by the first, and it means a goal's third repo goes faster than
   its first.

**Grooming is the one operation that can destroy the goal**, so it snapshots
first: the previous scratchpad is kept (`scratchpad.<n>.md`, last ~10) and a
groom may never drop an unchecked plan step, an open blocker, or an unresolved
finding — those are carried forward mechanically by the tick runner, not left to
the model's discretion. A rewrite that loses the plan is indistinguishable from
a goal that finished, which is the worst confusion available in this design.

---

### 4.6 Which model runs a tick

48 ticks a day is the whole cost of this feature, so the model is not a detail.
Three things in the codebase already point at the answer:

- **A tier ladder exists** — `AVAILABLE_MODELS` is `gpt-5.4-mini`, `gpt-5.4`,
  `gpt-5.5` (`src/runtime.rs:22`).
- **Per-step model override is an established pattern** — a flow's `prompt` and
  `branch` nodes already take `data.model`, falling back to the flow's default
  (`flow_exec.rs:741`, `:960`). Per-tick choice is the same move.
- **`ModelFloor` is declared and never resolved.** Presets carry
  `{tier, needs, min_context, prefer}` (`agent_preset.rs:130`) and nothing reads
  it. It was written for exactly this reason — a hard model name breaks on a pod
  that does not have it — and goals are the thing that finally makes it real.

#### The cheapest tick is the one that never calls a model

Before choosing a tier, notice how many ticks need no agent at all. "Is the build
done?", "did CI go green?", "does the workspace still exist?" are HTTP GETs. The
tick runner does that **pre-flight in Rust**:

```
pre-flight (no model):
  workspace still there?      buildr_get_workspace
  pending_run finished?       buildr_get_run
  CI verdict on the open PR?  checks/actions (§6.4)
     ↓
  nothing changed and nothing to do  → re-arm, spend nothing
  something finished                 → promote to a work tick
```

A build goal waiting on a cold `cargo build` or a CI run can otherwise burn
several full turns doing nothing but asking. Making the wait free is worth more
than any tier choice below it.

#### Tier follows the tick kind

| Tick kind | Tier | Why |
| --- | --- | --- |
| **plan** (first tick, or `Plan` empty/stale) | strong | Everything downstream is shaped by this. The most leveraged tokens the goal will spend. |
| **review + groom** (§4.4–4.5) | strong | It rewrites the only state the goal has. A weak model here is how a goal is *lost*, not merely slowed. |
| **work** | standard | The bulk of ticks: take one step, verify it, write it down. |
| **poll** | *none* | Pre-flight only, above. |

The goal record carries an optional `models: { plan, work, review }` override in
tier names — never model ids, so a goal written on one pod runs on another.
`resolve_tier(tier)` maps to what this pod actually has, defaulting to the
ladder; on a managed pod `METALCRAFT_MODEL` is often the sentinel `"default"`,
which the inference gateway resolves to the user's chosen model, so the "standard"
tier should mean *that* rather than a pinned name.

#### Escalate on thrash, rather than paying up front

Static assignment is brittle: a cheap tick that thrashes costs more than the
strong tick it was avoiding. The counter to key on already exists —
`no_progress_streak` (§3.2). **One no-progress tick escalates the next tick a
tier**; a groom resets it. A weak model that gets stuck buys itself one good tick
instead of burning three bad ones, and a goal that is genuinely hard drifts
upward on its own rather than needing a human to notice.

#### The leak worth knowing about

`sub_agent` inherits the parent's `model_name` (`tools/mod.rs`, `ToolConfig`), so
**a tick's tier sets the tier for its whole delegation subtree** — and that is
where the tokens actually go, since a work tick's own reasoning is small next to
the three delegations it makes.

Nothing is being done about this for now, deliberately (§11.1). The tick-kind
tiers above are chosen knowing they price the whole subtree, which is the right
way to read them: a "standard work tick" is not a standard-priced turn, it is a
standard-priced *tree*. If the numbers come back wrong, that is the first place
to look — not the first thing to pre-emptively engineer.

---

## 5. When there is no human in the room

A heartbeat tick has nobody attached, which breaks two things that assume a
person: `ask_user` has nowhere to deliver, and a tick that needs a decision would
otherwise guess. Hence `io: IoBinding` on the goal — reuse the enum
`scheduled_tasks` already has (WorkshopChat / Gateway / Unbound).

**Blocking is a last resort, and the prompt has to say so.** A blocked goal stalls
until a human happens to look, which on an overnight run means eight wasted
hours. The default posture is **decide and record**: make the reasonable call,
write it into `State` with the reasoning, and surface it in the journal as a
decision taken — reviewable after the fact, not a gate before it. Block only when
the call is *irreversible* (deleting data, force-pushing, anything public), spends
money, or would change what the goal means. "Which of two library choices" is a
decision to record; "should this repo drop its Postgres dependency" is a question
to ask.

- **`goal_block`** sets `status = blocked`, **stops the heartbeat**, and delivers
  the question to `io`. A blocked goal costs nothing while it waits — including
  no buildr compute, because its workspace is hibernated.
- The human answers in that chat; the answer unblocks the goal (`active`) and is
  appended to the scratchpad's `State`.
- Rails tripping (out of ticks, no progress 3× running, past deadline, out of
  compute minutes) also lands in `blocked`, never a silent stop. A goal that
  quietly gave up is the worst failure mode available: it is indistinguishable
  from one still working.

---

## 6. The two archetypes

Same machinery; different persona, tools and tick frame. Both ship as one agent
pack (`goal-agents`) whose preset rosters include `buildr-space-agent`.

### 6.1 Build goal (`kind: "build"`)

- **Workspace:** one buildr workspace, repo cloned at goal creation, all work on
  `goal/<slug>`.
- **Roster:** `goal-builder` (default, orchestrator) → `buildr-space-agent` for
  the hands-on work, `research-agent`, and a `reviewer` persona for review ticks.
- **Cadence:** one plan step per tick. Commit + push every tick; open or update
  **one PR per phase**, never per tick — a PR per heartbeat is unreviewable.
- **Verification before a box is checked:** `buildr_test` green, and for anything
  user-facing a `buildr_serve` + `buildr_fetch` (or `buildr_render`) that proves
  the page actually loads. buildr.space's own framing applies: a build that
  compiles is not a site that works.

### 6.2 Audit goal (`kind: "audit"`)

- **Findings ledger** in the scratchpad — each finding gets an id, `file:line`, a
  severity, and a state (`open` → `pr_open` → `merged` / `rejected`). This is the
  dedupe key; without it tick 9 re-reports what tick 4 already opened a PR for.
  `goal_finding` writes it so a client does not have to parse markdown.
- **Sweep ticks and fix ticks alternate.** A sweep tick covers one area (a
  directory, or one lens: correctness, error handling, dead code) reading through
  the GitHub contents API — **no workspace, no awake minutes**. A fix tick wakes
  the workspace, takes the highest-severity `open` finding, writes the change on
  its own branch, verifies it, and opens one PR for that finding alone.
- **`max_open_prs` (default 3).** At the cap the goal keeps sweeping and stops
  opening — twenty simultaneous bot PRs is how a repo learns to ignore them.
  Reconciled each fix tick against the open PR list.
- **A finding not worth a patch becomes an issue**, not a PR (§6.4) — which is
  most of what a first sweep turns up.
- **It runs autonomously.** No approve-each-finding gate: it sweeps, files,
  fixes, opens and keeps moving toward the goal on its own, checking in through
  the journal rather than asking permission. The rails are `max_open_prs`, the
  compute budget, and the one thing it never does — **merge**. A bot that opens a
  PR is a colleague; a bot that merges one is an incident.
- **PR body quotes the finding and what was verified**, so a reviewer sees the
  claim and the evidence without re-deriving either.

### 6.3 Opening the pull request

Neither side can do this today, and it is worth being exact about why, because
the fix turns out to be a few lines of JSON in the right place.

buildr.space's git surface accepts `status|diff|commit|push|pull` and nothing
else. Below it, `installation_token()` (`backend/src/services/github_app.rs:87`)
asks GitHub for a token narrowed to **`{"contents":"write","workflows":"write"}`**,
and the App itself is installed with Contents/Workflows/Metadata. So buildr holds
a credential incapable of opening a PR, issued by an App incapable of granting
one. A route alone would not have helped.

**buildr.space opens the PR.** The App gains the permission, and a new endpoint
uses it:

1. Add **Pull requests: Read & write** (and see §6.4) to the GitHub App.
2. Add `POST /api/v1/workspaces/{id}/pr`, which mints a token carrying
   `{"pull_requests":"write"}` and POSTs `/repos/{owner}/{repo}/pulls` with
   `{title, body, head, base}`.
3. `base` defaults to the repo's `default_branch` — already modelled at
   `github_app.rs:42` — and the request may override it.

Changing an App's permissions means **every existing installation must re-accept
before the new permission reaches it**, and a token request naming a permission
the installation lacks is refused outright. Pre-launch that is a non-event: the
installed base is small and known, and it is exactly why this is the moment to do
it (§6.4). It does shape one implementation detail — mint the PR token
**separately** from the git-op token rather than widening the existing mint. Then
an installation that has not re-accepted keeps pushing and loses only PR
creation, with an error that can say so. `installation_token()` has to grow a
permissions parameter either way, so the separate mint is close to free, and it
is the pattern every future permission addition will want.

The pod's `github` pack can open a PR with a `GITHUB_TOKEN` PAT and remains the
fallback if this slips — but it is not the plan. Doing it properly means one
credential instead of two, no PR-scoped PAT sitting in the pod key store, and PRs
attributed to an app rather than to you personally, which is what you want when
an audit goal is opening them continuously.

### 6.4 Spend the re-consent once

Re-accepting is a one-time cost per installation, and it costs the same whether
the change adds one permission or four. Pre-launch is the only moment it is
nearly free — so add everything a goal agent will plausibly need, not just the
one thing that unblocks G4:

| Permission | Why |
| --- | --- |
| **Pull requests: write** | Open the PR (§6.3). Also PR review comments — a goal that replies to feedback on its own PR needs this, not `issues`. |
| **Issues: write** | An audit finding that is not worth a patch should become an issue, not a PR. Separate permission from pull requests. |
| **Actions: read** | Read workflow runs and logs on the PR it just opened. `workflows: write` is about editing workflow *files*; it says nothing about reading runs. |
| **Checks: read** | The other half of "did CI go green" — most CI reports arrive as check runs. |

The last two matter more than they look. The build archetype's rule is that a
tick may never check a box it did not verify, and today verification stops at
`buildr_test` inside the sprite. Once a PR is open, **CI is the real verdict** —
and a goal that can read it can close the loop on its own work: open the PR on
one tick, read the run on the next, fix what went red on the third. Without it,
every goal ends at "I pushed something and I have no idea whether it passed."

(`statuses: read` is the third CI shape, for repos still using the commit-status
API. Cheap to include in the same breath; skip it if you would rather keep the
list short.)

#### Two smaller things in the same area

- **There is no branch op**, so creating a branch means `buildr_exec` →
  `git checkout -b`. Add `op: "branch"` to the existing match instead: same shape
  as its neighbours, and it keeps a git-shaped action out of a raw shell.
- **An exec checkout leaves the repo row's `branch` stale.** `push` falls back to
  `target.branch` when the request names none, so after `git checkout -b goal/x`
  a later bare push aims at the *old* branch — silently, and with a plausible
  success. A goal agent must pass `branch` explicitly on every push; better, the
  new `branch` op updates the row, which fixes the footgun for every caller
  rather than documenting it for one.

---

## 7. What has to change outside this feature

Much less than the pod-workspace version needed. The bash `cwd` lie, the 300s
ceiling and per-goal PVC directories are all moot — none of that work happens on
the pod any more.

1. **Sub-agent timeout.** Default 120s, ceiling 1800s (`src/tools/sub_agent.rs`).
   A delegation that drives a buildr build needs the ceiling; `goal-builder`
   declares `max_run_secs` accordingly.
2. **Concurrent-goal ceiling.** Enforced at goal creation against the buildr plan
   (1 free / 5 premium) with a real message, plus `MAX_ACTIVE_GOALS` on the pod.
3. **Cost accounting.** A goal is the first thing on a pod that spends money
   unattended and indefinitely — in two currencies. Tokens are metered by the
   inference layer; awake minutes come back from `buildr_get_workspace` /
   `GET /billing/plan`. Both land in `counters` and both are rails.
4. **The PR path is buildr.space work** (§6.3–6.4), and it gates the audit
   archetype: the App permission batch, `POST /workspaces/{id}/pr` on a
   separately-minted token, a `branch` git op, and the repo-row branch fix. Do
   the permission change before launch, while re-consent is free.

---

## 8. The client is not optional

Goals are created in **`metalcraft-front`** (the Tauri ADE) and the **iOS app**,
and nowhere else. The pod exposes the REST surface in `workshop_api.rs` the way
it does for everything else, but there is no chat command and no agent tool that
mints a goal — so **until a client ships, the feature cannot be used at all.**
That makes the UI part of the first release rather than a follow-on, which is the
opposite of how the other pod subsystems were built.

What each screen owes:

- **Create** — the goal string (the one field that matters, and it wants room to
  write a paragraph), the repo, kind, heartbeat, and the rails. Sensible defaults
  everywhere else: this is a form someone fills in once and then leaves running.
- **List** — every goal with its status, its progress, and when it last ticked. A
  `blocked` goal has to be visually loud; it is the only state that needs a human
  and the whole design leans on someone noticing.
- **Detail** — the journal (one line per tick, the thing you actually read), the
  scratchpad's `Plan` rendered as its checklist, decisions the goal took on its
  own (§5), open PRs and issues, and compute spent against the budget.
- **Reply to unblock** — answering the question in the journal is what sets the
  goal back to `active`. It should feel like answering a message, because that is
  what it is.
- **Pause / resume / stop**, and a visible "next tick at …".

The record therefore carries a derived **`progress`** (checked/total from the
scratchpad plan) and the counters, so no client ever parses markdown to draw a
progress bar. iOS follows front — the same REST, and the phone is where "what did
it do overnight" is actually read.

---

## 9. Phases

| Phase | What lands | Files |
| --- | --- | --- |
| **G1 — the primitive** ✅ | `Goal` store + CRUD, `InstanceOrigin::Goal`, scratchpad + the 4 `goal_*` tools, 30-min heartbeat pass in the daemon, journal chat, REST + OpenAPI | `src/goals.rs`, `src/goal_tick.rs`, `src/tools/goal.rs`, `src/paths.rs`, `src/agent_instance.rs`, `src/daemon.rs`, `src/workshop_api.rs` |
| **F1 — the way in** *(ships with G1; nothing works without it)* | Goal create / list / detail / journal / unblock-reply in `metalcraft-front`; iOS follows | `metalcraft-front`, `metalcraft-mobile` |
| **G2 — a place to work** (half: client, pre-flight, hibernate, compute accounting ✅; provisioning + pack pending) | workspace provisioning + reconcile-or-reprovision, hibernate enforcement, `pending_run` + short-fuse re-tick, compute-minute accounting, the `goal-agents` pack (rosters include `buildr-space-agent`) | `src/goal_tick.rs`, `src/goals.rs`, new pack |
| **G3 — the loop closes** ✅ | review + groom ticks, no-progress detection and tier escalation, model-free pre-flight (§4.6), rails → `blocked`, unblock-by-reply through `io` | `src/goal_tick.rs`, `src/workshop_api.rs` |
| **G4 — audit kind** | findings ledger + `goal_finding`, sweep/fix alternation, PR-per-finding, `max_open_prs`, dedupe via `github_list_pull_requests` | pack skills, `src/tools/goal.rs` |
| **G5 — build kind at depth** | phase → PR mapping, test/serve gating before a box is checked | pack skills |
| **B1 — buildr.space PR path** *(independent of G1–G3; **blocks G4**; do the permission batch now)* | App gains Pull requests write + Issues write + Actions/Checks read; `installation_token()` takes a permissions argument; `POST /workspaces/{id}/pr` on a separately-minted token; `op: "branch"` that also updates the repo row; `GET /workspaces/{id}/checks` for CI verdicts; new pack tools | buildr.space `github_app.rs`, `workspace_ops.rs`, `buildr-space` pack |

G1 alone is demonstrable without buildr at all: a goal that wakes every 30
minutes, thinks, writes it down, and reads back as a legible journal. G2 is what
makes it able to build software.

---

## 10. Decisions taken

- **Heartbeat: 30 minutes default**, 5-minute floor (used only for the pending-run
  re-tick), per-goal override.
- **Fresh conversation per tick**; the scratchpad is the carried state.
- **The workspace is disposable**; the branch and the scratchpad are the truth.
- **Hibernate every tick**, enforced by the runner rather than the prompt.
- **Evaluation is a review tick**, not a second engine — and the same tick grooms
  the scratchpad (§4.5), re-deriving the plan from the branch rather than from
  what earlier ticks claimed.
- **Blocked, never silently stopped** — but blocking is a last resort, and the
  default is to decide, record the decision, and keep moving (§5).
- **Goals run autonomously**: they open PRs and issues without a per-item gate,
  and never merge.
- **Model tier follows the tick kind** (§4.6), expressed in tiers not model ids —
  strong for plan and review/groom, standard for work, *no model at all* for a
  poll tick, and one tier of escalation on a no-progress streak.
- **The user's own buildr.space account pays.** Bring-your-own link (§2.2); their
  plan is their ceiling, and no metering has to be invented.
- **A goal is created by a person, in `metalcraft-front` or the iOS app** — never
  by an agent, and never by another goal's tick (§11). Committing a pod to days
  of unattended spend is a decision a human takes deliberately, on a screen built
  for it; it also ends the recursion question before it is asked.
- **buildr.space opens the PR** (§6.3), not the pod's PAT — one credential, and
  PRs attributed to an app rather than to a person.
- **Spend the re-consent once** (§6.4): add Issues, Actions and Checks in the same
  permission change, so a goal can read CI on the PR it just opened.

## 11. Open gaps and decisions

The shape is settled. What remains is one thing consciously left undone, then
details that can be decided as they arrive — none of them block G1.

### 11.1 Per-delegation model tiers — considered, deferred

**Deferred deliberately, not overlooked.** The idea: `sub_agent` takes an optional
`tier` (`mini | standard | strong`), defaulting to the parent's model so nothing
changes unless a call names one, letting an orchestrator delegate mechanical work
cheaply instead of at its own tier.

Why it was set aside: it puts the model in charge of spend. Even clamped
down-only, every delegation becomes a judgement call made by something with no
cost signal and no reliable sense of how hard a subtask is, on the busiest code
path the pod has — `sub_agent` is used by every orchestrator, chat included. The
blast radius of getting it subtly wrong is the whole pod, and the payoff is an
optimisation nobody has yet measured a need for. Tick-kind tiers (§4.6) and the
model-free pre-flight cut the same cost from the outside, without a model
deciding anything.

What would make it worth revisiting: real numbers from running goals showing that
delegation subtrees dominate, and specifically that they dominate on work whose
cheapness is obvious in advance. At that point the smaller half is available on
its own — a `model_tier` on `Persona`, sitting beside `max_run_secs`, declared by
the persona's author rather than chosen by a model at call time. That version has
no model in the loop and is a different risk entirely.

### 11.2 Smaller, decidable later

- **Concurrency.** The daemon runs due work inline and sequentially, so two goals
  due in the same minute serialize. Fine at first, and safer. Revisit when goals
  outnumber ticks.
- **A goal's own chat, or a journal?** Proposed: a journal chat, one line per
  tick, replyable to unblock. Full tick transcripts stay in `sessions/`
  diagnostics, because nobody will read them.
- **Sharing one workspace across goals** on the free plan (cap: 1). Probably
  "one goal per workspace, queue the rest" — but a repo-scoped shared workspace
  is the escape hatch if that cap bites.
- **Sub-goals.** No for v1: a goal's plan is its phases, and a phase that needs
  its own heartbeat is a second goal a person chose to create in the UI.
- **iOS parity timing.** Front first, iOS right after (§8) — or both at once, if
  the phone is where you will actually watch these run.
