# Metalcraft Packs skill

Discover integrations in the **Metalcraft Packs** registry
(`https://packs.metalcraftai.com/api/v1`). These tools are **read-only** and hit
the registry's **public** API — no key or `METALCRAFT_TOKEN` is required.

## What a pack is
An integration bundles personas, skills, HTTP-API tools, and flow templates
that extend an agent as a unit (e.g. `github`, `linear`, `metalcraft-email`). Each
pack has a **slug** (its id), a **version**, and a **requires_env** list of the keys
it needs to actually work once installed.

## Tools
- **`mpack_search`** — browse/search the catalog. Optional `q` (free text), `tag`,
  `sort` (`installs` | `name` | `new`), `limit`. Returns pack summaries with counts,
  `install_count`, and `verified`/`featured` flags.
- **`mpack_featured`** — the editorially highlighted packs.
- **`mpack_get`** — one pack's full detail by `slug`, including its `readme` (setup
  guide) and `requires_env`.
- **`mpack_version`** — just `{slug, version}`, a cheap "is there a newer version?"
  check against what's installed.

## Workflow
1. **Find** a pack with `mpack_search` (or `mpack_featured`). Resolve the exact
   `slug` before going further — don't guess.
2. **Inspect** it with `mpack_get`: summarize what it does, its `tool`/`persona`/
   `skill` counts, whether it's `verified`, and — importantly — the **keys in
   `requires_env`** the user will need.
3. **Installing** a pack onto this agent is a separate, deliberate step (it runs the
   pack's tools later with the user's keys). Direct the user to the **"My Agent"**
   page at `https://packs.metalcraftai.com` to install and enable it, then help them
   set the required keys. (You can list what's already installed here via the agent's
   own integration-packs list.)
4. **Updates:** compare `mpack_version` to the installed version to flag when a newer
   release is available.

Never invent slugs, versions, or `requires_env` — always read them from the tools.
