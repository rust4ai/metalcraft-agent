---
description: Install integration packs and manage the API key store by prompt using the meta tools
---

# Managing Integrations & Keys

You can configure the agent's own capabilities — install integration packs and
manage the API keys they authenticate with — using the **meta tools**. This is
the same thing the metalcraft-workshop GUI's Integrations and Key Store surfaces
do, driven by tool calls.

## Integration packs

A pack bundles personas, skills, and HTTP-API tools for one external service
(Linear, GitHub, Cloudflare, …). A pack is **installed** by *enabling* it; until
then its personas and tools don't resolve.

- `pack_list` — every installed pack with id, name, description, enabled state,
  and the env keys it requires (each flagged configured/missing).
- `pack_read` — one pack's full details by `id`: manifest, the personas/skills/
  tools/flow-templates it provides, and its **README** — the setup guide covering
  which credential to get, how to obtain it, and any provider-side steps (e.g.
  creating a bot and inviting it). Read this before enabling a pack so you can walk
  the user through exactly what it needs.
- `pack_enable` — enable (default) or disable a pack by `id`. The result echoes
  the pack's `requires_env` so you can see which keys still need setting.

## API keys / secrets

HTTP-API tools reference secrets via `$NAME` placeholders. Setting the key is
what lets an enabled pack actually authenticate.

- `key_list` — `configured` keys (name + masked preview only — never the raw
  value) plus `recommended` keys the enabled packs need, flagged configured/missing.
- `key_set` — create or overwrite a key by `name` with a raw `value`. The
  response masks the value; the raw secret is never echoed.
- `key_delete` — remove a key by `name`.

## Installing an integration end to end

When asked to "install the X integration using API key …":

1. `pack_list` to find the pack `id` (and confirm it's installed/available).
2. `pack_read` with that `id` to see its README and required keys. If the user
   hasn't supplied the credential yet, use the README to guide them through
   obtaining it and any provider-side setup (e.g. creating a bot, inviting it with
   the right permissions), step by step — then wait for them to hand you the key.
3. `pack_enable` with that `id` — note the `requires_env` keys in the result.
4. `key_set` each required key with the value the user gave you (e.g.
   `LINEAR_API_KEY`). Use the exact name from `requires_env` — `$NAME` matching
   is literal.
5. `key_list` (or re-check `pack_enable`'s `requires_env`) to confirm every
   required key now shows `configured: true`, then summarize what you enabled
   and which keys you set (masked).

## Rules

- Never echo a raw secret back to the user or into a summary — keys are masked
  for a reason. Confirm by name + masked preview only.
- Key names are literal: set exactly the name a pack's `requires_env` lists.
- Enabling a pack never fails on missing keys — you must set them separately.
