#!/usr/bin/env bash
# Update the running daemon to a newer prebuilt image from GHCR.
#
# Pulls the image CI already built (no compiling on the host) and recreates
# the container. State in the daemon-data / caddy-data volumes is preserved.
#
# Usage:
#   ./update.sh                 # update to :latest using the caddy compose file
#   TAG=0.6.0 ./update.sh       # pin to a specific release tag
#   COMPOSE_FILE=docker-compose.yml ./update.sh   # use the no-proxy compose file
#
# Roll back the same way:  TAG=0.5.2 ./update.sh
set -euo pipefail
cd "$(dirname "$0")"

COMPOSE_FILE="${COMPOSE_FILE:-docker-compose.caddy.yml}"
TAG="${TAG:-latest}"
export TAG

echo "==> Updating daemon to ghcr.io/rust4ai/metalcraft-agent:${TAG} via ${COMPOSE_FILE}"

docker compose -f "$COMPOSE_FILE" pull
docker compose -f "$COMPOSE_FILE" up -d

echo "==> Done. Running image:"
docker compose -f "$COMPOSE_FILE" images daemon || true

echo "==> Recent daemon logs (Ctrl-C to stop following):"
docker compose -f "$COMPOSE_FILE" logs --tail=20 -f daemon
