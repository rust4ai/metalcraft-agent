---
description: Manage this metalcraft project (personas, skills, flows) by prompt using the meta tools
version: 1.0.0
---

# Workshop Overview

You can author and inspect this metalcraft project itself — the same things the
metalcraft-workshop GUI does — using the **meta tools**. Everything is plain
files under the data dir; the meta tools are the safe, validated way to edit them.

## What you can manage

- **Personas** (`persona_list`, `persona_read`, `persona_write`, `persona_delete`)
  — agent definitions: name, description, tools, skills, packs, system prompt.
  See the `authoring-personas` skill for the schema.
- **Skills** (`skill_list`, `skill_read`, `skill_write`, `skill_delete`)
  — reusable markdown methodology docs loaded on demand via `load_skill`.
  See the `authoring-skills` skill.
- **Flows** (`flow_list`, `flow_read`, `flow_validate`, `flow_write`,
  `flow_delete`, `flow_run`, `flow_templates_list`, `flow_template_read`)
  — JSON DAGs of prompt/branch nodes. See the `authoring-flows` skill.
- **Diagnostics** (`diagnostics_list`, `diagnostics_read`) — read-only history
  of past runs (turns, tool calls, errors).

## The author loop

1. **Read first.** List and read the current state before editing
   (`persona_list` / `skill_read` / `flow_read`). Don't overwrite blind.
2. **Validate before saving a flow.** Call `flow_validate` and fix every error
   before `flow_write` — `flow_write` re-validates and refuses to save invalid flows.
3. **Write.** `*_write` creates or overwrites. Pack-provided entries are
   read-only — pick a new slug if a write is refused.
4. **Confirm.** Re-read or list to confirm the change landed, then summarize
   what you changed for the user.

## Rules

- Slugs/ids are filenames: lowercase, hyphenated, no spaces or `.json`/`.md`.
- Never invent a tool or skill name in a persona — only reference tools/skills
  that actually exist (check with `skill_list` and the persona schema).
- Deletes only affect user-local files; integration-pack entries can't be deleted.
