# Metalcraft Agent — Architecture

This document explains how the pieces fit together internally. For a conceptual overview and
glossary, see **[overview.md](overview.md)**.

## Module map

| Module | File | Responsibility |
| --- | --- | --- |
| Runtime | `src/runtime.rs` | Builds the agent runtime (`build_agent_runtime`) and hosts `TurnRunner`, the shared per-turn wrapper (compact → executor) used by the CLI, the daemon chat path, and one-shot tasks |
| Persona | `src/persona.rs` | Load/save/list personas; assemble the templated system prompt; parse skill frontmatter |
| Skill | `src/skill.rs` | Skill-file CRUD shared by the Workshop API and the `skill_*` meta tools |
| Flows (v1) | `src/flows.rs` | Load flows, parse schedules, BFS-collect reachable prompt nodes (legacy path) |
| Flow executor (v2) | `src/flow_exec.rs`, `src/flow_runs.rs` | Stateful flow state machine (conditional/branch/tool/http/… nodes) with pause/resume and run persistence |
| Tools | `src/tools/` | Core built-ins (file ops, bash, grep, web fetch, sub-agent), the generic HTTP-API tool, and native meta/integration tools |
| Approval | `src/approval.rs` | Classify operations by sensitivity; interactive approval prompt with diff preview |
| Context | `src/context.rs` | Token estimation and automatic conversation compaction |
| Guard | `src/guard.rs` | Error-spiral and loop/poll detection; verbose tool-call output |
| Diagnostics | `src/diagnostics.rs` | Per-session JSON logging of LLM calls, turns, and config switches |
| Scheduled tasks | `src/scheduled_tasks.rs` | Persisted scheduled follow-ups armed by `schedule_followup`, fired each daemon tick |
| Integration packs | `src/integration_packs.rs` | Pack manifest, loading, and enable/disable state |
| Workshop API | `src/workshop_api.rs` | Axum REST API for personas, skills, flows, chats, tools, packs, keys; hosts live chat and gateway webhook ingress |
| Gateway | `src/gateway_channels.rs`, `src/gateway_activity.rs` | Messaging gateway: channel types + user channel instances (inbound `/webhook/<adapter>`), with an append-only traffic log |
| Daemon | `src/daemon.rs` | Poll loop: due-flow evaluation, scheduled-follow-up firing, Workshop API startup (the bin `src/bin/metalcraft-daemon.rs` is a thin wrapper) |
| Key store | `src/key_store.rs` | Plaintext secret store referenced by `$NAME` placeholders |
| Paths | `src/paths.rs` | Data-directory resolution and subdirectory helpers |
| Seed | `src/seed.rs` | Bundled default personas/skills/packs/flow-templates/channels written on first run |
| UI / Diff | `src/ui.rs`, `src/diff_preview.rs` | Terminal styling and scrollable diff rendering |

## The agent runtime

Both binaries construct agents the same way through `build_agent_runtime()` in
`src/runtime.rs`:

1. **Load context** — `AgentRuntimeContext::from_environment()` reads configuration (model,
   API key, data dir, flags) from the environment / `.env`.
2. **Resolve the persona** — `src/persona.rs` loads the persona JSON, layering user files
   over enabled-pack files (user wins). The persona names the allowed tools and skills.
3. **Assemble the system prompt** — persona system prompt + attached skill contents +
   current working-directory context.
4. **Build the tool registry** — `src/tools/mod.rs` registers built-in tools plus any
   HTTP API tools from enabled packs / `api_tools/`.
5. **Install hooks** — an approval hook (gates tool calls) and an LLM hook (feeds the
   diagnostics logger).
6. **Run the ReAct loop** — the `metalcraft` executor drives `think → tool_call → observe`
   up to a step ceiling (`MAX_TURN_STEPS = 90`), with the step guard watching for error
   spirals (≈3 consecutive failures) and repeated-call loops.

Every turn — CLI, the daemon chat path (`run_chat_turn`), and one-shot tasks
(`run_one_shot_task`) — executes through the shared `runtime::TurnRunner`, which compacts the
context (if needed) and then runs the executor with the single shared `max_steps`. The CLI
builds the `TurnRunner` once and reuses it across turns; the daemon builds one per turn. This
is the single place turn execution lives, so compaction/step-limit/guard wiring can't be
present in one path and silently missing from another.

```
task / event / flow prompt
        │
        ▼
 AgentRuntimeContext  (env + .env)
        │
        ▼
 resolve persona ──► system prompt + skills
        │
        ▼
 build tool registry ──► built-ins + HTTP/pack tools
        │
        ▼
 metalcraft ReAct executor
   ├─ think     (LLM reasoning)
   ├─ tool_call (approval gate ► execute)
   └─ observe   (tool result)
        │
        ▼
   output / side effects
```

## Approval gating

Defined in `src/approval.rs`:

- `OperationKind::classify()` maps each tool call (name + arguments) to a sensitivity level.
- **Read-only** operations (`read_file`, `grep`, `find_files`, `list_files`, …) are
  auto-approved.
- **Destructive** operations (`write_file`, `edit_file`, `bash`, message-sending tools) need
  interactive approval when running in a TTY.
- The prompt shows the tool name, arguments, and — for file edits — a scrollable diff
  (`src/diff_preview.rs`). The user approves or denies; the decision is logged to
  diagnostics.
- `--auto-approve` (and headless/daemon mode) bypasses the interactive prompt.

## Context management

`src/context.rs` estimates tokens (~4 chars/token) and **compacts** the conversation
automatically when it grows past a threshold (around 60% of a 128k window), summarizing older
messages while preserving the most recent ones. Compaction events are recorded in
diagnostics.

## Flows and the scheduler

Flows are workflow graphs loaded by `metalcraft-daemon`:

1. The daemon loads every enabled flow from `<data>/flows/`.
2. Each flow's **entry node** carries a schedule: `manual`, `minutes: N`, `hours: N`, or a
   `cron:` expression (evaluated in the daemon's local timezone).
3. On each poll, the daemon checks whether a flow is due, and separately fires any due
   **scheduled follow-ups** (`src/scheduled_tasks.rs`).
4. When a flow is due, the daemon branches on `flow_exec::is_v2_flow`:
   - **v2 flows** run through `flow_exec::run_flow_v2` (`src/flow_exec.rs`) — a stateful state
     machine that walks nodes one at a time, threading a shared `variables` object and routing
     by output handle. Node types: `entry`, `prompt`, `set_variable`, `tool`, `conditional`
     (with `branch`, `http`, `sub_agent`, `approval`, `wait`, `foreach` staged). Runs persist
     to `<data>/runs/` so an `approval`/`wait` node can pause and later **resume**
     (`resume_flow`).
   - **v1 (legacy) flows** take the older path: BFS-collect reachable `prompt` nodes
     (`src/flows.rs`) and run each as a one-shot task.
5. Persona can be overridden per-flow (on the entry node) or per-node.

Flow templates (`<data>/flow_templates/`) are reusable starting points exposed through the
Workshop API.

## Integration packs

Packs (`src/integration_packs.rs`) are the plugin mechanism:

- A pack is a directory under `<data>/integrations/<id>/` with a manifest plus
  `personas/`, `skills/`, `api_tools/`, and `flow_templates/` subdirectories.
- Enable/disable state is stored in `<data>/integrations.json`.
- At resolution time, **user files shadow pack files** — a user persona named the same as a
  pack persona takes precedence.
- Pack contents are **read-only**; the Workshop API rejects writes to pack-owned items.

Sixteen packs ship today — including **github**, **linear**, **sentry**, **cloudflare**,
**calcom**, **discord**/**discord_admin**, **solarabase** (RAG), **email** (IMAP),
**digitalocean_spaces**, **railway**/**render**, **metalcraft-calendar**, and **vestaloop**.
See the README for the full list.

## HTTP API tools

Beyond hardcoded built-ins, tools can be defined as JSON (`src/tools/http_api.rs`,
`<data>/api_tools/*.json`). A definition describes the HTTP method, URL, headers, and body
template. Secrets are injected from the **key store** via `$NAME` placeholders, so tool
configs stay free of credentials. This is how pack tools (Discord, Solarabase) and any
user-defined integrations are implemented — no recompile required.

## The Workshop API

`src/workshop_api.rs` is an Axum server (enabled with `--api <KEY>`) under `/api/v1/`. It
exposes CRUD for personas, skills, flows, flow templates, HTTP tools, and integration packs;
manages the key store (including `keys/recommended` for enabled packs); runs **streaming chat
sessions** and **flow runs**; and serves diagnostics session logs. The full contract lives in
`openapi/workshop-api.yaml`.

## Gateway channels

The standalone event-listener subsystem (and its daemon `--event-*` flags) was removed.
Inbound messaging now arrives through **gateway channels**, hosted inside the Workshop API:

- **Channel types** are JSON manifests (WhatsApp via PipeStreamr/Twilio ships today);
  **channel instances** are user-created bindings persisted in `<data>/gateway_channels.json`
  (`src/gateway_channels.rs`).
- Inbound webhooks land at `/webhook/<adapter>` (e.g. `/webhook/pipestreamr`,
  `/webhook/twilio`) on the Workshop API and are turned into agent turns; the agent replies
  through the `gateway_send_message` / `say_to_user` tools.
- All inbound/outbound traffic is appended to `<data>/gateway_activity.jsonl`
  (`src/gateway_activity.rs`).
