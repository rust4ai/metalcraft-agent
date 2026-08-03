# Scoped Keys + Channel-Scoped Gateway Secrets

## Problem

Connecting the **Metalcraft Gateway** (one-click WhatsApp/SMS) writes three
`PIPESTREAMR_*` secrets straight into the flat, global `keys.json`:

```rust
// metalcraft_gateway::connect()
store.upsert("PIPESTREAMR_BASE_URL", &cfg.base_url);
store.upsert("PIPESTREAMR_API_KEY", &tok);           // = the pod's METALCRAFT_TOKEN
store.upsert("PIPESTREAMR_WEBHOOK_SECRET", &cfg.signing_secret);
```

Symptoms:

- **Leaky, oddly-named keys** show up in the workshop Keys tab even though the
  user only ever clicked "Connect" on the Metalcraft Gateway — they never chose
  anything called "PipeStreamr".
- **Foot-gun**: those keys look user-editable/deletable, but hand-editing them
  silently breaks the gateway.
- **Singleton limitation**: the pipestreamr adapter reads a *single global*
  `PIPESTREAMR_API_KEY` / `PIPESTREAMR_WEBHOOK_SECRET`, so only one gateway
  channel can ever work — a second would collide on the same globals.

Root cause: transport credentials live in the global key namespace, keyed by a
transport-specific name, instead of belonging to the channel instance that owns
them.

## Design: keys become *scoped*

A key gains a **scope**. Scope is a first-class enum so more scopes can slot in
later (persona, pack):

```rust
pub enum KeyScope { Global, Channel(String /* channel id */) }
```

Two payoffs:

- **Names get clean.** Because scope disambiguates, a channel's secrets are just
  `API_KEY`, `WEBHOOK_SECRET`, `BASE_URL` — no `PIPESTREAMR_` prefix. The ugly
  names disappear by *scoping*, not renaming.
- **Visible but safe.** Connect-managed secrets stay visible in the Keys UI,
  rendered locked with a "managed by the Metalcraft Gateway connection" badge
  (read-only + reveal; re-sync/reconnect is how they change). `managed` is
  *derived*, not stored: a channel's secrets are managed iff its type declares a
  `provisioner`; a global key is managed iff it is env-authoritative
  (`METALCRAFT_TOKEN`).

### Storage — re-schema `keys.json` (v2)

Single versioned store, scopes native on disk:

```json
{
  "version": 2,
  "global":   { "OPENAI_API_KEY": "sk-…", "TWILIO_AUTH_TOKEN": "…" },
  "channels": {
    "<channel-id>": { "WEBHOOK_SECRET": "whsec_…", "BASE_URL": "https://…" }
  }
}
```

Legacy load: a file with no numeric `version` field is the old flat map — every
top-level entry migrates into `global` (managed=false). Upgrade is persisted on
the next save (and forced once at boot).

### Resolution

- `lookup(name)` — **unchanged**: global scope, then process env. All existing
  callers (twilio, spaces, imap, `$VAR` expansion in http_api, meta tools) keep
  working untouched.
- `lookup_scoped(channel: Option<&str>, name)` — channel scope, then global,
  then env. Used by adapters that run in a channel context.
- `ENV_AUTHORITATIVE` (`METALCRAFT_TOKEN`) precedence rule preserved for global.

### Gateway credentials, resolved per channel

`PipeCfg { api_key, base_url }` built by `PipeCfg::for_channel(&ChannelInstance)`:

- `api_key`: for a `metalcraft-gateway` provisioner, **derived from
  `METALCRAFT_TOKEN`** at call time — never stored, never drifts. For a manual
  pipestreamr channel, `lookup_scoped(id, "API_KEY")` (legacy fallback:
  global `PIPESTREAMR_API_KEY` for one release).
- `base_url`: `lookup_scoped(id, "BASE_URL")` else the default.
- `webhook_secret` (inbound only): `lookup_scoped(id, "WEBHOOK_SECRET")`
  (legacy fallback: global `PIPESTREAMR_WEBHOOK_SECRET`).

## Phases

### Phase 1 — scoped storage + gateway rewire (fixes the leak)

1. **`key_store.rs`** — v2 schema (`version`/`global`/`channels`); legacy
   flat→v2 migration on load. Existing methods (`get`/`upsert`/`delete`/
   `contains`/`list_masked`) keep operating on **global** so current callers are
   untouched. Add `KeyScope`, channel methods (`get_channel`/`upsert_channel`/
   `delete_channel_key`/`delete_channel`/`channel_secret_names`), and
   `lookup_scoped`.
2. **`tools/pipestreamr.rs`** — `PipeCfg` + `PipeCfg::for_channel`; `send` takes
   `&PipeCfg` instead of reading globals.
3. **`tools/gateway.rs`** — dispatcher resolves the channel (by `from` →
   `integration_id`/`from`, else the single enabled instance for the adapter;
   0/>1 → helpful error), builds `PipeCfg`, calls `send`. Unlocks multi-channel.
4. **`workshop_api.rs::handle_pipestreamr_webhook`** — reorder: parse → resolve
   channel by `source_id` → verify signature with **that channel's**
   `WEBHOOK_SECRET` (fail-closed; keep `GATEWAY_ALLOW_UNSIGNED`) → dispatch.
5. **`metalcraft_gateway.rs`** — `connect` writes channel-scoped `BASE_URL` +
   `WEBHOOK_SECRET` (no api_key copy) and deletes legacy globals; `status` /
   `resync` / `maybe_reactive_resync` read/write channel scope.
6. **`gateway_channels.rs::delete_instance`** — cascade-delete the channel's
   secret scope (ties off the workshop delete-channel button).
7. **Boot migration** (`daemon.rs`, start of `run()`): move any global
   `PIPESTREAMR_*` into the `metalcraft-gateway` channel scope (stripping the
   prefix) and delete the globals; force the v2 format upgrade. Idempotent.
8. **Tests**: `PipeCfg` resolution (provisioner derives token / manual stored /
   legacy fallback), legacy flat→v2 migration idempotency, per-channel inbound
   verification + routing.

After Phase 1 the Keys tab is already clean — nothing gateway-related lives in
global anymore.

### Phase 2 — the scoped Keys UI

`list_keys` returns `KeyEntry { name, masked, scope, channel_name, managed }`;
`set_key`/`delete_key` take a scope. `KeysView` renders grouped sections: global
keys, then one group per channel (headed by channel name + type badge) with
managed secrets locked + reveal. Recommended-keys becomes scope-aware.

### Phase 3 — generalize

Manual `pipestreamr` type gets add-secret-in-scope (drop `requires_env`); Twilio
account secrets can adopt Channel scope too. Drop the Phase-1 legacy fallback
once installs have migrated.

## Decisions (agreed)

1. Re-schema `keys.json` into one versioned scoped store (not two files).
2. Managed channel secrets: visible + masked + reveal, **read-only**.
3. `KeyScope` ships `Global` + `Channel`, enum-extensible.
4. `metalcraft-gateway` `API_KEY` is a synthetic read-only row derived from the
   Metalcraft ID token — not stored.
