---
description: Schema and conventions for authoring metalcraft personas
---

# Authoring Personas

A persona is a JSON file (`personas/<slug>.json`) describing one agent. Create
or edit with `persona_write` (pass `slug` + a `persona` object); read with
`persona_read`; list with `persona_list`.

## Schema

```json
{
  "name": "Workshop",
  "description": "One-line summary shown in pickers and to the orchestrator",
  "tools": ["read_file", "list_files", "load_skill"],
  "packs": ["github"],
  "skills": ["planning", "explore-codebase"],
  "version": "1.0.0",
  "system_prompt": "You are ..."
}
```

- **name** (required) — human-readable display name.
- **description** (required) — one line; this is what the Orchestrator reads to
  decide whether to delegate to this persona, so make it specific.
- **tools** (required) — exact tool names the agent may call. Native tools
  include: `read_file`, `write_file`, `edit_file`, `bash`, `list_files`,
  `grep`, `find_files`, `web_fetch`, `load_skill`, `sub_agent`, and the meta
  tools (`persona_*`, `skill_*`, `flow_*`, `diagnostics_*`). Only list tools
  that exist.
- **packs** (optional) — integration pack ids (e.g. `"github"`, `"linear"`).
  Every HTTP-API tool the pack provides is added to the persona automatically,
  so you don't enumerate `<pack>_*` tools by name.
- **skills** (optional) — skill slugs the agent can `load_skill`. Only list
  skills that exist (`skill_list`).
- **version** (optional, semver) — set on personas you want force-upgraded on
  startup. Omit for one-off user personas.

## System prompt template variables

The `system_prompt` is a template. These placeholders are substituted with live
values, so place them where you want the lists to appear:

- `{{cwd}}` — current working directory.
- `{{available_skills}}` — bulleted list of the persona's skills + descriptions.
- `{{available_personas}}` — installed/enabled personas (use this in a
  delegating persona so it picks real `sub_agent` slugs instead of guessing).
- `{{installed_packs}}` — enabled integration packs.

Any list you don't reference is appended automatically with a default heading.

## Conventions

- Keep `tools` minimal — grant only what the persona's job needs.
- A delegating persona needs `sub_agent` and should reference
  `{{available_personas}}`.
- Pack-provided personas are read-only; to customize one, `persona_write` under
  a new slug.
