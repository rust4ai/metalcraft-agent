#!/usr/bin/env bash
# First-time bring-up of the agent from the prebuilt GHCR image (no local build).
#
# On first run it creates .env from the template and asks you to fill it in;
# once OPENAI_API_KEY is set it pulls the image and starts the daemon. For
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

# 1. Ensure .env exists and is filled in.
if [ ! -f .env ]; then
  cp .env.example .env
  echo "▸ created .env from .env.example"
  echo "  Edit it and set OPENAI_API_KEY (and DOMAIN / TLS_EMAIL / WORKSHOP_API_KEY"
  echo "  for the Caddy setup), then re-run ./start-agent.sh"
  exit 1
fi

# Guard against starting with the placeholder/empty key.
if grep -qE '^OPENAI_API_KEY=(sk-\.\.\.)?[[:space:]]*$' .env; then
  echo "✗ OPENAI_API_KEY is not set in .env — edit it, then re-run." >&2
  exit 1
fi

# 2. Pull the prebuilt image and start.
echo "==> Starting agent from ghcr.io/rust4ai/metalcraft-agent:${TAG} via ${COMPOSE_FILE}"
docker compose -f "$COMPOSE_FILE" pull
docker compose -f "$COMPOSE_FILE" up -d

# 3. Report what's running, then follow the logs.
echo "==> Running image:"
docker compose -f "$COMPOSE_FILE" images daemon || true

echo "==> Recent daemon logs (Ctrl-C to stop following):"
docker compose -f "$COMPOSE_FILE" logs --tail=20 -f daemon
