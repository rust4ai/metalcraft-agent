# Railway Deployment Guide

This guide covers deploying **metalcraft-agent-gateway** and **metalcraft-agent** (flowd) as two Railway services that communicate over Railway's private network.

## Architecture

```
Internet
  │
  ▼
┌──────────────────────────────────────┐
│  Railway Project                     │
│                                      │
│  ┌──────────────┐   private net    ┌────────────────┐
│  │   gateway     │◄───────────────►│     flowd       │
│  │   (public)    │                 │   (private)     │
│  │   port 3000   │   events ──────►│   port 3001     │
│  └──────┬───────┘                 └────────────────┘
│         │                                │
│         │ Discord websocket              │ OpenAI API
│         │ Slack/GitHub webhooks           │ (outbound)
│         ▼                                ▼
└──────────────────────────────────────┘
```

- **gateway** is the only public-facing service. It receives Discord events via websocket, Slack/GitHub webhooks via HTTP, and serves the outbound messaging API.
- **flowd** is private. It receives normalized events from the gateway over the internal network, runs LLM-powered agent tasks, and calls back to the gateway to send messages.

## Step 1: Create a Railway Project

1. Go to [railway.app](https://railway.app) and create a new project.
2. You'll add two services to this project.

## Step 2: Deploy the Gateway

1. Click **"New Service"** → **"GitHub Repo"** → select `metalcraft-agent-gateway`.
2. Railway will detect the `Dockerfile` and `railway.toml` automatically.
3. Add a **volume** mounted at `/data` (for SQLite persistence).
4. Set environment variables (Settings → Variables):

| Variable | Value | Required |
|----------|-------|----------|
| `PORT` | `3000` | Yes |
| `AGENT_GATEWAY_API_KEY` | (generate a strong random string) | Yes |
| `DISCORD_BOT_TOKEN` | (from Discord Developer Portal) | Yes (for Discord) |
| `GATEWAY_DB_PATH` | `/data/gateway.db` | Yes |
| `RUST_LOG` | `info` | No |
| `SLACK_BOT_TOKEN` | (from Slack app settings) | No |
| `SLACK_SIGNING_SECRET` | (from Slack app settings) | No |
| `GITHUB_WEBHOOK_SECRET` | (your chosen secret) | No |

5. In Settings → Networking, **generate a public domain** (e.g. `gateway-xxx.up.railway.app`). This is needed for Slack/GitHub webhooks.
6. Note the **internal hostname** shown in the Networking tab (e.g. `gateway.railway.internal`). You'll need this for the flowd service.
7. Deploy.

## Step 3: Deploy the Agent (flowd)

1. Click **"New Service"** → **"GitHub Repo"** → select `metalcraft-agent`.
2. Railway will detect the `Dockerfile` and `railway.toml`.
3. Set environment variables:

| Variable | Value | Required |
|----------|-------|----------|
| `OPENAI_API_KEY` | (your OpenAI API key) | Yes |
| `AGENT_GATEWAY_URL` | `http://gateway.railway.internal:3000` | Yes |
| `AGENT_GATEWAY_API_KEY` | (same value as gateway) | Yes |
| `EVENTD_WEBHOOK_SECRET` | (generate a strong random string) | Yes |
| `EVENTD_ADMIN_USER_IDS` | (your Discord user ID) | Yes |
| `EVENTD_HOST` | (flowd's internal hostname, e.g. `flowd.railway.internal`) | Yes |
| `EVENTD_PORT` | `3001` | Yes |
| `METALCRAFT_DATA_DIR` | `/data` | Recommended |
| `RUST_LOG` | `info` | No |

4. Override the **start command** in Settings → Deploy:
   ```
   metalcraft-flowd --persona discord-agent --auto-approve --event-port 3001 --events message_create --platforms discord
   ```
5. In Settings → Networking, do **NOT** generate a public domain. This service should only be reachable via Railway's private network. Just ensure the internal port `3001` is set.
6. Optionally add a **volume** at `/data` for persistent personas/skills/flows.
7. Deploy.

## Step 4: Get Your Discord User ID

The `EVENTD_ADMIN_USER_IDS` variable controls who can trigger the agent via Discord messages. To find your Discord user ID:

1. Open Discord → Settings → Advanced → Enable **Developer Mode**.
2. Right-click your username → **Copy User ID**.
3. Set the env var: `EVENTD_ADMIN_USER_IDS=123456789012345678`
4. For multiple admins: `EVENTD_ADMIN_USER_IDS=123456789,987654321`

## Step 5: Configure External Webhooks (Optional)

### Slack

1. Go to [api.slack.com/apps](https://api.slack.com/apps) → your app → **Event Subscriptions**.
2. Set the Request URL to: `https://gateway-xxx.up.railway.app/api/v1/webhooks/slack`
3. Slack will send a challenge request — the gateway handles this automatically.
4. Subscribe to events: `message.channels`, `app_mention`, etc.
5. Set `SLACK_SIGNING_SECRET` on the gateway to the value from **Basic Information → Signing Secret**.

### GitHub

1. Go to your repo → **Settings → Webhooks → Add webhook**.
2. Set Payload URL to: `https://gateway-xxx.up.railway.app/api/v1/webhooks/github`
3. Content type: `application/json`
4. Set a secret and use the same value for `GITHUB_WEBHOOK_SECRET` on the gateway.
5. Select events: Pushes, Pull requests, Issue comments, etc.

## Step 6: Verify

### Check gateway is running
```bash
curl https://gateway-xxx.up.railway.app/api/v1/subscribers \
  -H "Authorization: Bearer YOUR_API_KEY"
```
Should return `[]` (empty list) or a list of subscribers.

### Check flowd registered itself
After flowd starts, repeat the above — you should see a subscriber entry with `url` pointing to flowd's internal address.

### Test Discord
Send a message in a Discord channel where the bot is present. If you are an admin user, the agent should respond.

### Check logs
Railway → Service → **Logs** tab. Look for:
- Gateway: `Discord listener connected as BotName`
- flowd: `Registered as gateway subscriber (id: ..., url: http://flowd.railway.internal:3001/webhook/events)`
- flowd: `Processing message_create event from username in channel 123456`

## Environment Variables Reference

### Gateway (`metalcraft-agent-gateway`)

| Variable | Description | Default |
|----------|-------------|---------|
| `PORT` | HTTP server port | `3000` |
| `AGENT_GATEWAY_API_KEY` | Bearer token for API auth (required) | — |
| `DISCORD_BOT_TOKEN` | Discord bot token (enables Discord) | — |
| `SLACK_BOT_TOKEN` | Slack bot token (enables Slack) | — |
| `SLACK_SIGNING_SECRET` | HMAC secret for Slack webhook verification | — |
| `GITHUB_WEBHOOK_SECRET` | HMAC secret for GitHub webhook verification | — |
| `GATEWAY_DB_PATH` | SQLite database file path | `./gateway.db` |
| `RUST_LOG` | Log level | `info` |

### Agent/flowd (`metalcraft-agent`)

| Variable | Description | Default |
|----------|-------------|---------|
| `OPENAI_API_KEY` | OpenAI API key for LLM calls | — (required) |
| `AGENT_GATEWAY_URL` | Gateway base URL (enables event listener) | — |
| `AGENT_GATEWAY_API_KEY` | Gateway auth token | — |
| `EVENTD_WEBHOOK_SECRET` | Secret for inbound webhook auth (required with gateway) | — |
| `EVENTD_ADMIN_USER_IDS` | Comma-separated admin user IDs (required with gateway) | — |
| `EVENTD_HOST` | Hostname for gateway callback URL | `localhost` |
| `EVENTD_PORT` | Event listener port | `3001` |
| `METALCRAFT_DATA_DIR` | Data directory for personas/skills/flows | `~/.local/share/metalcraft-agent` |
| `RUST_LOG` | Log level | `info` |

## Scheduled Flows (e.g. Daily Commit Summary)

Flows run alongside the event listener in the same flowd process. To set up the daily commit summary flow:

1. SSH into the flowd service or use a Railway volume.
2. Copy and edit the flow template:
   ```
   /data/flows/daily-commit-summary.json
   ```
3. Replace `OWNER/REPO` with the actual GitHub repo (e.g. `rust4ai/metalcraft-agent`).
4. Replace `CHANNEL_ID` with the Discord channel ID to post in.
5. Set `"enabled": true`.
6. Restart flowd (or wait for the next poll cycle).

The flow uses the `discord-reporter-agent` persona. To use it, set the start command to:
```
metalcraft-flowd --persona discord-reporter-agent --auto-approve --event-port 3001 --events message_create --platforms discord
```

Or use `--event-persona discord-reporter-agent` to use a different persona for events vs flows.

## Troubleshooting

**flowd can't reach gateway**: Check that `AGENT_GATEWAY_URL` uses the Railway internal hostname (e.g. `http://gateway.railway.internal:3000`), not the public URL.

**Gateway can't reach flowd**: Check that `EVENTD_HOST` matches flowd's Railway internal hostname and that port 3001 is exposed internally.

**Agent doesn't respond to messages**: Check `EVENTD_ADMIN_USER_IDS` — only messages from listed user IDs are processed. Check the flowd logs for "Ignoring event from non-admin user".

**"Missing required env var" on startup**: flowd validates `EVENTD_WEBHOOK_SECRET`, `EVENTD_ADMIN_USER_IDS`, and `AGENT_GATEWAY_API_KEY` at boot. All three are required when `AGENT_GATEWAY_URL` is set.

**SQLite errors on gateway**: Ensure a Railway volume is mounted at `/data` and `GATEWAY_DB_PATH` is set to `/data/gateway.db`. Without a volume, the database resets on every deploy.
