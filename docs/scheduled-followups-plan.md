# Scheduled follow-ups ("wake me up and re-run this")

## Problem

The agent cannot defer work. When a task needs a wait ("the domain should be
live in a few minutes — check again then"), the agent can only ask the user to
ping it back:

> Yes — I can check again, but I can't actually wait and wake myself up in a few
> minutes. If you ping me with "check now," I'll re-run Railway and confirm.

We want the agent to schedule its own follow-up: *sleep N minutes, then run a
query and report the result* — without a human in the loop.

## Why a blocking `sleep` tool is the wrong shape

A Workshop chat turn is a **synchronous SSE request/response**:
`POST /api/v1/chats/{id}/turn` streams events until the turn ends, then the
stream closes (`src/workshop_api.rs`). A tool that blocked for minutes would:

- hold the HTTP request open the whole time,
- block the chat's single in-flight turn (`ChatSession.turn_in_flight`),
- and be lost entirely on any daemon restart.

So the mechanism must be **schedule-and-return**, not block. The current turn
ends normally with an "I'll check back in ~N min" reply; the follow-up runs
later as its own unit of work.

## What already exists (and gets reused)

- **Daemon scheduler loop** — `src/daemon.rs` already polls on an interval and
  fires due *flows* (`is_due` for cron, `elapsed_due` for interval). We add a
  second thing it scans: due scheduled tasks.
- **Subagents** — `src/tools/sub_agent.rs` spawns a `create_react_agent` with a
  persona/tool-set/pack scope, runs it to completion, and returns a result.
- **Channel-agnostic delivery** — `ReplySink` (`src/tools/mod.rs`) routes a
  `say_to_user` message to the right place: the SSE stream for a workshop chat,
  or a gateway adapter (PipeStreamr/Twilio) for a gateway session.
- **Persisted chats** — chats are rehydrated from disk on restart
  (`load_persisted_chats`).

## Design: a deferred subagent

The agent calls a new tool that *schedules* a subagent task and returns
immediately. Later, the daemon fires it as a subagent whose `say_to_user`
output is delivered back into the originating chat/channel via a `ReplySink`.

### New tool: `schedule_followup`

Agent-invocable. Parameters mirror `sub_agent` plus a delay:

| param | meaning |
|-------|---------|
| `delay` / `at` | relative (`"3m"`, `"90s"`, `"2h"`) or absolute time to wake |
| `task` | instruction to run on wakeup, e.g. *"re-check metalcraftai.com custom-domain status on Railway; report if HTTPS is live"* |
| `persona` | run the wakeup as this persona (defaults to the current one) |
| `tool_set` / `pack` | tool scoping, same semantics as `sub_agent` |

Behavior: validates the delay against a max (below), writes a job to the store,
returns `{ "scheduled_id": ..., "run_at": ... }`. The agent then calls
`say_to_user` to tell the user it will follow up — and the turn ends.

### Store: `<data>/scheduled_tasks.json`

```jsonc
{
  "id": "sch_…",
  "chat_id": "…",              // originating chat
  "io_binding": { … },         // how to deliver: workshop chat id, or gateway channel + address
  "run_at": "2026-07-23T18:04:00Z",
  "task": "re-check …",
  "persona": "railway-agent",
  "tool_set": "all",
  "pack": "railway",
  "status": "pending",         // pending | running | done | failed | cancelled
  "created_at": "…",
  "reschedule_depth": 0        // loop guard (see Guardrails)
}
```

New module `src/scheduled_tasks.rs`: `list()`, `add()`, `cancel(id)`,
`due(now)`, `mark(id, status)`. Persisted with the same atomic-write pattern the
rest of the data dir uses; loaded on startup so pending jobs survive restarts.

### Scheduler tick (daemon)

Extend the `src/daemon.rs` poll loop: each tick, `scheduled_tasks::due(now)`
returns jobs whose `run_at` has passed and `status == pending`. For each:

1. mark `running`,
2. build a `ReplySink` from its `io_binding` (see Delivery),
3. spawn a subagent (reuse the `sub_agent` executor path) with that sink,
   persona, and tool scope, running the `task`,
4. on completion mark `done` (or `failed`); the subagent's `say_to_user` is what
   reaches the user.

### Delivery via `ReplySink` — the payoff

Because delivery is just a `ReplySink`:

- **Gateway sessions (Discord/SMS) work with zero UI changes** — the sink sends
  through the bound adapter exactly like a live reply. This is the whole of the
  user-facing win for gateway channels, available in P1.
- **Workshop chats** need the daemon to reach the in-memory `ChatStore` +
  publish to the chat's live event stream (P3 below). The wakeup turn also
  appends to the persisted chat so it's durable regardless of who's watching.

## Workshop chat UI + API integration

A wakeup message arrives with **no active `turn` request open**, so the current
UI would never see it live. Chosen approach: **live per-chat SSE subscription.**

### API

- `GET /api/v1/chats/{id}/events` — a persistent per-chat SSE subscription. The
  daemon-fired wakeup turn publishes its events (`turn_started → llm_* →
  tool_* → reply → done`) to a per-chat broadcast channel; any subscriber
  receives them live, identical to a normal turn. The existing `POST …/turn`
  stream is unchanged for user-initiated turns.
- `GET /api/v1/scheduled-tasks` — list pending + recent jobs (for a badge/panel).
- `DELETE /api/v1/scheduled-tasks/{id}` — cancel a pending job.

### Workshop frontend

- **ChatsView**: when `schedule_followup` fires, render an inline chip —
  *"⏳ Follow-up scheduled — checking back in ~3 min"* + task summary + a ✕ that
  calls the cancel endpoint. When the wakeup turn runs, its events arrive over
  the per-chat subscription and the assistant message appears live.
- **Scheduled panel** (optional, like Network/Settings): pending + recent jobs
  across all chats with status and cancel.
- Plumbing mirrors the existing SSE decoder in
  `workshop-api/src/connection.rs` and the `ChatStreamEvent` bus in the Tauri
  layer.

## Guardrails

- **Max delay** (e.g. 24h) — reject longer; that's what flows/cron are for.
- **Max pending per chat** — cap the queue.
- **Self-reschedule depth cap** — a wakeup task may schedule another, but
  `reschedule_depth` is bounded so a task can't re-arm itself forever
  (`runtime.rs` already treats repeated async checks carefully to avoid
  runaway-loop detection; this is the persistence-level equivalent).
- **Dedup** identical pending `(chat_id, task)` jobs.
- **Cancel** endpoint + UI control.

## Phasing

- **P1 — agent core (no UI):** `scheduled_tasks` store + `schedule_followup`
  tool + daemon tick + subagent delivery via `ReplySink`. Fixes "I can't wake
  myself up" **for gateway channels immediately**. Verify with a short-delay
  smoke test.
- **P2 — Workshop API + basic UI:** `/scheduled-tasks` list + cancel; inline
  scheduled chip in ChatsView (durable via the persisted chat).
- **P3 — Workshop live:** per-chat SSE subscription so wakeup turns stream into
  an open chat with no refresh; optional Scheduled panel.
