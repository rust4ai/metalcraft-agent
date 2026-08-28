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

curl localhost:3002/health          # {"status":"ok","name":"metalcraft-agent","version":"0.12.0"}
```

It's now live on `:3002` — connect the [Metalcraft Workshop](https://github.com/rust4ai/metalcraft-workshop)
desktop app to `http://localhost:3002`, or drive it over the HTTP API.

For a first-time, HTTPS-behind-Caddy deploy, the repo's helper scripts wrap it
all (they create `.env`, validate required vars, pull, and start):

```bash
./start-agent.sh          # first-time bring-up (Caddy/HTTPS by default)
./update-agent.sh         # pull a newer release later (TAG=0.12.0 to pin)
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
  Linear, Sentry, Discord, calendar, notes, contacts, email, …) as JSON, or
  write your own HTTP-API tools.
- **Skills** — reusable, prompt-authored methodologies a persona can load on demand.
- **Stateful flows** — a daemon runs saved flows on cron/interval schedules. A
  flow is a state machine: prompt, HTTP, and sub-agent nodes, `branch`/
  `conditional` routing, and durable `approval`/`wait` pauses that resume later.
- **Gateway channels** — route inbound webhook messages to a persona and reply
  back through the same channel. Zero-copy "connect" to the Metalcraft Gateway,
  which self-heals rotated credentials.
- **Scheduled follow-ups** — the agent can defer work (`schedule_followup`) and
  wake itself later to re-check something, instead of asking you to ping it back.
- **Grounded in time** — every turn is stamped with the current UTC time, and the
  calendar pack is timezone-aware (`mcal_now`), so relative dates resolve correctly.
- **The Workshop** — a desktop app to author personas/skills/flows, run chats,
  manage packs and keys, and inspect past runs. Everything it does is also
  available by prompt via the `workshop-agent` persona.
- **HTTP API + webhooks** — a `/api/v1/*` surface, authenticated by a static key
  or by Metalcraft ID (OIDC) tokens.
- **OpenTelemetry traces** — every chat turn emits an OTLP/JSON trace.
- **Self-hostable** — one container, no database; all state is files in one
  data directory. Deploy behind Caddy for automatic HTTPS.

## Core concepts

| Concept | What it is |
|---------|-----------|
| **Persona** | A named bundle of *tools + system prompt + skills*. Every run happens under one. Default: `orchestrator-agent`. |
| **Skill** | A markdown methodology a persona can `load_skill` on demand. |
| **Integration pack** | A versioned directory of personas/skills/HTTP-API tools for one service, seeded into the data dir and enabled at runtime. |
| **Flow** | A saved state machine of prompt/HTTP/sub-agent/branch/conditional/approval/wait nodes, run on a cron/interval schedule by the daemon. v2 runs are durable and can pause and resume. |
| **Gateway channel** | An inbound message route (webhook) bound to a persona, with replies sent back through the adapter. |
| **Key store** | Runtime secret storage (`PUT /api/v1/keys/<NAME>`), global or channel-scoped; HTTP tools reference secrets by `$NAME` — never env vars. |

See [docs/architecture.md](docs/architecture.md) and the docs at
[metalcraftai.com/docs](https://metalcraftai.com/docs) for the full model.

Anything with a face — a web UI, a native client, a marketing page — is built to
[docs/design-system.md](docs/design-system.md): the palette, type, geometry and
component language of the landing hero, written down so a second surface can be
built without looking at the first.

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

**Daemon** — `metalcraft-daemon` runs the flow scheduler and, when the API is
enabled, the HTTP API + inbound gateway webhooks. This is what the container runs.

```bash
metalcraft-daemon --auto-approve                       # poll + serve
metalcraft-daemon --api my-secret --persona coding-agent --poll-seconds 30
metalcraft-daemon --api-oidc                            # API with OIDC-only auth, no static key
metalcraft-daemon --once                                # run due flows once, then exit
```

The API is served when a static key is present (`--api <KEY>` or `WORKSHOP_API_KEY`)
**or** OIDC-only mode is on (`--api-oidc` or `WORKSHOP_API_ENABLED=1`); otherwise
the daemon runs the flow scheduler only.

**Manage the project by prompt** — the `workshop-agent` persona exposes the
Workshop's authoring surface (personas, skills, flows, diagnostics) as agent
tools, so you can edit the project itself in natural language.

## HTTP API authentication

The `/api/v1/*` surface accepts a Bearer token in one of two modes (`/health`
and `/` stay unauthenticated for platform probes):

- **Static key** — set `WORKSHOP_API_KEY` to a long random secret; it is the
  Bearer token. The simplest path for self-hosting.
- **Metalcraft ID (OIDC)** — set `WORKSHOP_API_ENABLED=1` (or `--api-oidc`) to
  serve **without** a static key. Callers authenticate with a Metalcraft ID
  token (`mck_…`): either the pod owner's PAT, or a token audience-scoped to
  this pod (`pod:{slug}`, e.g. a connection token minted by the control plane).
  Used by managed pods that mint no static key. The pod is identified by
  `POD_PUBLIC_URL`.

Both may be enabled at once; positive OIDC verifications are cached briefly.

## Configuration

The daemon loads `.env` on startup (via `dotenvy`). Copy the template:
`cp .env.example .env`. The only required variable is `OPENAI_API_KEY`.

| Variable | Needed for | Default |
|----------|-----------|---------|
| `OPENAI_API_KEY` | **inference** (required) | — |
| `WORKSHOP_API_KEY` | static-key HTTP API + webhooks; Bearer token for `/api/v1/*`. Unset (and no OIDC) = no HTTP server. | unset |
| `WORKSHOP_API_ENABLED` | serve the HTTP API with **no static key**, authenticating via Metalcraft ID (`mck_`) tokens (`--api-oidc` flag equivalent). | unset |
| `PORT` / `WORKSHOP_API_PORT` | port the API binds on `0.0.0.0` | `3002` |
| `METALCRAFT_MODEL` | LLM model for the daemon/Workshop/flows (`STARKBOT_MODEL` also honored; the CLI reads `OPENAI_MODEL`) | `gpt-5.4` |
| `OPENAI_BASE_URL` | route inference through a gateway (e.g. Metalcraft Inference for auth + credit metering) | OpenAI |
| `METALCRAFT_TOKEN` | a Metalcraft ID PAT (`mck_…`); first-party packs (calendar, notes, contacts, …) and the gateway authenticate with it automatically | unset |
| `METALCRAFT_PERSONA` | CLI default persona | `orchestrator-agent` |
| `METALCRAFT_DEFAULT_PERSONA` | persona the Workshop's New Chat defaults to | `orchestrator-agent` |
| `METALCRAFT_DATA_DIR` | where personas/skills/flows/chats/keys live | OS data dir |
| `POD_PUBLIC_URL` | the pod's public URL; identifies the pod for OIDC audience checks and the gateway webhook | unset |
| `RUST_LOG` | log level | `info` |

Daemon flow settings also read `STARKBOT_PERSONA` (default `coding-agent`),
`STARKBOT_POLL_SECONDS`, `STARKBOT_FLOWS_DIR`, `STARKBOT_AUTO_APPROVE`, and
`STARKBOT_ONCE` — the legacy names still honored by the scheduler.

> **Secrets** (e.g. `TWILIO_ACCOUNT_SID`, integration-pack API keys) are **not**
> env vars — store them at runtime via the key store (`PUT /api/v1/keys/<NAME>`).
> HTTP tools reference them by `$NAME`; `GET /api/v1/keys/recommended` lists what
> enabled packs expect. Keys can be **global** or scoped to a gateway channel.

## Integrations & packs

Packs are versioned directories under `seed/integration_packs/<id>/`, seeded into
the data dir on startup and enabled at runtime (`pack_enable`, or the Workshop's
Packs tab). Shipped packs:

`calcom` · `cloudflare` · `digitalocean_spaces` · `discord` · `discord_admin` ·
`email` (IMAP, read-only) · `github` · `linear` · `metalcraft-calendar`
(timezone-aware) · `metalcraft-contacts` (CRM) · `metalcraft-notes` (markdown
notes) · `railway` · `render` · `sentry` · `solarabase` (RAG) · `sprite_builder`
· `starflask` (media)

Most tools are declarative JSON (HTTP-API tools with `$SECRET` substitution); a
few that need non-HTTP protocols or request signing ship as native Rust tools.
First-party Metalcraft packs (calendar, contacts, notes, …) authenticate with a
single `METALCRAFT_TOKEN`. `scripts/smoke_packs.py` runs the read-only tools of a
pack against the live API to catch query drift.

## Gateway channels

A gateway channel routes an inbound webhook to a persona and sends the reply back
through the same adapter. Two channel types ship: **PipeStreamr** (the message
transport) and **Metalcraft Gateway** (platform-managed SMS via a claimed number).

The Metalcraft Gateway connects **zero-copy**: because the pod already holds the
user's `METALCRAFT_TOKEN`, `POST /api/v1/gateway/metalcraft/connect` fetches the
base URL, integration id, webhook secret, and active number, registers the pod's
inbound webhook, and writes **channel-scoped** secrets — no pasting. The daemon
runs a self-heal loop that refreshes an adopted connection token before expiry
and re-syncs a rotated secret or reassigned number. Channel-owned secrets are
managed (read-only in the Keys UI); a scope-aware keys API (`GET /api/v1/keys`)
groups global keys and per-channel secrets.

## Flows

A flow is a durable state machine the daemon runs on a schedule (or via
`POST /api/v1/flows/{id}/run`). Node types:

- **prompt** — run a persona on a prompt (with `{{input}}` templating).
- **http** — call an HTTP API and capture the response.
- **sub_agent** — delegate to another persona.
- **branch** / **conditional** — route to different nodes on the result.
- **approval** / **wait** — pause the run durably and resume it later (on a
  human decision, or when a wake time arrives).

Runs are persisted to a run store. Paused runs resume via
`POST /api/v1/flow-runs/{run_id}/resume`; inspect them with
`GET /api/v1/flow-runs` and `GET /api/v1/flow-runs/{run_id}`. The daemon
auto-resumes `wait` nodes and timed-out `approval` nodes on its poll tick. Older
linear flows still run on the legacy per-prompt path.

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
  `./data`. It holds `personas/ skills/ flows/ runs/ api_tools/
  integration_packs/ keys.json chats/ sessions/ traces/`.

### One-click: Deploy on Render

[![Deploy to Render](https://render.com/images/deploy-to-render-button.svg)](https://render.com/deploy?repo=https://github.com/rust4ai/metalcraft-agent)

The repo ships a [`render.yaml`](render.yaml) Blueprint that stands the agent up
as an always-on web service with a **persistent disk at `/data`** (so personas,
skills, flows, chats, and keys survive redeploys). Render terminates TLS and
gives you an `https://<name>.onrender.com` URL — no reverse proxy needed.

1. Click the button above (or in Render: **New → Blueprint**, point it at this repo).
2. Render reads `render.yaml`: a web service from the public image
   (`ghcr.io/rust4ai/metalcraft-agent`) with a 1 GB disk mounted at `/data`.
3. Render prompts you for two secrets:
   - **`OPENAI_API_KEY`** — your OpenAI key.
   - **`WORKSHOP_API_KEY`** — a long random secret *you choose* (e.g.
     `openssl rand -hex 24`). **Keep it** — it's the Bearer token you'll use to
     connect the [Workshop](https://github.com/rust4ai/metalcraft-workshop) and
     call `/api/v1/*`, so you connect with a key you already have in hand.
4. Pick a **paid instance** — a persistent disk requires Starter or higher.
   Render's free tier is ephemeral and would lose the agent's state.
5. Deploy, then check `https://<name>.onrender.com/health`. Connect the Workshop
   to that URL using the `WORKSHOP_API_KEY` you set.

> **Railway** works too (its **Volumes** persist `/data` on any plan, often the
> simpler option) — deploy the same public image with a volume mounted at `/data`
> and `METALCRAFT_DATA_DIR=/data`.

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
- `src/hub_auth.rs` — Metalcraft ID bearer auth for the API
- `src/flow_exec.rs` / `src/flow_runs.rs` — v2 flow state machine + run store
- `src/metalcraft_gateway.rs` / `src/gateway_channels.rs` — gateway connect + channels
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
