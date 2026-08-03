# Metalcraft Gateway — one-click "connect" for the agent (zero copy-paste)

Add **Metalcraft Gateway** as a second messaging option (alongside PipeStreamr) that a
user connects with **no config paste** — no `base_url`, no `integration_id`, no
`webhook_secret`. Because the pod, the gateway service, and the hub all share
**Metalcraft ID** auth, the agent *fetches* its config instead of the user copying it.

## Why this is possible today

- Every pod already has a **write-scoped `METALCRAFT_TOKEN`** (`mck_…` PAT) injected by
  the k3s control plane (`env_secret` in metalcraft-k3-cluster; hub mints it `["write"]`
  by default). That token *is* the user's Metalcraft ID credential.
- The gateway (`gateway.metalcraftai.com`) already authenticates `mck_…` PATs and
  exposes everything the agent needs:
  - `GET /api/v1/phone` → `{ integration_id, signing_secret, active_number,
    consumer_webhook_url, verified }` for the token's user.
  - `PATCH /api/v1/integrations/{id}` → set `consumer_webhook_url`.
  - `POST /api/v1/messages/send` → PipeStreamr-wire-compatible outbound.
- The agent is **already wire-compatible**: the `pipestreamr` adapter + the
  `/webhook/pipestreamr` inbound handler speak exactly the gateway's protocol. The
  gateway signs inbound with the integration's `signing_secret`, which is precisely
  `PIPESTREAMR_WEBHOOK_SECRET`.

So "connecting" = **sync three values the agent can fetch itself**, not paste.

## The connect flow (one button)

```
workshop "Connect Metalcraft Gateway"
  → agent: POST /api/v1/gateway/metalcraft/connect   (workshop_api)
      token       = METALCRAFT_TOKEN
      gateway_url = key "METALCRAFT_GATEWAY_URL" or default https://gateway.metalcraftai.com
      webhook_url = {POD_PUBLIC_URL}/webhook/pipestreamr
      1. POST {gateway}/api/v1/agent/connect { webhook_url }   (Bearer token)
         → { base_url, integration_id, signing_secret, active_number, channel }
         (409 "verify first" if the user has no verified number → surfaced to the UI)
      2. key_store.put PIPESTREAMR_BASE_URL   = base_url
         key_store.put PIPESTREAMR_API_KEY    = METALCRAFT_TOKEN
         key_store.put PIPESTREAMR_WEBHOOK_SECRET = signing_secret
      3. gateway_channels: create/update a `metalcraft-gateway` instance
         settings { integration_id, from: active_number }, enabled=true
  → UI shows "Connected as {active_number}"
```

Outbound and inbound then work with the **existing** `pipestreamr` adapter +
`/webhook/pipestreamr` handler — nothing new on the message path.

## Changes per repo

### metalcraft-gateway (small)
- Add `POST /api/v1/agent/connect { webhook_url }` (Bearer PAT): `ensure_active_integration`
  for the caller, set `consumer_webhook_url`, return `{ base_url, integration_id,
  signing_secret, active_number, channel }`. Returns `409` with the register/verify hint
  if the user isn't verified yet. (Everything it needs already exists; this is a
  convenience wrapper over `/phone` + the integrations PATCH so the agent does one call.)

### metalcraft-agent (core)
- New seed channel type `seed/gateway_channels/metalcraft-gateway/channel_type.json`
  (`adapter: "pipestreamr"`, `requires_env: []`, a `"provisioner": "metalcraft-gateway"`
  hint, settings = only `persona` + `model`; `integration_id`/`from` are auto-filled).
- New module `metalcraft_gateway.rs`: `connect(webhook_base) -> ConnectedConfig` (the
  flow above; idempotent — re-run to re-sync after a number change / secret rotation)
  and `status()` (calls `GET {gateway}/api/v1/phone`, reports connected/verified state).
- New `workshop_api` routes: `POST /api/v1/gateway/metalcraft/connect`,
  `GET /api/v1/gateway/metalcraft/status`. `webhook_base` comes from `POD_PUBLIC_URL`
  (see below) or the request body (workshop passes it).
- Reuse `pipestreamr` adapter + `/webhook/pipestreamr` unchanged.

### metalcraft-k3-cluster (one line, recommended)
- Inject `POD_PUBLIC_URL` (the pod's own external URL) into `env_secret` so the agent
  knows its webhook base with zero inputs. Fallback if we skip this: the workshop (which
  already addresses the pod) passes `webhook_base` in the connect call.

### metalcraft-workshop (UI)
- Render the `metalcraft-gateway` type as a **Connect panel** (not a settings form):
  status → "Connect" button → "Connected as {number}", plus persona/model pickers.
- Surface the **one human step**: if `status()` says not-verified, guide the user to
  register + verify their number (Phase 1: link to `gateway.metalcraftai.com`; Phase 2:
  inline register → show code → poll until verified).

## The only human step

Registering + **verifying** a personal number (texting a 6-digit code once) can't be
automated — it's the ownership proof. Everything else (token, URLs, secret, integration
id, webhook wiring) is fetched/synced. After verification, Connect is one click.

## Prerequisites / notes
- Pod token must be `write`-scoped (it is, by default).
- The user must be **premium** (gateway register is premium-gated) — pods are already
  premium-gated, so this aligns.
- `POD_PUBLIC_URL` must be HTTPS and reachable by the gateway (it POSTs inbound there).

## Status — BUILT (2026-08-03)

Phases 1 **and** 2 implemented across all four repos (compiles/checks clean; not yet
run end-to-end against live infra):
- **gateway**: `POST /api/v1/agent/connect` (committed + pushed; e2e 38/38).
- **agent** (v0.10.0): seed `metalcraft-gateway` type, `metalcraft_gateway.rs`
  (`connect`/`status`/`register`), 3 workshop_api routes, `provisioner` on `ChannelType`.
- **k3**: `env_secret` injects `POD_PUBLIC_URL = https://{slug}.{BASE_DOMAIN}` (8/8 tests).
- **workshop** (tauri 0.5.0 / api 0.3.0): Connection trait + Remote/Local impls, 3 Tauri
  commands, `provisioner` wire type, and the **inline** Connect panel in `GatewayView.tsx`
  (status → register → show code → poll-verify → Connect). tsc clean.

**Phase 3 — BUILT.** Self-heal + status chip:
- **agent** (v0.10.1): `metalcraft_gateway::resync()` (idempotent re-sync — full `connect`
  when `POD_PUBLIC_URL` is known, else refresh secret + `integration_id`/`from` from
  `GET /phone`), a periodic `heal_loop()` (spawned in `daemon.rs`, every
  `METALCRAFT_GATEWAY_HEAL_SECS`, default 600s), and `maybe_reactive_resync()` fired from
  the pipestreamr webhook's signature-reject branch (rate-limited 1/30s) so a rotated
  secret self-heals on the next inbound.
- **workshop** (tauri v0.5.1): a persistent `MgStatusChip` on the connected channel's row
  (connected / register / verify / connect / attention), fed by `gateway_metalcraft_status`.

First real run still needs a premium account + a pod + the gateway deployed with
`POD_PUBLIC_URL` reachable. Nothing left unbuilt in this plan.
