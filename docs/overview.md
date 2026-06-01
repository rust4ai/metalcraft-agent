# Metalcraft Agent — Overview

Metalcraft Agent is a Rust application for building and running **reactive AI agents**. It
wraps the [`metalcraft`](https://crates.io) ReAct agent framework with a domain layer of
**personas**, **skills**, **tools**, and **flows**, plus an admin REST API ("Workshop") for
managing all of it.

The same codebase ships two binaries:

| Binary | Source | Role |
| --- | --- | --- |
| `metalcraft-agent` | `src/main.rs` | Interactive REPL and one-shot task runner |
| `metalcraft-daemon` | `src/bin/metalcraft-daemon.rs` | Flow scheduler, Workshop API server, and event webhook listener |

## What it does

An agent takes a task (typed in the REPL, passed on the command line, fired by a scheduled
flow, or triggered by an external event), reasons about it with an LLM, and calls tools to
get work done — reading and editing files, running shell commands, fetching web pages,
calling external HTTP APIs, and delegating to sub-agents. Sensitive actions pass through an
**approval gate** before they execute.

### Three ways to run an agent

1. **Interactive REPL** — `metalcraft-agent` opens a conversational session with the default
   Orchestrator persona (use `--persona <slug>` to pick another).
2. **One-shot task** — `metalcraft-agent "refactor the auth module"` runs a single request and
   exits. Persona is selected with `--persona <slug>`; every positional arg is the task.
3. **Flow scheduler daemon** — `metalcraft-daemon` polls the `flows/` directory and runs
   enabled workflows on a schedule (interval or cron). It can additionally serve the
   Workshop API and listen for inbound events (Discord/Slack/GitHub-style webhooks).

## Core concepts

| Term | Meaning |
| --- | --- |
| **Persona** | A JSON config (`personas/*.json`) defining an agent's name, description, system prompt, allowed tools, and skills. Personas are the unit of agent identity and capability. |
| **Skill** | A Markdown file (`skills/*.md`) holding reusable methodology (e.g. how to review code, how to debug). Loaded on demand via the `load_skill` tool, or attached to a persona. |
| **Tool** | A capability the agent can call. Built-ins: `read_file`, `write_file`, `edit_file`, `bash`, `grep`, `find_files`, `list_files`, `load_skill`, `web_fetch`, `sub_agent`. Additional tools come from JSON-configured HTTP API definitions. |
| **Flow** | A workflow graph (JSON) of nodes (entry / prompt / branch) and edges, with a schedule. The daemon traverses from the entry node and runs each reachable prompt node as a one-shot task. |
| **Integration Pack** | A bundle of personas, skills, HTTP tools, and flow templates that can be enabled or disabled as a unit (e.g. the Discord pack, the Solarabase RAG pack). User files always shadow pack files. |
| **Key Store** | A JSON file (`keys.json`) mapping secret names to values. HTTP tools reference secrets with `$NAME` placeholders so credentials never live in tool configs. |
| **Workshop** | The admin REST API (and companion app) for editing personas, skills, flows, tools, packs, and keys, and for running chats and flows. |
| **Approval** | The interactive gate that classifies each tool call and asks the user before destructive operations run (auto-approved for read-only tools or with `--auto-approve`). |
| **Diagnostics** | Optional per-session JSON logging of every LLM call, turn, and config change, under `logs/<timestamp>/`. |

## Tech stack

- **Language:** Rust (edition 2024)
- **Agent framework:** `metalcraft` (ReAct loop), `metalcraft-flows` (flow data model)
- **LLM client:** `rig` (OpenAI-compatible) — defaults to GPT-class models, configurable via env
- **Async runtime:** Tokio
- **HTTP server:** Axum (Workshop API + event listener)
- **Scheduling:** `cron` crate (interval and cron expressions)
- **Terminal UI:** Rustyline, Crossterm, Syntect (diff/syntax highlighting)

## Storage model

Metalcraft Agent has **no database** — everything lives as files under a single data
directory, resolved in this order:

1. `METALCRAFT_DATA_DIR` (explicit override)
2. The OS application-data directory (e.g. `~/.local/share/metalcraft-agent` on Linux)
3. `./data` (container-friendly fallback)

```
<data>/
├── personas/                 # *.json persona configs
├── skills/                   # *.md methodology guides
├── flows/                    # *.json workflow graphs
├── flow_templates/           # *.json reusable flow templates
├── api_tools/                # *.json HTTP tool configs
├── integration_packs/        # pack directories (read-only contents)
├── integration_packs.json    # per-pack enabled/disabled state
├── keys.json                 # API key store (secret name -> value)
├── uploads/                  # upload root for multipart HTTP tools
└── logs/<timestamp>/         # diagnostics sessions
```

On first run, bundled default personas, skills, and packs are seeded to disk
(`src/seed.rs`). Seeding never overwrites files a user has edited.

## Where to go next

- **[architecture.md](architecture.md)** — how the runtime, flows, approval, and packs fit together internally.
- **[getting-started.md](getting-started.md)** — building, running, deploying, and configuring the app.
