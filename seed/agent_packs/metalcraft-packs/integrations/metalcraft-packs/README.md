# Metalcraft Packs (discovery)

Gives the agent read-only discovery over the Metalcraft Packs registry
(`https://packs.metalcraftai.com`): search/browse the catalog, read pack details
and required keys, list featured packs, and check a pack's latest version.

## Setup
**No keys required.** The registry's discovery endpoints are public, so this pack
works as soon as it's enabled.

## Installing other packs
This pack only *finds* packs. Installing one onto the agent is done from the
**"My Agent"** page at `https://packs.metalcraftai.com` (which calls the agent's
`POST /api/v1/integration-packs/install`). After installing, set any keys the pack
lists in `requires_env`.
