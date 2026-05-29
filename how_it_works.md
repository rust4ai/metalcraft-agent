# How Metalcraft Agent Works

This document explains the architecture of **metalcraft-agent** in detail: how the
agent loop runs, how personas/skills/tools fit together, how tool approval works,
and how the flow scheduler executes local workflow files. It is meant to be read
top-to-bottom by someone new to the codebase.

---

## 1. The big picture

Metalcraft Agent is a Rust application built on top of three external crates:

- **`metalcraft`** — the agent framework. It provides the ReAct-style agent graph
  (`create_react_agent`), the `Executor` that runs it, the `AgentState`/`AgentMessage`
  types, the `Tool` trait + `ToolRegistry`, and the hook system (before-tool-call,
  LLM-call, step-guard).
- **`rig`** — the LLM client layer. The OpenAI-compatible client (`rig::providers::openai`)
  turns a model name into a `CompletionModel` that the agent graph drives.
- **`metalcraft-flows`** — the flow/workflow data model: `SavedFlow`, `FlowNode`,
  node types, and `validate()`.

On top of those, this repo adds:

- **Two binaries** — an interactive/one-shot CLI (`metalcraft-agent`) and a
  scheduler daemon (`metalcraft-daemon`).
- **Personas** — JSON files that define an agent's name, system prompt, allowed tools,
  and skills.
- **Tools** — Rust implementations of file/shell/search/network operations, plus a
  generic JSON-configured HTTP tool.
- **Skills** — markdown methodology files loaded on demand.
- **A flow runtime** — loads local workflow JSON and runs reachable prompt nodes.
- **Supporting subsystems** — tool approval, context compaction, a safety step-guard,
  diagnostics logging, an event listener (webhook-driven), and a "workshop" admin REST API.

```
┌──────────────────────────────────────────────────────────────────┐
│                         metalcraft-agent (bin)                     │
│   interactive REPL  |  one-shot task  |  --api workshop server     │
└───────────────┬────────────────────────────────────────┬─────────┘
                │                                          │
                │ build_agent_runtime()                    │
                ▼                                          │
┌──────────────────────────────────┐                      │
│           runtime.rs              │                      │
│  persona → system prompt          │                      │
│  tools   → ToolRegistry           │                      │
│  model   → rig CompletionModel    │                      │
│  approval hook + llm hook         │                      │
│  = create_react_agent_with_hooks  │                      │
│  ⇒ CompiledGraph<AgentState>      │                      │
└───────────────┬──────────────────┘                      │
                │ Executor::run(state, "agent")            │
                ▼                                          │
        ┌───────────────┐                                 │
        │  Agent loop    │  think → call tool → observe →… │
        │  (ReAct graph) │                                 │
        └───────────────┘                                 │
                                                           ▼
┌──────────────────────────────────────────────────────────────────┐
│                      metalcraft-daemon (bin)                       │
│  poll flows/ dir → due? → BFS prompt nodes → run_one_shot_task()   │
│  optional: event listener (webhooks) + workshop API (shared)       │
└──────────────────────────────────────────────────────────────────┘
```

---

## 2. Source map

| File | Responsibility |
|------|----------------|
| `src/main.rs` | Entry point for the CLI: arg parsing, REPL, slash commands, one-shot mode, `--api` dispatch. |
| `src/bin/metalcraft-daemon.rs` | Scheduler daemon: poll loop, schedule due-checks, optional event listener + workshop API. |
| `src/runtime.rs` | Shared agent construction (`build_agent_runtime`, `run_one_shot_task`) used by both binaries. |
| `src/persona.rs` | Persona load/save/list and system-prompt assembly; frontmatter parsing for skills. |
| `src/flows.rs` | Flow loading, schedule parsing, BFS traversal to collect reachable prompt texts. |
| `src/tools/` | Tool implementations + the registry builder. |
| `src/approval.rs` | Tool-call classification + interactive approval prompts (incl. scrollable diff viewer). |
| `src/context.rs` | Token estimation + automatic conversation compaction. |
| `src/guard.rs` | Step guard: error-spiral and loop detection; verbose tool-call printing. |
| `src/diagnostics.rs` | Optional per-session JSON logging of LLM calls, turns, and config changes. |
| `src/seed.rs` | Writes bundled default personas/skills/flows/api_tools into the data dir on startup. |
| `src/paths.rs` | Resolves the data directory and its subdirectories. |
| `src/workshop_api.rs` | Axum REST API for editing personas/skills/flows/api-tools and reading diagnostics. |
| `src/event_listener.rs` | Webhook listener that turns gateway events (e.g. Discord messages) into agent tasks. |
| `src/events.rs` | Gateway event types. |
| `src/diff_preview.rs`, `src/ui.rs` | Diff rendering for approvals; terminal styling helpers. |

---

## 3. Startup and configuration

Both binaries do the same first two things in `main()`:

1. `env_logger::init()` — log level via `RUST_LOG`.
2. `metalcraft_agent::seed::ensure_defaults()` — creates the data directory and writes
   any bundled seed files that don't already exist (it never overwrites existing files).

### Where data lives (`src/paths.rs`)

The data root is resolved in priority order:

1. `METALCRAFT_DATA_DIR` env var (explicit override),
2. the OS app-data dir (`~/.local/share/metalcraft-agent` on Linux),
3. `./data` as a container-friendly fallback.

Subdirectories: `personas/`, `skills/`, `flows/`, `logs/`, `api_tools/`.

### Seeding (`src/seed.rs`)

Default personas, skills, api-tools, and one example flow are compiled into the binary
with `include_str!` from the `seed/` directory and written out on first run. This means
a fresh install is immediately usable, and users can then edit the files in their data dir.

### Environment / API key (`src/runtime.rs`)

`AgentRuntimeContext::from_environment()` loads `.env` (via `dotenvy`), resolves the
personas/skills dirs, and requires `OPENAI_API_KEY`. The model defaults to `gpt-5.4`
(`DEFAULT_MODEL`), overridable by `OPENAI_MODEL`; available models are
`gpt-5.4-mini`, `gpt-5.4`, `gpt-5.5`.

---

## 4. How the agent is built

Everything funnels through `runtime::build_agent_runtime(...)`. Given a persona, cwd,
model name, approval mode, and optional LLM-call hook, it:

1. **Builds the system prompt** — `persona.build_system_prompt(skills_dir, cwd)`
   concatenates the persona's `system_prompt`, appends the working directory, and (if
   the persona lists skills) appends an "Available Skills" section listing each skill
   name + its frontmatter `description`, instructing the model to call `load_skill`.
2. **Builds the tool registry** — `tools::create_registry_for_with_config(&persona.tools, cfg)`
   registers exactly the tools the persona names (see §6).
3. **Creates the model** — `openai::Client::new(api_key).completion_model(model_name)`.
4. **Builds the approval hook** — `approval::build_hook(approval_mode)` (see §7).
5. **Compiles the graph** — `create_react_agent_with_hooks(model, registry, system_prompt,
   before_tool_hook, llm_call_hook)` and stores it as an `Arc<CompiledGraph<AgentState>>`
   so it can be cheaply cloned/shared across turns.

It also returns a separate `compaction_model` (a second `CompletionModel` handle) used for
summarizing context (see §8).

### Running the graph

An `Executor` drives the compiled graph:

```rust
let executor = Executor::new_from_arc(graph.clone())
    .max_steps(90)
    .with_step_guard(step_guard.clone());
let outcome = executor.run(turn_state, "agent").await;
```

`run` returns a `RunOutcome`:

- `Completed(state)` — the agent produced a final answer (`state.final_answer()`).
- `Interrupted { state, reason, .. }` — stopped early (e.g. step guard tripped, or
  approval was denied), but the conversation state is preserved.

The agent itself is a **ReAct loop** provided by `metalcraft`: the model thinks, optionally
emits a tool call, the framework executes the tool (subject to the before-tool-call hook),
appends the `ToolResult` to the message list, and repeats until the model returns a final
answer or `max_steps` (90) is hit.

`AgentState.messages` is a vector of `AgentMessage`:
`User`, `Assistant`, `ToolCall { name, args }`, `ToolResult { name, result }`.

---

## 5. The CLI: interactive and one-shot modes (`src/main.rs`)

```
metalcraft-agent [--auto-approve] [--diagnostics] <persona> [task]
```

Arg handling:

- `--api <KEY>` (or `WORKSHOP_API_KEY` env) short-circuits everything and starts the
  workshop REST server instead of an agent (see §11).
- `--auto-approve` and `--diagnostics` are extracted as flags; the first remaining arg is
  the persona slug (default `coding-agent`), and any further args joined together are the
  one-shot task.
- If **stdin is not a TTY** (`atty`), approval is forced to auto-approve and a one-shot task
  is required (headless usage).

**One-shot mode** (a task was given): calls `runtime::run_one_shot_task(...)`, prints the
final answer or interruption reason, and exits.

**Interactive mode** (no task): a `rustyline` REPL. Each non-command line becomes a turn.
Conversation state persists across turns via `state.continue_with(input)` (or
`AgentState::new(input)` for the first turn). Before each turn, `context::compact_if_needed`
may summarize old history. Then the executor runs and the resulting state is stored for the
next turn.

### Slash commands

| Command | Effect |
|---------|--------|
| `/quit`, `/exit` | Leave the REPL. |
| `/clear` | Drop the conversation state (fresh context). |
| `/tokens` | Print the estimated token count and message count. |
| `/cd [path]` | Show or change the working directory; **rebuilds the agent** so the new cwd is in the system prompt. |
| `/persona [list]` / `/persona set <slug>` | List or switch persona; rebuilds the agent and clears the conversation. |
| `/model [list]` / `/model use <name>` | List or switch model; rebuilds the agent and clears the conversation. |

Switching persona, model, or cwd all call `build_agent_runtime` again to produce a fresh
graph, because the system prompt and/or tool set change.

---

## 6. Tools (`src/tools/`)

A tool is anything implementing the `metalcraft::Tool` trait: `name()`, `description()`,
`parameters_schema()` (JSON Schema describing the args), and an async `call(args) -> Result<Value>`.
The persona's `tools` array selects which tools are registered, by name, in
`create_registry_for_with_config`:

| Tool | Purpose |
|------|---------|
| `read_file` | Read a file. |
| `write_file` | Write/overwrite a file. |
| `edit_file` | String-replacement edit. |
| `bash` | Run a shell command (returns JSON `{exit_code, stdout, stderr}`). |
| `list_files` | List a directory. |
| `grep` | Search file contents. |
| `find_files` | Find files by name/pattern. |
| `web_fetch` | Fetch a URL (HTML→markdown via `htmd`). |
| `load_skill` | Load a skill markdown body on demand (enum-restricted to the persona's skills). |
| `sub_agent` | Spawn a nested agent for a delegated subtask. |

Two tools need extra runtime config (`ToolConfig`: api_key, model name, system prompt,
skills dir, available skills):

- **`load_skill`** — needs the skills dir + the persona's allowed skill list, so its
  parameter schema can restrict `skill` to a known `enum`. It reads
  `<skills_dir>/<skill>.md`, strips YAML frontmatter, and returns the body.
- **`sub_agent`** — needs the api key/model/system prompt to build a child agent. It accepts
  a `task` and a `tool_set` (`read_only` default, or `full`), builds its own registry and
  ReAct graph, runs it with a **120-second timeout** and `max_steps(90)`, and returns the
  child's final answer plus which tools it used and how many turns it took.

### User-defined HTTP tools (`src/tools/http_api.rs`)

If a persona names a tool that isn't one of the built-ins, the registry tries to load it as
an **HTTP API tool** from `<data_dir>/api_tools/<name>.json`. This is how the Discord tools
work without any hardcoded Rust. The JSON config defines:

- `name`, `description`, `method`, `url`,
- `headers` (values support `$ENV_VAR` expansion),
- `parameters` (the JSON Schema shown to the model),
- `body_mapping` — `none` | `params` (default) | `template`,
- `body_defaults` (merged under the args; args win on conflict),
- `body_template` (for `template` mapping).

At call time it expands `{param}` placeholders in the URL from the args, strips any
unexpanded optional `{param}` query segments, expands `$ENV_VAR` references, builds the
body per `body_mapping`, sends the request (30s timeout), and returns
`{status, data}` (JSON) or `{status, body}` (text, truncated to 50k chars).

Example: `discord_send_message.json` POSTs to `$AGENT_GATEWAY_URL/api/v1/messages` with a
`Bearer $AGENT_GATEWAY_API_KEY` header and `body_defaults: { "platform": "discord" }`.

---

## 7. Tool approval (`src/approval.rs`)

The before-tool-call hook decides whether each tool call proceeds. Two modes:

- **`AutoApprove`** — the hook is `None`; everything proceeds (used with `--auto-approve` or
  when headless).
- **`Interactive`** — a data-driven policy. Each call is **classified** into an
  `OperationKind`, which maps to a `PermissionLevel` of `AutoApprove` or `RequiresApproval`
  (with optional per-kind overrides).

Default policy:

| OperationKind | Tools | Default |
|---------------|-------|---------|
| ReadFile / ListFiles / Search / LoadSkill | read_file, list_files, grep, find_files, load_skill | **auto** |
| WriteNewFile | write_file (path doesn't exist) | **auto** |
| OverwriteFile | write_file (path exists) | prompt |
| EditFile | edit_file | prompt |
| Execute | bash, **and any unknown tool** | prompt |
| NetworkFetch | web_fetch | prompt |
| SubAgent | sub_agent | prompt |
| DiscordAction | discord_send/edit/add_reaction | prompt |

Note two safety details: read-only Discord tools (`discord_get_*`) classify as `ReadFile`
(auto), and **unknown tools default to `Execute`** (prompt) — fail safe.

When approval is required, the terminal prompt:

- For `edit_file`/`write_file`, computes a colored diff (`diff_preview`). Small diffs print
  inline with a Yes/No menu; large diffs open a **scrollable alternate-screen viewer**
  (PgUp/PgDn/Home/End to scroll, ↑/↓ or y/n/Enter to decide).
- For `bash`, shows the command; for others, the JSON args.
- The prompt runs on a dedicated OS thread (so it doesn't block the tokio runtime or fight
  with rustyline's terminal state) and times out to a denial after inactivity.

A denial returns `BeforeToolCallAction::Deny(reason)`, which the framework feeds back to the
model as the tool result so it can adapt.

---

## 8. Context compaction (`src/context.rs`)

Long conversations are summarized to stay under the context window.
`CompactionConfig` defaults: 128k-token window, compact at 60% utilization, always keep the
10 most recent messages intact.

`estimate_tokens` is a cheap heuristic (~4 chars per token across all message content).
Before each interactive turn, `compact_if_needed`:

1. Returns early if under threshold or if there aren't more messages than `keep_recent`.
2. Otherwise takes all-but-the-last-10 messages, renders them into a transcript, and asks
   the **compaction model** (a lightweight `rig` agent with a "summarizer" preamble) to
   produce a concise factual summary (decisions, files touched, commands, findings, errors).
3. Replaces the old messages with a single `[Summary of earlier conversation]: …`
   assistant message, preserving the recent 10.

---

## 9. The step guard (`src/guard.rs`)

`build_agent_guard` returns a `StepGuard` invoked after every step. It does two jobs:

1. **Verbose tool tracing** — prints each `▶ tool(args)` call and its `✓/✗` result as the
   agent runs (bash results print parsed stdout/stderr and exit code). If diagnostics are
   on, it also logs the full turn.
2. **Safety stops** (returns `GuardAction::Stop`, which surfaces as an `Interrupted`
   outcome):
   - **Error spiral** — 3 consecutive tool turns where *every* result starts with `ERROR:`.
   - **Loop detection** — the newest tool call is byte-for-byte identical (name + args) to
     the immediately preceding one. (Identical-but-spaced calls like repeated `cargo check`
     between edits are allowed; only back-to-back repeats trip it.)

---

## 10. Flows and the scheduler daemon

This is the "workflow" half of the project. A **flow** is a JSON file describing a small
graph of nodes; the daemon polls a directory and runs due flows.

### Flow file shape

```json
{
  "spec_version": "1",
  "id": "nightly-review",
  "name": "Nightly Review",
  "enabled": true,
  "flow": {
    "nodes": [
      { "id": "entry", "node_type": "entry",
        "data": { "schedule_type": "hours", "interval": 24 }, "position": [0,0] },
      { "id": "task", "node_type": "prompt",
        "data": { "prompt": "Review project status and summarize priorities." }, "position": [200,0] }
    ],
    "edges": [ { "id": "e1", "source": "entry", "target": "task" } ]
  }
}
```

### Schedule parsing (`src/flows.rs`)

`load_enabled_flows` lists flow summaries, keeps `enabled: true`, loads each, then parses
its schedule. `parse_schedule` first runs `metalcraft_flows::validate(flow)` and then reads
the single `entry` node's `data.schedule_type`:

- `manual` — parsed but **never auto-run** by the daemon.
- `minutes` / `hours` — require a positive numeric `interval`.
- `cron` — the expression is validated with the `cron` crate (so a bad expression is
  rejected at load time).

`collect_reachable_prompt_texts` does a **BFS from the single entry node** over the edges and
collects `data.prompt` from each reachable `prompt` node, in traversal order. It explicitly
**errors** on `branch`, `branch_tool`, or custom node types — these are recognized but not
yet executed. Constraints: exactly one entry node, prompts must have `data.prompt`, only
reachable prompts run, and they run sequentially.

### The poll loop (`src/bin/metalcraft-daemon.rs`)

```
cargo run --bin metalcraft-daemon -- --persona coding-agent --poll-seconds 30
cargo run --bin metalcraft-daemon -- --once --auto-approve
```

Each cycle:

1. `load_enabled_flows(flows_dir)`.
2. For each flow, `is_due(state, schedule)` decides whether to run:
   - `Manual` → never.
   - `EveryMinutes`/`EveryHours` → due if never run, or if enough wall-clock time elapsed
     since `last_started_at`.
   - `Cron` → due if the next scheduled time after the last start is ≤ now.
3. If due and not already marked running, mark it running, set `last_started_at`, collect
   the reachable prompts, and run each one via `runtime::run_one_shot_task` with the daemon's
   persona/model/approval settings. Results are logged.
4. Run state is kept **in-memory only** (a `HashMap<flow_id, FlowRunState>`) — restarting the
   daemon forgets history, so interval flows run again on next start.
5. With `--once`, exit after one pass; otherwise sleep `poll-seconds` and repeat.

Daemon flags: `--flows-dir`, `--persona` (default `coding-agent`), `--model`,
`--poll-seconds` (default 30), `--once`, `--auto-approve`, plus the event-listener and
workshop-API flags below.

---

## 11. The workshop REST API (`src/workshop_api.rs`)

An optional Axum server for a desktop "workshop" app to manage the agent's files. Enabled by
`--api <KEY>` (or `WORKSHOP_API_KEY`) on **either** binary — `metalcraft-agent --api` runs it
standalone; `metalcraft-daemon --api` spawns it alongside the scheduler so one process does
both. Port defaults to 3002 (`--api-port` / `WORKSHOP_API_PORT` / `PORT`). All routes require
a `Bearer <KEY>` header.

Routes (all under `/api/v1`):

- `GET /snapshot` — personas, skills, flows, diagnostics sessions, api-tools, and the dir layout.
- `GET|PUT|DELETE /personas/{slug}`
- `GET|PUT|DELETE /skills/{slug}`
- `GET|PUT|DELETE /flows/{id}`
- `GET /diagnostics`, `GET /diagnostics/{id}`
- `GET /api-tools`, `GET|PUT|DELETE /api-tools/{name}`

These read and write the same files in the data dir that the agent and daemon use, so edits
made through the API take effect on the next agent build / flow poll.

---

## 12. The event listener (`src/event_listener.rs`)

When `AGENT_GATEWAY_URL` is set, `metalcraft-daemon` spawns a webhook listener that turns
inbound platform events (e.g. Discord messages, via an external "agent gateway") into
one-shot agent tasks. It requires `EVENTD_WEBHOOK_SECRET`, `AGENT_GATEWAY_API_KEY`, and a
non-empty admin allow-list (`EVENTD_ADMIN_USER_IDS` / `--admin-user-ids`) — it refuses to
start without them. It registers itself as a subscriber with the gateway, authenticates
inbound webhooks against the secret, only acts on events from allowed admin user IDs, and
caps concurrent agent runs with a semaphore (`MAX_CONCURRENT_TASKS = 4`). This is what lets
the agent run "reactively" — replying in Discord — combined with the Discord HTTP tools.

---

## 13. Diagnostics (`src/diagnostics.rs`)

With `--diagnostics`, a timestamped session directory is created under `logs/` containing:

- `session_info.json` — startup config (persona, model, tools, skills, system prompt, cwd, mode).
- `turn_NNN.json` — the full message array after each step (logged by the step guard).
- `persona_switch_after_turn_NNN.json` / `model_switch_after_turn_NNN.json` — config changes.
- `compaction_after_turn_NNN.json` — before/after token counts when compaction runs.

Diagnostics are wired in via two hooks: the **LLM-call hook** (logs each request snapshot)
and the **step guard** (logs each turn). The workshop API can read these sessions back.

---

## 14. End-to-end: what happens on one interactive turn

1. You type a line at the `[coding-agent metalcraft-agent]>` prompt.
2. If it's a slash command, it's handled directly (possibly rebuilding the agent).
3. Otherwise the input is appended to the conversation (`continue_with`) or starts a new one.
4. `compact_if_needed` may summarize old history.
5. The `Executor` runs the ReAct graph: the model thinks and may emit tool calls.
6. Each tool call passes through the **approval hook** (auto or prompt), then executes.
7. The **step guard** prints the call/result, watches for error spirals and loops, and (if
   on) logs the turn to diagnostics.
8. The loop continues until a final answer or `max_steps`/guard stop.
9. The final answer is printed and the updated state is kept for the next turn.

---

## 15. Building and running

```bash
cargo build
cargo test

# Interactive
cargo run --bin metalcraft-agent -- coding-agent

# One-shot
cargo run --bin metalcraft-agent -- coding-agent "summarize the README"

# Headless one-shot, no prompts
cargo run --bin metalcraft-agent -- --auto-approve coding-agent "run the tests"

# Flow daemon (single pass)
cargo run --bin metalcraft-daemon -- --once --auto-approve

# Workshop API only
cargo run --bin metalcraft-agent -- --api $WORKSHOP_API_KEY
```

Required: `OPENAI_API_KEY` (in `.env` or the environment). Optional: `OPENAI_MODEL`,
`METALCRAFT_DATA_DIR`, and the gateway/event/workshop variables described above.
</content>
</invoke>
