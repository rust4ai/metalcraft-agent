#!/usr/bin/env bash
# First-time bring-up of the agent from the prebuilt GHCR image (no local build).
#
# On first run it creates .env from the template and asks you to fill it in;
# once the required vars are set it pulls the image and starts the daemon. For
# routine updates afterwards, use ./update-agent.sh.
#
# Usage:
#   ./start-agent.sh                                    # caddy compose (HTTPS)
#   COMPOSE_FILE=docker-compose.yml ./start-agent.sh    # no-proxy variant
#   TAG=0.8.2 ./start-agent.sh                          # pin a specific release
set -euo pipefail
cd "$(dirname "$0")"

COMPOSE_FILE="${COMPOSE_FILE:-docker-compose.caddy.yml}"
TAG="${TAG:-latest}"
export TAG

# Read a KEY=value from .env (value only, quotes/whitespace trimmed). Empty when
# unset, missing, or commented out (`# KEY=` won't match `^KEY=`).
env_val() { grep -E "^$1=" .env 2>/dev/null | tail -1 | cut -d= -f2- | tr -d '"' | xargs || true; }

# 1. Ensure .env exists.
if [ ! -f .env ]; then
  cp .env.example .env
  echo "▸ created .env from .env.example"
  echo "  Edit it and set OPENAI_API_KEY (and DOMAIN / TLS_EMAIL / WORKSHOP_API_KEY"
  echo "  for the Caddy setup), then re-run ./start-agent.sh"
  exit 1
fi

# 2. Banner — what we're about to do.
case "$COMPOSE_FILE" in
  *caddy*) MODE="Caddy · automatic HTTPS" ;;
  *)       MODE="plain HTTP, no reverse proxy" ;;
esac
OPENAI="$(env_val OPENAI_API_KEY)"
WORKSHOP="$(env_val WORKSHOP_API_KEY)"
DOMAIN="$(env_val DOMAIN)"
TLS_EMAIL="$(env_val TLS_EMAIL)"
PORT="$(env_val PORT)"; PORT="${PORT:-3002}"

echo "──────────────────────────────────────────────"
echo "  starting metalcraft-agent"
echo "  image        : ghcr.io/rust4ai/metalcraft-agent:${TAG}"
echo "  compose file : ${COMPOSE_FILE}"
echo "  mode         : ${MODE}"
echo "──────────────────────────────────────────────"

# 3. Validate required vars — collect errors (fatal) and warnings (non-fatal).
errors=()
warnings=()

if [ -z "$OPENAI" ] || [ "$OPENAI" = "sk-..." ]; then
  errors+=("OPENAI_API_KEY — required. Your OpenAI API key. This is the one thing the agent can't run without.")
fi

if [ -z "$WORKSHOP" ]; then
  warnings+=("WORKSHOP_API_KEY — unset. The HTTP API, Workshop connection, and inbound webhooks stay DISABLED (the flow scheduler still runs). Set a long random secret to enable them.")
fi

if [[ "$COMPOSE_FILE" == *caddy* ]]; then
  [ -z "$DOMAIN" ] && errors+=("DOMAIN — required for the Caddy (HTTPS) setup, e.g. agent.example.com (its A record must point at this host, ports 80/443 open). For plain HTTP instead: COMPOSE_FILE=docker-compose.yml ./start-agent.sh")
  [ -z "$TLS_EMAIL" ] && errors+=("TLS_EMAIL — required for the Caddy (HTTPS) setup. Let's Encrypt account email.")
fi

if [ ${#warnings[@]} -gt 0 ]; then
  echo "⚠ warnings:"
  for w in "${warnings[@]}"; do echo "   - $w"; done
  echo ""
fi
if [ ${#errors[@]} -gt 0 ]; then
  echo "✗ cannot start — set these in .env, then re-run:" >&2
  for e in "${errors[@]}"; do echo "   - $e" >&2; done
  exit 1
fi

# 4. Pull the prebuilt image and start.
echo "==> Pulling image…"
docker compose -f "$COMPOSE_FILE" pull
echo "==> Starting…"
docker compose -f "$COMPOSE_FILE" up -d

# 5. Report + how to verify (printed BEFORE the log tail, which blocks).
if [[ "$COMPOSE_FILE" == *caddy* ]]; then
  HEALTH="https://${DOMAIN}/health"
else
  HEALTH="http://localhost:${PORT}/health"
fi
echo ""
echo "==> Running image:"
docker compose -f "$COMPOSE_FILE" images daemon || true
echo ""
echo "==> Started. Verify health with:  curl ${HEALTH}"
echo "==> Following daemon logs (Ctrl-C to stop — the daemon keeps running):"
docker compose -f "$COMPOSE_FILE" logs --tail=20 -f daemon
