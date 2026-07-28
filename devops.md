# Deployment Guide

This guide covers deploying **metalcraft-agent** as a single self-contained daemon.

> **Architecture note (read this first if you remember the old setup).** Earlier
> versions ran two services — a public `metalcraft-agent-gateway` and a private
> daemon that talked to it over an internal network. **That external gateway was
> removed.** Everything now lives in one process: the daemon runs the flow
> scheduler *and* hosts the Workshop API, which also serves inbound gateway
> webhooks. There is no second service, no `EVENTD_*`/`AGENT_GATEWAY_*` env vars,
> and the old `--event-port/--events/--platforms` flags are deprecated no-ops.

## Architecture

```
Internet
  │
  ▼  HTTPS (via Caddy or platform TLS)
┌─────────────────────────────────────────────┐
│  metalcraft-daemon (single process)          │
│                                              │
│   Workshop API  (0.0.0.0:$PORT, default 3002)│
│     ├─ /health                  (public)     │
│     ├─ /webhook/<adapter>       (signed)     │
│     └─ /api/v1/*                (Bearer auth) │
│                                              │
│   Flow scheduler (polls every N seconds)     │
│                                              │
│   └─► OpenAI API (outbound LLM calls)        │
└─────────────────────────────────────────────┘
```

- The **Workshop API** is the only network surface. It binds `0.0.0.0:$PORT` and
  is enabled whenever `WORKSHOP_API_KEY` is set (without it, the daemon runs the
  flow scheduler only — no HTTP server).
- `/health` is unauthenticated (for platform health checks).
- `/api/v1/*` requires `Authorization: Bearer $WORKSHOP_API_KEY`.
- `/webhook/<adapter>` (e.g. `/webhook/twilio`, `/webhook/pipestreamr`) is
  unauthenticated at the router level but each adapter verifies its own
  signature on the request.
- **Gateway channels** (Twilio, pipestreamr, …) are configured at runtime via
  `/api/v1/gateway/channels`, not via env vars. Account-level secrets such as
  `TWILIO_ACCOUNT_SID` / `TWILIO_AUTH_TOKEN` are stored in the key store
  (`/api/v1/keys`), never in the deploy config.

## Environment Variables

The **only required variable is `OPENAI_API_KEY`**. Everything else has a
sensible default. Set `WORKSHOP_API_KEY` if you want the HTTP API/webhooks.

| Variable | Description | Default |
|----------|-------------|---------|
| `OPENAI_API_KEY` | OpenAI API key for LLM calls. **Required.** | — |
| `WORKSHOP_API_KEY` | Enables the Workshop API and is the Bearer token for `/api/v1/*`. If unset, no HTTP server starts. | unset (API off) |
| `PORT` / `WORKSHOP_API_PORT` | Port the Workshop API binds (`0.0.0.0`). `WORKSHOP_API_PORT` wins if both are set. | `3002` |
| `STARKBOT_MODEL` | LLM model name for the **daemon** (flow/scheduled tasks). (`OPENAI_MODEL` is the interactive CLI's model var and has no effect on the daemon.) | `gpt-5.4` |
| `STARKBOT_PERSONA` | Persona slug used for flow tasks. | `coding-agent` |
| `METALCRAFT_DEFAULT_PERSONA` | Default persona the Workshop surfaces for new chats. | `orchestrator-agent` |
| `STARKBOT_POLL_SECONDS` | Flow scheduler poll interval. | `30` |
| `STARKBOT_FLOWS_DIR` | Flows directory. | `<data dir>/flows` |
| `STARKBOT_AUTO_APPROVE` | Skip tool-approval prompts (`true`/`1`). Equivalent to `--auto-approve`. | `false` |
| `STARKBOT_ONCE` | Run one poll cycle and exit. Equivalent to `--once`. | `false` |
| `METALCRAFT_DATA_DIR` | Data dir for personas/skills/flows/chats/keys. | `~/.local/share/metalcraft-agent` |
| `RUST_LOG` | Log level. | `info` |
| `TZ` | Process timezone (affects cron-scheduled flows). | system |

CLI flags (`--persona`, `--model`, `--poll-seconds`, `--once`, `--auto-approve`,
`--api`, `--api-port`) override the matching env vars. A containerised daemon
needs **no flags** — it reads everything from the environment.

> **Pack secrets are not env vars.** Credentials for integration packs (e.g.
> `SOLARABASE_API_KEY`, `SPRITE_BUILDER_API_KEY`) live in the **key store** (`keys.json`) and
> are injected into HTTP tools via `$NAME` placeholders — set them through
> `PUT /api/v1/keys/{name}`, guided by `GET /api/v1/keys/recommended`, not the process
> environment.

## Getting Started with DigitalOcean

End-to-end walkthrough for running the agent on a **DigitalOcean Droplet**,
provisioned and managed with the [`doctl`](https://docs.digitalocean.com/reference/doctl/)
CLI. This gives you a real VM with a **persistent disk** (unlike App Platform —
see Option B) and HTTPS via Caddy.

> Prefer a fully managed, build-on-push service over a VM you maintain? Skip to
> [Option B — App Platform](#option-b--digitalocean-app-platform-doappyaml). The
> tradeoff is App Platform's ephemeral filesystem.

### 1. Install and authenticate doctl

```bash
brew install doctl                 # macOS (or see DO docs for other platforms)
doctl auth init                    # paste a personal access token from the DO dashboard
doctl account get                  # verify it works
```

### 2. Add your SSH key

```bash
# Upload a public key so you can SSH into the droplet (skip if already added):
doctl compute ssh-key import metalcraft-key --public-key-file ~/.ssh/id_ed25519.pub

# Note the fingerprint — you pass it to `droplet create`:
doctl compute ssh-key list
```

### 3. Create the droplet

Use the Marketplace **Docker on Ubuntu** image so Docker + Compose are
preinstalled:

```bash
doctl compute droplet create metalcraft-agent \
  --image docker-20-04 \
  --size s-1vcpu-1gb \
  --region nyc1 \
  --ssh-keys <your-key-fingerprint> \
  --wait
```

> `s-1vcpu-1gb` is the cheapest size that comfortably runs the daemon. If
> `docker-20-04` is unavailable in your region, list options with
> `doctl compute image list --public | grep -i docker`, or create a plain
> `ubuntu-24-04-x64` droplet and install Docker with
> `curl -fsSL https://get.docker.com | sh`.

Get the public IP:

```bash
doctl compute droplet list metalcraft-agent --format Name,PublicIPv4
```

### 4. Point your domain at the droplet

Create an `A` record for your domain (e.g. `agent.example.com`) pointing at that
IP. If your DNS is managed by DigitalOcean you can do it from the CLI:

```bash
doctl compute domain records create example.com \
  --record-type A --record-name agent --record-data <droplet-ip> --record-ttl 300
```

Caddy needs ports **80 and 443** reachable to obtain a Let's Encrypt cert. The
Marketplace Docker image leaves the firewall open by default; if you've enabled
`ufw`/cloud firewalls, allow 80 and 443.

### 5. Deploy on the droplet

```bash
ssh root@<droplet-ip>

# Get the deploy files (clone the repo, or just scp the two compose/Caddy files):
git clone https://github.com/rust4ai/metalcraft-agent.git
cd metalcraft-agent

# Create the .env Caddy + the daemon need:
cat > .env <<'ENV'
DOMAIN=agent.example.com
TLS_EMAIL=you@example.com
OPENAI_API_KEY=sk-...
WORKSHOP_API_KEY=<a long random secret>
ENV

# Pull the prebuilt image from GHCR and start (Caddy handles HTTPS):
docker compose -f docker-compose.caddy.yml up -d
```

### 6. Verify

```bash
curl https://agent.example.com/health          # from your laptop
docker compose -f docker-compose.caddy.yml logs -f daemon   # on the droplet
```

You should see `Workshop API listening on http://0.0.0.0:8080` in the logs and a
`200` from `/health`. Runtime state persists in the `daemon-data` Docker volume
and TLS certs in `caddy-data`, so both survive `docker compose` restarts and
reboots.

### Updating

```bash
ssh root@<droplet-ip>
cd metalcraft-agent
git pull
docker compose -f docker-compose.caddy.yml pull   # fetch the latest GHCR image
docker compose -f docker-compose.caddy.yml up -d   # recreate with the new image
```

### Tearing down

```bash
doctl compute droplet delete metalcraft-agent
```

## Deployment Options

### Option A — VPS / Droplet behind Caddy (recommended, persistent state)

`docker-compose.caddy.yml` runs the daemon behind Caddy with automatic HTTPS and
a persistent `/data` volume.

Prereqs: a domain with an A record → host IP, and ports 80/443 open
(Let's Encrypt validates over `:80`).

1. Create a `.env` next to the compose file (copy `.env.example`):
   ```
   DOMAIN=agent.example.com
   TLS_EMAIL=you@example.com
   OPENAI_API_KEY=sk-...
   WORKSHOP_API_KEY=<a long random secret>
   ```
2. Bring it up (pulls the prebuilt GHCR image, or `--build` to compile locally):
   ```bash
   docker compose -f docker-compose.caddy.yml up -d
   ```
3. Verify:
   ```bash
   curl https://$DOMAIN/health
   ```

The daemon is **not** published on the host — only Caddy is exposed; daemon
traffic stays on the internal compose network. Certs persist in the `caddy-data`
volume; runtime state persists in `daemon-data` (`/data`).

### Option B — DigitalOcean App Platform (`.do/app.yaml`)

Single public HTTP service, TLS handled by the platform.

```bash
doctl apps create --spec .do/app.yaml          # first deploy
doctl apps update <APP_ID> --spec .do/app.yaml # subsequent updates
```

Set `WORKSHOP_API_KEY` and `OPENAI_API_KEY` as encrypted secrets in the DO
dashboard (or via `doctl`). `deploy_on_push: true` redeploys on push to `master`.

> ⚠️ **App Platform has an ephemeral filesystem — no persistent volume.** Seeded
> personas/skills load fine (they're embedded in the binary), but anything
> created at runtime — chats, new personas/skills/flows, stored keys — is **lost
> on every deploy/restart.** For durable state, use Option A (Droplet + volume),
> or back the daemon with DO Spaces / a managed DB.

### Option C — Plain Docker

```bash
docker build -t metalcraft-agent .
docker run -d --name metalcraft-agent \
  -p 3002:3002 \
  -e OPENAI_API_KEY=sk-... \
  -e WORKSHOP_API_KEY=<secret> \
  -v metalcraft-data:/data -e METALCRAFT_DATA_DIR=/data \
  metalcraft-agent
```

The image's default command is `metalcraft-daemon --auto-approve`. The Workshop
API binds `$PORT` (default 3002).

### Option D — Render (`render.yaml`)

The repo ships a Render Blueprint (`render.yaml`): a web service built from the GHCR image
with a **persistent 1 GB disk mounted at `/data`**, `healthCheckPath: /health`, and the
`starter` plan. Render injects `PORT`, which the daemon honors. Create the service from the
blueprint, set `OPENAI_API_KEY` and `WORKSHOP_API_KEY` as environment variables in the Render
dashboard, and you get managed TLS at `https://<name>.onrender.com`. State survives restarts
via the mounted disk.

### Option E — Railway (`railway.toml`)

`railway.toml` configures a Dockerfile-built Railway deploy (`builder = "dockerfile"`,
`restartPolicyType = "on_failure"`). It sets **no start command or health-check path** — it
relies on the image's `CMD` (`metalcraft-daemon --auto-approve`) and Railway's injected `PORT`
(honored via `WORKSHOP_API_PORT`/`PORT`). Set `OPENAI_API_KEY` and `WORKSHOP_API_KEY` as
Railway variables. Note that Railway's default filesystem is ephemeral — attach a **volume**
at your `METALCRAFT_DATA_DIR` for durable state, the same caveat as App Platform (Option B).

## Configuring Gateway Channels (Twilio, pipestreamr, …)

Channels are created at runtime over the API — not in the deploy config:

1. Store account secrets in the key store:
   ```bash
   curl -X PUT https://$DOMAIN/api/v1/keys/TWILIO_ACCOUNT_SID \
     -H "Authorization: Bearer $WORKSHOP_API_KEY" \
     -H 'Content-Type: application/json' -d '{"value":"AC..."}'
   curl -X PUT https://$DOMAIN/api/v1/keys/TWILIO_AUTH_TOKEN \
     -H "Authorization: Bearer $WORKSHOP_API_KEY" \
     -H 'Content-Type: application/json' -d '{"value":"..."}'
   ```
2. Create a channel (see `/api/v1/gateway/types` for available adapters):
   ```bash
   curl -X POST https://$DOMAIN/api/v1/gateway/channels \
     -H "Authorization: Bearer $WORKSHOP_API_KEY" \
     -H 'Content-Type: application/json' \
     -d '{"type_id":"twilio","name":"Support line","settings":{...}}'
   ```
3. Point the provider's webhook at `https://$DOMAIN/webhook/<adapter>`
   (e.g. `/webhook/twilio`). Each adapter verifies the request signature.

## Scheduled Flows (e.g. Daily Commit Summary)

Flows run inside the daemon's poll loop (every `STARKBOT_POLL_SECONDS`). Flow
templates ship in the seed data (e.g.
`seed/integration_packs/discord/flow_templates/daily-commit-summary.json`).

1. Copy a template into your flows dir (`<data dir>/flows/` or `STARKBOT_FLOWS_DIR`).
2. Fill in the repo/channel placeholders and set `"enabled": true`.
3. The daemon picks it up on the next poll cycle.

Per-flow persona is resolved from the flow's entry node (`"data": { "persona": ... }`),
independently of `STARKBOT_PERSONA` — one daemon can run flows under different
personas. For wall-clock scheduling use a cron entry node, e.g.
`"schedule_type": "cron", "cron": "0 0 0 * * *"` (daily at 00:00). Cron uses the
process timezone — set `TZ=UTC` for UTC.

## Verify

```bash
# Health (unauthenticated)
curl https://$DOMAIN/health

# Authenticated API
curl https://$DOMAIN/api/v1/snapshot -H "Authorization: Bearer $WORKSHOP_API_KEY"

# Gateway channel types
curl https://$DOMAIN/api/v1/gateway/types -H "Authorization: Bearer $WORKSHOP_API_KEY"
```

Logs to look for on startup:
- `Workshop API listening on http://0.0.0.0:<port>`

## Troubleshooting

**No HTTP server / connection refused** — `WORKSHOP_API_KEY` is unset, so the API
is disabled and only the flow scheduler runs. Set it to enable the server.

**`/api/v1/*` returns 401** — missing or wrong `Authorization: Bearer
$WORKSHOP_API_KEY` header.

**State resets on every deploy** — you're on an ephemeral filesystem (DO App
Platform). Mount a persistent volume at `/data` and set `METALCRAFT_DATA_DIR=/data`,
or move to a Droplet (Option A).

**Webhook rejected** — the adapter's signature check failed. Confirm the
provider's signing secret / account credentials are in the key store and the
webhook URL matches `/webhook/<adapter>`.

**`OPENAI_API_KEY` missing** — the daemon fails to start LLM calls. It is the one
hard requirement.
