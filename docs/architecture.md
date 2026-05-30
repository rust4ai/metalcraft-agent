# Metalcraft Agent — Architecture

This document explains how the pieces fit together internally. For a conceptual overview and
glossary, see **[overview.md](overview.md)**.

## Module map

| Module | File | Responsibility |
| --- | --- | --- |
| Runtime | `src/runtime.rs` | Builds the ReAct agent graph shared by both binaries (`build_agent_runtime`) |
| Persona | `src/persona.rs` | Load/save/list personas; assemble the system prompt; parse skill frontmatter |
| Flows | `src/flows.rs` | Load flows, parse schedules, BFS-traverse the graph to collect prompt nodes |
| Tools | `src/tools/` | Built-in tools (file ops, bash, grep, web fetch, sub-agent) + generic HTTP API tool |
| Approval | `src/approval.rs` | Classify operations by sensitivity; interactive approval prompt with diff preview |
| Context | `src/context.rs` | Token estimation and automatic conversation compaction |
| Guard | `src/guard.rs` | Error-spiral and loop detection; verbose tool-call output |
| Diagnostics | `src/diagnostics.rs` | Per-session JSON logging of LLM calls, turns, and config switches |
| Integration packs | `src/integration_packs.rs` | Pack manifest, loading, and enable/disable state |
| Workshop API | `src/workshop_api.rs` | Axum REST API for personas, skills, flows, chats, tools, packs, keys |
| Event listener | `src/event_listener.rs` | Webhook server that turns inbound gateway events into agent tasks |
| Events | `src/events.rs` | Event normalization and platform-specific prompt conversion |
| Key store | `src/key_store.rs` | Plaintext secret store referenced by `$NAME` placeholders |
| Paths | `src/paths.rs` | Data-directory resolution and subdirectory helpers |
| Seed | `src/seed.rs` | Bundled default personas/skills/packs written on first run |
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
   up to a step ceiling (~90 steps), with the step guard watching for error spirals
   (≈3 consecutive failures) and repeated-call loops.

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
3. On each poll, the daemon checks whether a flow is due.
4. When due, it **BFS-traverses** the graph from the entry node and collects all reachable
   `prompt` nodes (`src/flows.rs`).
5. Each prompt runs as a one-shot task via the shared runtime. The persona can be overridden
   per-flow (on the entry node) or per-node.

Flow templates (`<data>/flow_templates/`) are reusable starting points exposed through the
Workshop API.

## Integration packs

Packs (`src/integration_packs.rs`) are the plugin mechanism:

- A pack is a directory under `<data>/integration_packs/<id>/` with a manifest plus
  `personas/`, `skills/`, `api_tools/`, and `flow_templates/` subdirectories.
- Enable/disable state is stored in `<data>/integration_packs.json`.
- At resolution time, **user files shadow pack files** — a user persona named the same as a
  pack persona takes precedence.
- Pack contents are **read-only**; the Workshop API rejects writes to pack-owned items.

Shipped packs include **Discord** (agent personas, formatting skills, and message tools that
talk to an agent gateway) and **Solarabase** (RAG retrieval/query/upload tools backed by a
Solarabase knowledge base).

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

## Event listener

`src/event_listener.rs` + `src/events.rs` provide a webhook endpoint (enabled via daemon
event flags). Inbound platform events (Discord/Slack/GitHub-style) are normalized into a
common shape and converted into agent task prompts, letting agents respond to external
activity in addition to schedules.
