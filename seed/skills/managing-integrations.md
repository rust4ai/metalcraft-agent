---
description: Inspect integrations, install agent packs, and manage the API key store by prompt using the meta tools
---

# Managing Integrations & Keys

You can configure the agent's own capabilities — install agent packs and manage
the API keys they authenticate with — using the **meta tools**. This is the same
thing the metalcraft-workshop GUI's Integrations and Key Store surfaces do,
driven by tool calls.

## The two units

An **integration** bundles the HTTP-API tools for one external service
(Linear, GitHub, Cloudflare, …). It is not installed on its own.

An **agent pack** is the install unit. It carries an agent preset, its personas
and skills, and vendors every integration those personas need. Installing
one makes all of it resolvable at once; uninstalling removes it again.

So "install the Linear integration" means: find the agent pack that provides it.

## Reading what's here

- `integration_list` — every installed integration with id, name, description,
  version, and the env keys it requires (each flagged configured/missing).
- `integration_read` — one pack's full details by `id`: manifest, the personas/skills/
  tools/flow-templates it provides, and its **README** — the setup guide covering
  which credential to get, how to obtain it, and any provider-side steps (e.g.
  creating a bot and inviting it). Read this so you can walk the user through
  exactly what the pack needs.
- `agentpack_list` / `agentpack_read` — the installed agent packs, what each
  provides, and the consent summary (domains it reaches, which tools mutate).

## Changing what's here

- `agentpack_install` — install an agent pack from a `.agentpack` file or URL.
  Show the user its consent summary first: the domains it will reach and the
  environment keys it will want. This is a real grant, not a formality.
- `agentpack_uninstall` — remove one, along with the integrations only it
  was using.
- `agentpack_export` — package this pod's own preset as a distributable
  `.agentpack`.

## API keys / secrets

HTTP-API tools reference secrets via `$NAME` placeholders. Setting the key is
what lets an installed pack actually authenticate.

- `key_list` — `configured` keys (name + masked preview only — never the raw
  value) plus `recommended` keys the installed packs need, flagged
  configured/missing.
- `key_set` — create or overwrite a key by `name` with a raw `value`. The
  response masks the value; the raw secret is never echoed.
- `key_delete` — remove a key by `name`.

## Setting up an integration end to end

When asked to "set up the X integration using API key …":

1. `integration_list` to see whether X's integration is already here. If it isn't,
   the user needs to install an agent pack that provides it (`agentpack_install`)
   — ask them for the source rather than guessing one.
2. `integration_read` with that `id` to see its README and required keys. If the user
   hasn't supplied the credential yet, use the README to guide them through
   obtaining it and any provider-side setup (e.g. creating a bot, inviting it
   with the right permissions), step by step — then wait for them to hand you
   the key.
3. `key_set` each required key with the value the user gave you (e.g.
   `LINEAR_API_KEY`). Use the exact name from `requires_env` — `$NAME` matching
   is literal.
4. `key_list` (or re-read the pack) to confirm every required key now shows
   `configured: true`, then summarize what is now working and which keys you set
   (masked).

## Rules

- Never echo a raw secret back to the user or into a summary — keys are masked
  for a reason. Confirm by name + masked preview only.
- Key names are literal: set exactly the name a pack's `requires_env` lists.
- Installing an agent pack never fails on missing keys — you must set them
  separately.
- There is no enable/disable. A pack is present because some installed agent
  pack provides it. If a capability should go away, uninstall that agent pack.
