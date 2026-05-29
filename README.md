# Metalcraft Agent

Metalcraft Agent is a Rust application leveraging the Metalcraft framework to create reactive agents with various personas and functionalities. This agent can run interactively, execute one-shot tasks, or operate a flow scheduler daemon for local workflow files.

<img width="1542" height="867" alt="image" src="https://github.com/user-attachments/assets/6765878a-1484-4426-a7fd-cfd56c5f420f" />

## Features

- **Reactive Agent Creation**: Utilizes Metalcraft for creating agents with customizable behaviors.
- **Persona Management**: Define and manage different personas for specialized tasks.
- **Tool Interaction**: Interact with various tools with the option for auto-approval.
- **Async Execution**: Built on Tokio for efficient async operations.
- **Local Flow Scheduling**: Poll a local `flows/` directory and execute enabled workflows on an interval.

## Project Structure

- **Cargo.toml**: Configuration and dependencies for the Rust project.
- **src/main.rs**: Entry point for the interactive/one-shot agent CLI.
- **src/bin/metalcraft-daemon.rs**: Scheduler daemon binary for enabled local flows.
- **src/runtime.rs**: Shared one-shot agent runtime setup used by both binaries.
- **src/flows.rs**: Flow loading, schedule parsing, and MVP flow execution helpers.
- **src/lib.rs**: Core module declarations.
- **src/tools/**: Contains implementations for various tools used by the agent.
- **docs/**: Documentation and analysis for project features and upgrades.
- **skills/**: Descriptions of various skills and methodologies employed by the agent.
- **tests/**: Contains unit and integration tests for different modules.

## Agent Usage

```bash
metalcraft-agent [--auto-approve] [--diagnostics] <persona> [task]
```

- **`<persona>`**: The persona to be used by the agent.
- **`[task]`**: Specific task to be executed. If omitted, the agent enters interactive mode.
- **`--auto-approve`**: Automatically approve prompts for all tools.
- **`--diagnostics`**: Log full LLM call details to a timestamped session directory under `logs/`.

Flags can be combined and placed in any order before the persona argument.

### Examples

```bash
# Interactive mode with default approval prompts
metalcraft-agent coding-agent

# One-shot task
metalcraft-agent coding-agent "refactor the auth module"

# Skip all approval prompts
metalcraft-agent --auto-approve coding-agent

# Enable diagnostics logging
metalcraft-agent --diagnostics coding-agent

# Both flags together
metalcraft-agent --auto-approve --diagnostics coding-agent "fix the login bug"
```

## Flow Daemon Usage

`metalcraft-daemon` is a companion binary that polls a local flow directory, finds enabled workflow definitions, and runs reachable `prompt` nodes as one-shot agent tasks.

By default it looks for flow JSON files in `flows/`. It first checks `./flows` from the current working directory, then falls back to a `flows/` directory next to the executable. The `flows/` directory is intended for local workflow definitions and is gitignored by default, along with `logs/`.

```bash
cargo run --bin metalcraft-daemon -- --persona coding-agent --poll-seconds 30
```

You can also run a single scan and exit:

```bash
cargo run --bin metalcraft-daemon -- --once --auto-approve
```

### Daemon behavior

On each poll cycle, the daemon:

1. loads flow summaries from the configured flows directory
2. keeps only flows with `enabled: true`
3. validates each flow and parses the entry-node schedule
4. skips flows that are not currently due
5. traverses the graph from the single `entry` node in BFS order
6. executes each reachable `prompt` node using the configured persona and model

The daemon tracks in-memory run state so interval-based flows are only re-run once their configured time window has elapsed.

### Daemon flags

- **`--flows-dir <path>`**: Override the default `flows/` directory.
- **`--persona <slug>`**: Default persona for prompt nodes. Defaults to `coding-agent`. A flow can override this per-flow (entry node `data.persona`) or per-node (prompt node `data.persona`); see [Per-flow persona](#per-flow-persona).
- **`--model <name>`**: Model name to use. Defaults to `gpt-5.4`.
- **`--poll-seconds <n>`**: Poll interval for checking enabled flows. Defaults to `30`.
- **`--once`**: Perform one scan/run pass and exit.
- **`--auto-approve`**: Skip approval prompts for daemon-run tasks.
- **`--help` / `-h`**: Print daemon usage.

### Supported schedules and nodes

Current daemon behavior intentionally supports a limited MVP subset of the flow spec.

Supported schedules:

- `manual` — parsed, but never auto-run by the daemon
- `minutes`
- `hours`
- `cron` — run on a cron schedule (see [Cron schedules](#cron-schedules))

Supported node types:

- `entry`
- `prompt`

Not currently executed:

- `branch`
- `branch_tool`
- custom vendor node types

Other current constraints:

- the flow must contain exactly one `entry` node
- prompt nodes must include `data.prompt`
- only reachable prompt nodes are executed
- prompts run sequentially in BFS traversal order
- flow run history is kept in memory only for the current daemon process

### Cron schedules

Set `schedule_type` to `cron` and provide a `cron` expression in the entry node's `data`:

```json
{ "id": "entry", "node_type": "entry",
  "data": { "schedule_type": "cron", "cron": "0 0 0 * * *" }, "position": [0, 0] }
```

Notes:

- The expression uses the `cron` crate's **6- or 7-field** format
  (`sec min hour day-of-month month day-of-week [year]`) — seconds are **required**,
  so a standard 5-field crontab line will not parse. Shorthands like `@daily`,
  `@hourly`, and `@weekly` are also accepted.
- Examples: `0 0 0 * * *` = every day at 00:00; `0 30 9 * * Mon-Fri` = 09:30 on weekdays;
  `0 0 */6 * * *` = every 6 hours on the hour.
- Times are evaluated in the **daemon process's local timezone**, not UTC. To schedule
  in UTC, run the daemon with `TZ=UTC` (e.g. `TZ=UTC metalcraft-daemon ...` or set `TZ`
  in the container/Railway env).
- The expression is validated when the flow is loaded; an invalid expression causes the
  flow to be skipped with a logged warning.

### Per-flow persona

By default every prompt node runs as the daemon's `--persona`. A flow can override this:

- **Per-flow**: set `data.persona` on the `entry` node — applies to all prompt nodes.
- **Per-node**: set `data.persona` on an individual `prompt` node — overrides the
  flow-level value for just that prompt.

Resolution order for each prompt: prompt node `data.persona` → entry node `data.persona`
→ `--persona` flag (default `coding-agent`).

### Example flow file

```json
{
  "spec_version": "1",
  "id": "nightly-review",
  "name": "Nightly Review",
  "created_at": "2026-05-26T00:00:00Z",
  "updated_at": "2026-05-26T00:00:00Z",
  "enabled": true,
  "flow": {
    "nodes": [
      {
        "id": "entry",
        "node_type": "entry",
        "data": { "schedule_type": "hours", "interval": 24 },
        "position": [0, 0]
      },
      {
        "id": "task",
        "node_type": "prompt",
        "data": { "prompt": "Review the current project status and summarize the top priorities." },
        "position": [200, 0]
      }
    ],
    "edges": [
      {
        "id": "e1",
        "source": "entry",
        "target": "task"
      }
    ]
  }
}
```

Ensure you have the correct personas set up in the `personas/` directory to use this functionality effectively.

## Diagnostics

When `--diagnostics` is enabled, a timestamped session directory is created under `logs/` containing:

- **`session_info.json`** — startup configuration: persona, model, tools, skills, system prompt, working directory, and approval mode.
- **`turn_NNN.json`** — full message array after each agent step, capturing the complete LLM conversation including tool calls and results.
- **`persona_switch_after_turn_NNN.json`** — logged when the user switches personas mid-session via `/persona set`.
- **`model_switch_after_turn_NNN.json`** — logged when the user switches models mid-session via `/model use`.
- **`compaction_after_turn_NNN.json`** — logged when context compaction occurs, recording before/after token counts.

## Building and Testing

To build the project:

```bash
cargo build
```

To run tests:

```bash
cargo test
```

## Contributing

Contributions are welcome! Please make sure to update tests as appropriate and follow the existing style conventions.
