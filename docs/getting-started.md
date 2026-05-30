# Metalcraft Agent — Getting Started

How to build, run, configure, and deploy Metalcraft Agent. For concepts see
**[overview.md](overview.md)**; for internals see **[architecture.md](architecture.md)**.

## Prerequisites

- A Rust toolchain (edition 2024).
- An OpenAI-compatible LLM endpoint and API key.

## Build

```bash
cargo build              # debug build
cargo build --release    # optimized build
cargo build --bin metalcraft-daemon   # just the daemon
cargo test               # run the test suite
```

Binaries land in `target/debug/` or `target/release/`.

## Configuration (environment variables)

Configuration is read from the environment and a `.env` file.

| Variable | Purpose |
| --- | --- |
| `OPENAI_API_KEY` | Credentials for the LLM endpoint (required) |
| `OPENAI_MODEL` | Model override (defaults to a built-in GPT-class model) |
| `METALCRAFT_DATA_DIR` | Explicit data-directory path (otherwise OS app-data dir, then `./data`) |
| `WORKSHOP_API_KEY` / `WORKSHOP_API_PORT` | Workshop API auth key and port |
| `AGENT_GATEWAY_URL` / `AGENT_GATEWAY_API_KEY` | Gateway used by the Discord pack and event relay |
| `EVENTD_WEBHOOK_SECRET` | Shared secret for verifying inbound event webhooks |
| `SOLARABASE_API_KEY` | Credential for the Solarabase RAG pack |
| `RUST_LOG` | Log verbosity |
| `TZ` | Timezone used for cron schedule evaluation (e.g. `TZ=UTC`) |

Pack-specific secrets are normally stored in the **key store** (`keys.json`) and referenced by
HTTP tools via `$NAME` placeholders rather than being read directly from the environment. The
Workshop endpoint `GET /api/v1/keys/recommended` lists the keys that enabled packs expect.

## Running the agent CLI

```bash
metalcraft-agent [--auto-approve] <persona> [task]
```

- `<persona>` — persona to use (defaults to `coding-agent`).
- `[task]` — a single request; omit it to enter the interactive REPL.
- `--auto-approve` — skip all approval prompts (required for non-interactive use).
- Sessions are always logged to `logs/<timestamp>/`.

Examples:

```bash
# Interactive REPL with approval prompts
metalcraft-agent coding-agent

# One-shot task
metalcraft-agent coding-agent "refactor the auth module"

# Headless, no prompts, with session logging
metalcraft-agent --auto-approve coding-agent "run the test suite and fix failures"

# From source
cargo run -- coding-agent
```

## Running the daemon

The daemon polls `<data>/flows/`, runs due flows, and optionally serves the Workshop API and
event listener.

```bash
metalcraft-daemon [--persona P] [--model M] [--poll-seconds N] [--once]
                  [--api KEY] [--api-port PORT]
                  [--event-port PORT] [--event-host HOST] [--events E1,E2]
                  [--auto-approve]
```

Examples:

```bash
# Run the scheduler loop (auto-approving, since there is no TTY)
metalcraft-daemon --auto-approve

# Evaluate flows once and exit (useful for cron/CI)
metalcraft-daemon --once --auto-approve

# Also serve the Workshop API
metalcraft-daemon --auto-approve --api my-secret-key --api-port 3002

# From source
cargo run --bin metalcraft-daemon -- --once --auto-approve
```

## Working with personas and skills

- **Personas** are JSON files in `<data>/personas/`. Each defines a name, description, system
  prompt, the tools it may call, and any attached skills. Add a new one by dropping in a JSON
  file (or via the Workshop API).
- **Skills** are Markdown files in `<data>/skills/`. They hold reusable methodology that an
  agent loads with the `load_skill` tool. Add one by creating a `.md` file.

Bundled defaults are seeded on first run and never overwrite your edits.

## Integration packs

Packs bundle personas, skills, HTTP tools, and flow templates. They are disabled by default.

- List and toggle packs via the Workshop API
  (`GET /api/v1/integration-packs`, `PUT /api/v1/integration-packs/{id}/enabled`).
- After enabling a pack, store its required secrets in the key store
  (`PUT /api/v1/keys/{name}`), guided by `GET /api/v1/keys/recommended`.
- Shipped packs: **Discord** (needs `AGENT_GATEWAY_URL` + `AGENT_GATEWAY_API_KEY`) and
  **Solarabase** RAG (needs `SOLARABASE_API_KEY`).

## Defining custom HTTP tools

Create a JSON definition in `<data>/api_tools/` (or via `PUT /api/v1/api-tools/{name}`)
describing the HTTP method, URL, headers, and body template. Reference secrets as `$NAME` so
they resolve from the key store at call time. No recompile is needed — the tool becomes
available to personas that list it.

## Deployment

- **Docker:** a multi-stage `Dockerfile` (Rust builder → Debian runtime) and a
  `docker-compose.yml` run the daemon. Set env vars and mount/point `METALCRAFT_DATA_DIR` at a
  persistent volume.

  ```bash
  docker build -t metalcraft-agent .
  docker-compose up -d
  ```

- **Railway:** `railway.toml` is preconfigured. Set `OPENAI_API_KEY`, `WORKSHOP_API_KEY`,
  `AGENT_GATEWAY_URL`, `TZ`, and any pack secrets as service variables.

See `devops.md` at the repo root for the full operations guide.

## Diagnostics

The CLI always creates a timestamped session directory under `logs/<timestamp>/`, and flow runs do the same:

- `session_info.json` — session metadata
- `turn_NNN.json` — each LLM turn (request + response)
- `compaction_after_NNN.json` — context-compaction events
- `persona_switch_after_NNN.json` — persona changes

Sessions are also served through the Workshop API at
`GET /api/v1/diagnostics` and `GET /api/v1/diagnostics/{id}`.
