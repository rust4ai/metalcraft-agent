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
- **A flow runtime** — a stateful v2 flow state machine (with a legacy v1 prompt-collection
  path) that runs local workflow JSON.
- **Supporting subsystems** — tool approval, context compaction, a safety step-guard,
  diagnostics logging, gateway channels (inbound messaging webhooks), scheduled follow-ups,
  and a "workshop" admin REST API.

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
│  = create_react_agent_with_options│                      │
│  ⇒ CompiledGraph<AgentState>      │                      │
└───────────────┬──────────────────┘                      │
                │ TurnRunner::run → Executor::run(...)     │
                ▼                                          │
        ┌───────────────┐                                 │
        │  Agent loop    │  think → call tool → observe →… │
        │  (ReAct graph) │                                 │
        └───────────────┘                                 │
                                                           ▼
┌──────────────────────────────────────────────────────────────────┐
│                      metalcraft-daemon (bin)                       │
│  poll flows/ → due? → v2 state machine (or v1 one-shot prompts)    │
│  + fire scheduled follow-ups                                       │
│  optional: workshop API (hosts gateway-channel webhooks)           │
└──────────────────────────────────────────────────────────────────┘
```

---

## 2. Source map

| File | Responsibility |
|------|----------------|
| `src/main.rs` | Entry point for the CLI: arg parsing, REPL, slash commands, one-shot mode, `--api` dispatch. |
| `src/cli.rs` | CLI invocation parsing (persona/flags/positional task). |
| `src/bin/metalcraft-daemon.rs` | Thin daemon wrapper; delegates to `daemon::run`. |
| `src/daemon.rs` | Scheduler poll loop: flow due-checks, scheduled-follow-up firing, workshop API startup. |
| `src/runtime.rs` | Shared agent construction (`build_agent_runtime`) and `TurnRunner` — the single per-turn primitive (compact → executor) used by the CLI, daemon chat, and `run_one_shot_task`. |
| `src/persona.rs` | Persona load/save/list and templated system-prompt assembly; frontmatter parsing for skills. |
| `src/skill.rs` | Skill-file CRUD shared by the workshop API and `skill_*` meta tools. |
| `src/flows.rs` | Legacy v1 flow loading, schedule parsing, BFS traversal to collect reachable prompt texts. |
| `src/flow_exec.rs`, `src/flow_runs.rs` | v2 stateful flow executor (conditional/branch/tool/http/… nodes) with pause/resume and run persistence. |
| `src/tools/` | Tool implementations (core built-ins, HTTP-API tool, meta/integration tools) + the registry builder. |
| `src/approval.rs` | Tool-call classification + interactive approval prompts (incl. scrollable diff viewer). |
| `src/context.rs` | Token estimation + automatic conversation compaction. |
| `src/guard.rs` | Step guard: error-spiral and loop/poll detection; verbose tool-call printing. |
| `src/diagnostics.rs` | Optional per-session JSON logging of LLM calls, turns, and config changes. |
| `src/scheduled_tasks.rs` | Persisted scheduled follow-ups armed by `schedule_followup`, fired each daemon tick. |
| `src/integration_packs.rs` | Pack manifest, loading, and enable/disable state. |
| `src/key_store.rs` | Plaintext secret store referenced by `$NAME` placeholders. |
| `src/seed.rs` | Writes bundled personas/skills/flows/api_tools/flow_templates/integration_packs/gateway_channels into the data dir on startup. |
| `src/paths.rs` | Resolves the data directory and its subdirectories. |
| `src/workshop_api.rs` | Axum REST API for personas/skills/flows/tools/packs/keys; live chat (`run_chat_turn`) + SSE; gateway webhook ingress. |
| `src/gateway_channels.rs`, `src/gateway_activity.rs` | Gateway channel types/instances (inbound `/webhook/<adapter>`) + append-only traffic log. |
| `src/diff_preview.rs`, `src/ui.rs` | Diff rendering for approvals; terminal styling helpers. |

---

## 3. Startup and configuration

Both binaries do the same first two things in `main()`:

1. `env_logger::init()` — log level via `RUST_LOG`.
2. `metalcraft_agent::seed::ensure_defaults()` — creates the data directory and writes
   bundled seed files. It won't clobber files you've edited, except that a bundled persona
   with a newer `version` force-upgrades its installed copy (`write_versioned_seeds`).

### Where data lives (`src/paths.rs`)

The data root is resolved in priority order:

1. `METALCRAFT_DATA_DIR` env var (explicit override),
2. the OS app-data dir (`~/.local/share/metalcraft-agent` on Linux),
3. `./data` as a container-friendly fallback.

Subdirectories include `personas/`, `skills/`, `flows/`, `flow_templates/`, `api_tools/`,
`integration_packs/`, `gateway_channels/`, `chats/`, `runs/`, `traces/`, `uploads/`, and
`sessions/` (diagnostics — there is no `logs/`).

### Seeding (`src/seed.rs`)

Default personas, skills, api-tools, flow templates, integration packs, gateway channels, and
one example flow are compiled into the binary with `include_str!` from the `seed/` directory
and written out on first run. This means a fresh install is immediately usable, and users can
then edit the files in their data dir.

### Environment / API key (`src/runtime.rs`)

`AgentRuntimeContext::from_environment()` loads `.env` (via `dotenvy`), resolves the
personas/skills dirs, and requires `OPENAI_API_KEY`. The model defaults to `gpt-5.4`
(`DEFAULT_MODEL`), overridable by `OPENAI_MODEL`; available models are
`gpt-5.4-mini`, `gpt-5.4`, `gpt-5.5`.

---

## 4. How the agent is built

Everything funnels through `runtime::build_agent_runtime(...)`. Given a persona, cwd,
model name, approval mode, optional LLM-call and LLM-response hooks, a `RuntimeOptions`
struct (reply sink, tool-choice, terminal tools, session binding, reschedule depth), and a
`make_compaction_model` closure, it:

1. **Builds the system prompt** — `persona.build_system_prompt(skills_dir, cwd)` renders a
   mustache-style template. Placeholders like `{{cwd}}`, `{{available_skills}}`,
   `{{available_personas}}`, and `{{installed_packs}}` are substituted; for any the persona
   author didn't use, the corresponding section (working directory, an "Available Skills"
   list instructing the model to call `load_skill`, sub-agent personas, installed packs) is
   appended as a fallback.
2. **Builds the tool registry** — `tools::create_registry_for_with_config(&resolved, cfg)`,
   where `resolved = persona.resolved_tool_names()` (the persona's explicit tools plus any
   pack-provided tools), with terminal tools such as `say_to_user` injected if the session
   needs them (see §6).
3. **Creates the model** — `openai::Client::new(api_key).completion_model(model_name)`, and a
   separate `compaction_model` from `make_compaction_model` (see §8).
4. **Builds the approval hook** — `approval::build_hook(approval_mode)` (see §7).
5. **Compiles the graph** — `create_react_agent_with_options(model, registry, &system_prompt,
   AgentOptions { before_tool_call, llm_call_hook, llm_response_hook, tool_choice,
   terminal_tools })` and stores it as an `Arc<CompiledGraph<AgentState>>` so it can be
   cheaply cloned/shared across turns.

### Running a turn (`runtime::TurnRunner`)

`build_agent_runtime` only *builds* the runtime; every turn is executed through the shared
`TurnRunner`, which owns the one turn operation — **compact context, then run the executor**:

```rust
let (compacted, outcome) = TurnRunner::new(runtime).run(turn_state, step_guard).await;
```

`TurnRunner::run` first calls `context::compact_if_needed` (see §8), then:

```rust
Executor::new_from_arc(graph.clone())
    .max_steps(MAX_TURN_STEPS)   // = 90, one shared constant
    .with_step_guard(step_guard)
    .run(state, "agent").await
```

All three turn paths funnel through this: the CLI (`src/main.rs`, which builds one
`TurnRunner` and reuses it across REPL turns), the daemon chat path `run_chat_turn`
(`src/workshop_api.rs`), and one-shot tasks `run_one_shot_task` (`src/runtime.rs`, which
builds a `TurnRunner` per call). The step guard is passed to `run` rather than held by the
`TurnRunner`, because its lifetime differs per caller (session-long in the CLI; per-turn in
the daemon, where it also emits SSE tool events). Keeping compaction + `max_steps` + executor
wiring in this single place is what prevents a behaviour from being present in one turn path
and silently missing from another.

`run` returns a `RunOutcome`:

- `Completed(state)` — the agent produced a final answer (`state.final_answer()`).
- `Interrupted { state, reason, .. }` — stopped early (e.g. step guard tripped, or
  approval was denied), but the conversation state is preserved.
- `Failed { state, node, error }` — a node errored; the partial state is preserved.

The agent itself is a **ReAct loop** provided by `metalcraft`: the model thinks, optionally
emits a tool call, the framework executes the tool (subject to the before-tool-call hook),
appends the `ToolResult` to the message list, and repeats until the model returns a final
answer or `MAX_TURN_STEPS` (90) is hit.

`AgentState.messages` is a vector of `AgentMessage`:
`User`, `Assistant`, `ToolCall { name, args }`, `ToolResult { name, result }`.

### How a turn ends, and what can hold it open (`src/turn_plan.rs`)

In a daemon session the loop is **tool-only**: `tool_choice: Required`, and the turn ends
when the model calls a *terminal* tool — `say_to_user` (an answer) or `ask_user` (a
question). That is what makes ending a turn a decision the model makes, and it used to be a
decision nothing could question: after one `sub_agent` returned something plausible,
`say_to_user` was always the cheapest next move, so a four-step job routinely ended one step
in and looked finished.

Three pieces make the plan checkable instead:

- **`update_plan`** writes this turn's steps into a shared `TurnPlan` (`src/turn_plan.rs`),
  created per runtime by `build_agent_runtime` and cleared by `TurnRunner::run` at the start
  of every turn — the CLI reuses one runtime for a whole session, so the plan must not.
- **`sub_agent`** asks every delegate to end its report with a ```` ```handoff ```` block
  (`{completed, not_done, suggest_persona}`), strips it from the prose, and records a
  `Handoff` in the plan when a delegation comes back unfinished. An absent or unparseable
  block reads as complete, so a model that ignores the protocol behaves exactly as before.
- **`say_to_user`** asks `TurnPlan::blocking_reason()` before delivering. Open steps or an
  unacknowledged handoff mean the call returns an **error** listing what is still owed.

That last step only works because of a matching change in `metalcraft` 0.10:
`invoked_terminal_tool` now requires the terminal call to have *succeeded*. A failed
`say_to_user` returns control to the agent node instead of ending the turn — which also
fixes a real silent failure, where a reply whose sink errored ended the turn with nothing
delivered and the agent believing it had answered.

The gate is bounded (`MAX_GATE_REFUSALS = 2`): after two refusals the plan stops blocking
and the turn closes with the outstanding work visible in the answer. A rail that can trap a
turn is worse than the behaviour it corrects. `ask_user` is never gated — being stuck
mid-plan is precisely when asking is the right move.

A delegated sub-agent gets `turn_plan: None`: it runs its own turn and must be unable to
satisfy or block its parent's plan.

---

## 5. The CLI: interactive and one-shot modes (`src/main.rs`)

```
metalcraft-agent [--auto-approve] [--persona <slug>] [task]
```

Arg handling (`src/cli.rs::parse_cli_invocation`, unit-tested; env fallbacks applied in `main`):

- `--api <KEY>` (or `WORKSHOP_API_KEY` env) short-circuits everything and starts the
  workshop REST server instead of an agent (see §11).
- `--auto-approve` is a flag. `--persona <slug>` / `-p <slug>` (or the `METALCRAFT_PERSONA`
  env var) selects the persona, defaulting to the **Orchestrator** (`orchestrator-agent`).
  Persona is a flag — NOT a positional — so every remaining positional arg is part of the
  one-shot task (joined with spaces). This is what lets `metalcraft-agent "fix the bug"` work.
- The CLI always creates a diagnostics session directory for runs.
- If **stdin is not a TTY** (`atty`), approval is forced to auto-approve and a one-shot task
  is required (headless usage).

**One-shot mode** (a task was given): calls `runtime::run_one_shot_task(...)`, prints the
final answer or interruption reason, and exits.

**Interactive mode** (no task): a `rustyline` REPL. Each non-command line becomes a turn.
Conversation state persists across turns via `state.continue_with(input)` (or
`AgentState::new(input)` for the first turn). Each turn runs through the session's reused
`TurnRunner` (`turn_runner.run(...)`), which compacts old history if needed and then runs the
executor; the resulting state is stored for the next turn. The REPL prints a brief
`(context compacted)` notice on the turns where compaction fired.

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
| `update_plan` | Record this turn's steps; the reply tool is held to them (§4). |
| `ask_user` | Ask one clarifying question and end the turn waiting for the answer. |

Beyond these core built-ins, the registry also natively provides delivery tools
(`say_to_user`, `ask_user`, `gateway_send_message`, `schedule_followup`), meta tools that manage the
agent's own files (`persona_*`, `skill_*`, `flow_*`, `pack_*`, `key_*`, `diagnostics_*`), and
integration tools (`spaces_*` for S3/Spaces, `email_*` for IMAP). See `src/tools/mod.rs`.

Several tools need extra runtime config (`ToolConfig`: api_key, model name, system prompt,
skills dir, available skills, plus `reply_sink`, `session_binding`, `reschedule_depth`,
`interrupt`, and `turn_plan`):

- **`load_skill`** — needs the skills dir + the persona's allowed skill list, so its
  parameter schema can restrict `skill` to a known `enum`. It reads
  `<skills_dir>/<skill>.md`, strips YAML frontmatter, and returns the body.
- **`sub_agent`** — needs the api key/model/system prompt to build a child agent. It accepts
  a `task`, a `tool_set` (`read_only` default, `full`, or `all`, with an optional `pack`
  scope and a `persona` mode), builds its own registry and ReAct graph, runs it with a
  **120-second timeout** and `max_steps(90)`, and returns the child's final answer plus which
  tools it used and how many turns it took — and a `completed` flag (plus `not_done` /
  `suggest_persona`) parsed out of the delegate's handoff block, which it also records in the
  turn plan.
- **`say_to_user`** — routes a reply through the session's `reply_sink` (SSE for workshop
  chat, adapter send for gateway); it's the terminal tool for tool-only sessions, and the
  one the turn plan gates (§4).
- **`ask_user`** — the other terminal tool: delivers a question (with optional `options` a
  client can render as choices) and ends the turn waiting on the user, whose reply arrives as
  the next turn with the conversation intact. Never gated by the plan.
- **`update_plan`** — replaces this turn's plan wholesale; needs `turn_plan`, and is not
  registered without one (a sub-agent, a flow node, a one-shot run).
- **`schedule_followup`** — arms a persisted follow-up bound to the session (`session_binding`,
  `reschedule_depth`); the daemon fires it later (see §10).

### User-defined HTTP tools (`src/tools/http_api.rs`)

If a persona names a tool that isn't one of the built-ins, the registry tries to load it as
an **HTTP API tool** from `<data_dir>/api_tools/<name>.json`. This is how most pack tools
(GitHub, Linear, Cloudflare, Sentry, …) work without any hardcoded Rust. The JSON config
defines:

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

Example: `github_get_authenticated_user.json` GETs `https://api.github.com/user` with an
`Authorization: Bearer $GITHUB_TOKEN` header, where `$GITHUB_TOKEN` is resolved from the key
store at call time.

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
| MetaRead | read-only meta tools (`persona_get`, `flow_list`, `diagnostics_*`, …) | **auto** |
| MetaWrite | mutating meta tools (`persona_save`, `key_set`, `pack_enable`, …) | prompt |
| DiscordAction | discord_send/edit/add_reaction | prompt |

Classification is partly prefix-driven: read-only calls for the calendar/scheduling packs
(`calcom_`, `vestaloop_`, `mcal_`) and read-only Discord-admin calls (`discord_*` list/get/
search) auto-approve, while their mutating counterparts prompt. Two safety details: read-only
Discord chat tools (`discord_get_*`) classify as `ReadFile` (auto), and **unknown tools
default to `Execute`** (prompt) — fail safe. Approvals are also **remembered per session**:
once you approve an overwrite/edit to a given path, later writes to that same path in the
session skip the re-prompt.

When approval is required, the terminal prompt:

- For `edit_file`/`write_file`, computes a colored diff (`diff_preview`). Small diffs print
  inline with a Yes/No menu; large diffs open a **scrollable alternate-screen viewer**
  (PgUp/PgDn/Home/End to scroll, ↑/↓ or y/n/Enter to decide).
- For `bash`, shows the command; for others, the JSON args.
- The prompt runs on a dedicated OS thread (so it doesn't block the tokio runtime or fight
  with rustyline's terminal state) and **waits indefinitely** for the user's decision rather
  than auto-denying.

A denial returns `BeforeToolCallAction::Deny(reason)`, which the framework feeds back to the
model as the tool result so it can adapt.

---

## 8. Context compaction (`src/context.rs`)

Long conversations are summarized to stay under the context window.
`CompactionConfig` defaults: 128k-token window, compact at 60% utilization, always keep the
10 most recent messages intact.

`estimate_tokens` is a cheap heuristic (~4 chars per token across all message content).
Compaction runs inside `TurnRunner::run`, so it applies to **every** turn path — CLI,
workshop/gateway chat, and one-shot/flow tasks alike (previously one-shot runs skipped it).
`compact_if_needed`:

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
   outcome). `GuardConfig` tunes the thresholds (`max_consecutive_errors`,
   `max_identical_repeats`, `max_poll_repeats`, and the `poll_tools` set):
   - **Error spiral** — `max_consecutive_errors` (default 3) consecutive tool turns where
     *every* result starts with `ERROR:`.
   - **Loop detection** — an ordinary tool repeated byte-for-byte (name + args) more than
     `max_identical_repeats` (default 4) times **in a row** trips the guard; a few spaced
     repeats (e.g. `cargo check` between edits) are fine.
   - **Poll budget** — tools flagged as status **polls** (`poll_tools`) are exempt from the
     ordinary repeat limit and instead governed by the much higher `max_poll_repeats`
     (default 60), so polling an async job isn't mistaken for a runaway loop. One-shot runs
     seed `poll_tools` from the persona's HTTP poll tools.
   - Denied/interrupted calls are **retracted** from both the loop and error-spiral tallies,
     so a call the user rejected doesn't count toward a stop.

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
- `cron` — a `data.cron` expression, validated with the `cron` crate at load time (a bad
  expression causes the flow to be skipped). The crate uses a **6/7-field** format
  (`sec min hour dom month dow [year]`, seconds required) plus `@daily`/`@hourly`/etc.
  shorthands. **It runs** — see the poll loop below — and is evaluated in the daemon's
  **local timezone** (`chrono::Local`); use `TZ=UTC` for UTC scheduling.

There are **two execution models**, selected per flow by `flow_exec::is_v2_flow`:

- **v2 (stateful state machine, `src/flow_exec.rs`)** — the daemon calls `run_flow_v2`, which
  walks the graph one node at a time, threading a shared `variables` object and routing by
  output handle. Node types: `entry`, `prompt`, `set_variable`, `tool`, `conditional` (with
  `branch`, `http`, `sub_agent`, `approval`, `wait`, `foreach` staged). Runs persist to
  `<data>/runs/` (`src/flow_runs.rs`) so an `approval`/`wait` node can pause and later resume
  (`resume_flow`).
- **v1 (legacy, `src/flows.rs`)** — `collect_reachable_prompts` does a **BFS from the single
  entry node** and runs each reachable `prompt` node as a one-shot task, in traversal order.
  **Persona resolution** per prompt: the prompt node's `data.persona`, else the entry node's
  `data.persona` (flow-wide default), else the daemon's `--persona` flag. This older path
  handles only `entry`/`prompt` nodes.

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
3. If due and not already marked running, mark it running, set `last_started_at`, and run it:
   v2 flows go through `run_flow_v2`; v1 flows collect reachable prompts and run each via
   `runtime::run_one_shot_task`. Model and approval settings come from the daemon. Results are
   logged.
4. Separately each tick, `run_due_scheduled_tasks` fires any due **scheduled follow-ups**
   (`src/scheduled_tasks.rs`, armed by the `schedule_followup` tool) — delivering them back to
   their bound session (e.g. a workshop chat).
5. The v1 interval scheduler keeps run state **in-memory only** (a `HashMap<flow_id,
   FlowRunState>`) — restarting the daemon forgets history, so interval flows run again on next
   start. (v2 flow *runs* are persisted to `<data>/runs/` for resume, which is separate.)
6. With `--once`, exit after one pass; otherwise sleep `poll-seconds` and repeat.

Daemon flags: `--flows-dir`, `--persona` (default `coding-agent`), `--model`,
`--poll-seconds` (default 30), `--once`, `--auto-approve`, and the workshop-API flags
(`--api`, `--api-port`) below. The former `--event-*` / `--events` flags are deprecated
no-ops.

---

## 11. The workshop REST API (`src/workshop_api.rs`)

An optional Axum server for a desktop "workshop" app to manage the agent's files **and** run
it live. Enabled by `--api <KEY>` (or `WORKSHOP_API_KEY`) on **either** binary —
`metalcraft-agent --api` runs it standalone; `metalcraft-daemon --api` spawns it alongside the
scheduler so one process does both. Port defaults to 3002 (`--api-port` / `WORKSHOP_API_PORT`
/ `PORT`). `/health` and `/info` are open; the rest require a `Bearer <KEY>` header.

Route families (all under `/api/v1` unless noted):

- **Files** — `GET /snapshot`; `GET|PUT|DELETE /personas/{slug}`, `/skills/{slug}`,
  `/flows/{id}`, `/api-tools/{name}`; `GET /diagnostics`, `GET /diagnostics/{id}`.
- **Flows & runs** — `POST /flows/{id}/run`; `/flow-runs`, `/flow-runs/{id}`,
  `POST /flow-runs/{id}/resume`; `/flow-templates*`.
- **Keys & packs** — `/keys*` (incl. `GET /keys/recommended`); `/integration-packs*`.
- **Live chat** — `/chats*`, including `POST /chats/{id}/turn` (a real agent turn via
  `run_chat_turn`) and an SSE stream at `/chats/{id}/events`.
- **Scheduling & gateway** — `/scheduled-tasks*`; `/gateway/*`; inbound webhooks at
  `/webhook/pipestreamr` and `/webhook/twilio` (see §12).

File routes read and write the same data-dir files the agent and daemon use, so edits take
effect on the next agent build / flow poll. The full contract lives in
`openapi/workshop-api.yaml`.

---

## 12. Gateway channels (`src/gateway_channels.rs`)

The old standalone event listener was removed. Inbound messaging now arrives through
**gateway channels** hosted inside the workshop API. A **channel type** is a JSON manifest
(WhatsApp via PipeStreamr/Twilio ships today); a **channel instance** is a user-created
binding persisted in `<data>/gateway_channels.json`. Inbound webhooks land at
`/webhook/<adapter>` on the workshop API, get turned into agent turns (`run_chat_turn`), and
the agent replies via `gateway_send_message` / `say_to_user`. All inbound/outbound traffic is
appended to `<data>/gateway_activity.jsonl` (`src/gateway_activity.rs`). This is what lets the
agent run "reactively" — e.g. answering WhatsApp messages.

---

## 13. Diagnostics (`src/diagnostics.rs`)

With each CLI run (and for flow runs), a timestamped session directory is created under `sessions/` containing:

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
4. The turn runs through `TurnRunner::run`, which first calls `compact_if_needed` to summarize
   old history if the context is over threshold.
5. Inside the same call, the `Executor` runs the ReAct graph: the model thinks and may emit
   tool calls.
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

# Interactive (default Orchestrator persona)
cargo run --bin metalcraft-agent

# One-shot
cargo run --bin metalcraft-agent -- "summarize the README"

# Pick a persona
cargo run --bin metalcraft-agent -- --persona coding-agent "summarize the README"

# Headless one-shot, no prompts
cargo run --bin metalcraft-agent -- --auto-approve "run the tests"

# Flow daemon (single pass)
cargo run --bin metalcraft-daemon -- --once --auto-approve

# Workshop API only
cargo run --bin metalcraft-agent -- --api $WORKSHOP_API_KEY
```

Required: `OPENAI_API_KEY` (in `.env` or the environment). Optional: `OPENAI_MODEL`,
`METALCRAFT_DATA_DIR`, and the gateway/event/workshop variables described above.
</content>
</invoke>
