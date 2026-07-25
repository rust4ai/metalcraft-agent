# Metalcraft Agent

**A batteries-included, self-hosted AI agent — built in Rust on the [Metalcraft](https://github.com/rust4ai/metalcraft) framework.**

Every task runs as a *persona*: a **secured, scoped set of tools**, not a blank
check. An Orchestrator delegates each job to the specialist built for it, so a
research task is read-only and the Railway persona can touch Railway and nothing
else. Run it as an interactive CLI, a one-shot task, or a long-lived daemon that
serves an HTTP API, answers inbound messages, and fires scheduled workflows —
behind your own domain, on your own box. You hold the keys.

[![license](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![website](https://img.shields.io/badge/site-metalcraftai.com-4d6a9c)](https://metalcraftai.com)
[![discord](https://img.shields.io/badge/Discord-join-5865F2)](https://discord.gg/9FqRMsmVt2)

<img width="1542" height="867" alt="Metalcraft Workshop" src="https://github.com/user-attachments/assets/6765878a-1484-4426-a7fd-cfd56c5f420f" />

## Quickstart

The agent ships as a public prebuilt image — no Rust toolchain, no compile.

```bash
docker run -d --name metalcraft \
  -e OPENAI_API_KEY=sk-... \
  -e WORKSHOP_API_KEY=a-long-secret \
  -e METALCRAFT_DATA_DIR=/data -v metalcraft:/data \
  -p 3002:3002 \
  ghcr.io/rust4ai/metalcraft-agent

curl localhost:3002/health          # {"status":"ok","version":"0.8.2"}
```

It's now live on `:3002` — connect the [Metalcraft Workshop](https://github.com/rust4ai/metalcraft-workshop)
desktop app to `http://localhost:3002`, or drive it over the HTTP API.

For a first-time, HTTPS-behind-Caddy deploy, the repo's helper scripts wrap it
all (they create `.env`, validate required vars, pull, and start):

```bash
./start-agent.sh          # first-time bring-up (Caddy/HTTPS by default)
./update-agent.sh         # pull a newer release later (TAG=0.8.2 to pin)
```

**Other ways to run it**

```bash
# Install the binary from source (always current):
cargo install --git https://github.com/rust4ai/metalcraft-agent metalcraft-agent

# Or from a clone — interactive CLI:
echo "OPENAI_API_KEY=sk-..." > .env
cargo run --bin metalcraft-agent "refactor the auth module"
```

The **only required variable is `OPENAI_API_KEY`**. See [Configuration](#configuration).

## Why Metalcraft Agent

- **Secured toolsets** — a request runs *as a persona*: a scoped tool set plus
  its own system prompt and skills. Least privilege, per task.
- **Orchestrator delegation** — the default `orchestrator-agent` breaks work
  into sub-tasks and hands each to the specialist persona built for it.
- **Integration packs** — add capabilities (Railway, GitHub, Render, Cloudflare,
  Linear, Sentry, Discord, email, …) as JSON, or write your own HTTP-API tools.
- **Skills** — reusable, prompt-authored methodologies a persona can load on demand.
- **Scheduled flows** — a daemon polls a `flows/` directory and runs enabled
  workflows on cron or interval schedules.
- **Gateway channels** — route inbound webhook messages to a persona and reply
  back through the same channel (PipeStreamr adapter shipped).
- **Scheduled follow-ups** — the agent can defer work (`schedule_followup`) and
  wake itself later to re-check something, instead of asking you to ping it back.
- **The Workshop** — a desktop app to author personas/skills/flows, run chats,
  manage packs and keys, and inspect past runs. Everything it does is also
  available by prompt via the `workshop-agent` persona.
- **HTTP API + webhooks** — a `WORKSHOP_API_KEY`-gated `/api/v1/*` surface.
- **OpenTelemetry traces** — every chat turn emits an OTLP/JSON trace.
- **Self-hostable** — one container, no database; all state is files in one
  data directory. Deploy behind Caddy for automatic HTTPS.

## Core concepts

| Concept | What it is |
|---------|-----------|
| **Persona** | A named bundle of *tools + system prompt + skills*. Every run happens under one. Default: `orchestrator-agent`. |
| **Skill** | A markdown methodology a persona can `load_skill` on demand. |
| **Integration pack** | A versioned directory of personas/skills/HTTP-API tools for one service, seeded into the data dir and enabled at runtime. |
| **Flow** | A saved graph of prompt/branch nodes with a cron/interval schedule, run by the daemon. |
| **Gateway channel** | An inbound message route (webhook) bound to a persona, with replies sent back through the adapter. |
| **Key store** | Runtime secret storage (`PUT /api/v1/keys/<NAME>`); HTTP tools reference secrets by `$NAME` — never env vars. |

See [docs/architecture.md](docs/architecture.md) and the docs at
[metalcraftai.com/docs](https://metalcraftai.com/docs) for the full model.

## Running it

**Interactive / one-shot CLI** — `metalcraft-agent [--auto-approve] [--persona <slug>] [task]`

```bash
metalcraft-agent                                  # interactive, Orchestrator persona
metalcraft-agent "refactor the auth module"       # one-shot; Orchestrator delegates
metalcraft-agent --persona coding-agent "fix the login bug"
metalcraft-agent -p workshop-agent "create a skill 'greeting' that says hello"
```

- `--persona <slug>` / `-p` — persona to run as (default `orchestrator-agent`;
  also settable via `METALCRAFT_PERSONA`).
- `--auto-approve` — skip the approval prompt for every tool call.
- With no `[task]`, the agent enters interactive mode. Sessions are logged to a
  timestamped session directory.

**Daemon** — `metalcraft-daemon` runs the flow scheduler and (with
`WORKSHOP_API_KEY` set) the HTTP API + inbound gateway webhooks. This is what the
container runs.

```bash
metalcraft-daemon --auto-approve                       # poll + serve
metalcraft-daemon --persona coding-agent --poll-seconds 30
metalcraft-daemon --once                               # run due flows once, then exit
```

**Manage the project by prompt** — the `workshop-agent` persona exposes the
Workshop's authoring surface (personas, skills, flows, diagnostics) as agent
tools, so you can edit the project itself in natural language.

## Configuration

The daemon loads `.env` on startup (via `dotenvy`). Copy the template:
`cp .env.example .env`. The only required variable is `OPENAI_API_KEY`.

| Variable | Needed for | Default |
|----------|-----------|---------|
| `OPENAI_API_KEY` | **inference** (required) | — |
| `WORKSHOP_API_KEY` | enabling the HTTP API + webhooks; Bearer token for `/api/v1/*`. Unset = no HTTP server. | unset |
| `PORT` / `WORKSHOP_API_PORT` | port the API binds on `0.0.0.0` | `3002` |
| `OPENAI_MODEL` | override the LLM model | `gpt-5.4` |
| `METALCRAFT_PERSONA` | CLI default persona | `orchestrator-agent` |
| `METALCRAFT_DEFAULT_PERSONA` | persona the Workshop's New Chat defaults to | `orchestrator-agent` |
| `METALCRAFT_DATA_DIR` | where personas/skills/flows/chats/keys live | OS data dir |
| `RUST_LOG` | log level | `info` |

Daemon flow settings also read `STARKBOT_PERSONA` (default `coding-agent`),
`STARKBOT_POLL_SECONDS`, `STARKBOT_FLOWS_DIR`, `STARKBOT_AUTO_APPROVE`, and
`STARKBOT_ONCE` — the legacy names still honored by the scheduler.

> **Secrets** (e.g. `TWILIO_ACCOUNT_SID`, integration-pack API keys) are **not**
> env vars — store them at runtime via the key store (`PUT /api/v1/keys/<NAME>`).
> HTTP tools reference them by `$NAME`; `GET /api/v1/keys/recommended` lists what
> enabled packs expect.

## Integrations & packs

Packs are versioned directories under `seed/integration_packs/<id>/`, seeded into
the data dir on startup and enabled at runtime (`pack_enable`, or the Workshop's
Packs tab). Shipped packs:

`calcom` · `cloudflare` · `digitalocean_spaces` · `discord` · `discord_admin` ·
`email` (IMAP, read-only) · `github` · `linear` · `railway` · `render` ·
`sentry` · `solarabase` (RAG) · `sprite_builder` · `starflask` (media)

Most tools are declarative JSON (HTTP-API tools with `$SECRET` substitution); a
few that need non-HTTP protocols or request signing ship as native Rust tools.
`scripts/smoke_packs.py` runs the read-only tools of a pack against the live API
to catch query drift.

## Deploy & self-host

One container, no database — all state is files under one data directory, so
back it up by backing up a volume.

- **Docker + Caddy (automatic HTTPS):** the repo ships a `Dockerfile`,
  `docker-compose.yml`, a `Caddyfile`, and `docker-compose.caddy.yml`. Caddy
  terminates TLS and reverse-proxies to the daemon; the daemon is never published
  to the host.
- **Helper scripts:** [`start-agent.sh`](start-agent.sh) (first-run) and
  [`update-agent.sh`](update-agent.sh) (pull a newer image). Both default to the
  Caddy compose file and honor `COMPOSE_FILE=` / `TAG=`.
- **Data directory** resolves in order: `METALCRAFT_DATA_DIR` → OS data dir →
  `./data`. It holds `personas/ skills/ flows/ api_tools/ integration_packs/
  keys.json chats/ logs/ traces/`.

Full walkthroughs (DigitalOcean droplet, App Platform, plain Docker) are in
[devops.md](devops.md) and at [metalcraftai.com/docs/deployment](https://metalcraftai.com/docs/deployment).

## Observability

Every Workshop-API chat session emits a standard **OpenTelemetry** trace
(GenAI semantic conventions) to `<data>/traces/<session>/otlp-trace.json` — a
session span nesting one span per turn, per LLM call (model, duration, tokens),
and per tool call. It's plain OTLP/JSON, so it ingests directly into Phoenix,
Langfuse, Braintrust, or an OpenTelemetry Collector. Tracing is best-effort and
never blocks a turn.

## Project structure

- `src/main.rs` — interactive / one-shot agent CLI
- `src/bin/metalcraft-daemon.rs` — flow scheduler + HTTP API daemon
- `src/runtime.rs` — shared agent runtime; `src/workshop_api.rs` — the HTTP API
- `src/tools/` — native tool implementations; `src/scheduled_tasks.rs` — follow-ups
- `seed/` — bundled personas, skills, integration packs, gateway channel types
- `docs/` — architecture and design notes

## Building & testing

```bash
cargo build --release      # binaries: metalcraft-agent, metalcraft-daemon
cargo test                 # unit + integration tests
cargo run --bin metalcraft-agent
```

Requires a recent Rust toolchain (edition 2024).

## Links

- **Website:** [metalcraftai.com](https://metalcraftai.com) · [docs](https://metalcraftai.com/docs)
- **Framework:** [rust4ai/metalcraft](https://github.com/rust4ai/metalcraft)
- **Workshop (desktop app):** [rust4ai/metalcraft-workshop](https://github.com/rust4ai/metalcraft-workshop)
- **Community:** [Discord](https://discord.gg/9FqRMsmVt2)

## Contributing

Issues and PRs welcome. Please run `cargo test` and `cargo clippy` before opening
a PR. MIT licensed — see [LICENSE](LICENSE).
