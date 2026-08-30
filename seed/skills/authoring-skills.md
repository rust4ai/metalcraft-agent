---
description: Format and conventions for authoring metalcraft skills
version: 1.1.0
---

# Authoring Skills

A skill is a markdown file (`skills/<slug>.md`) holding reusable methodology an
agent loads on demand with `load_skill`. Create or edit with `skill_write`
(pass `slug`, `description`, `body`); read with `skill_read`; list with
`skill_list`.

## Format

A skill file is YAML frontmatter (a `description:` line, and optionally a
`version:` line) followed by the markdown body:

```markdown
---
description: One-line summary used to decide when to load this skill
version: 1.0.0
---

# Skill Title

Concrete, step-by-step guidance the agent should follow for this task type.
```

When you call `skill_write`, you provide the `description` and `body`
separately — the frontmatter is assembled for you. Don't include the `---`
fences in the `body`.

## Versions and the built-in skills

`version` is what lets a corrected **built-in** skill reach a pod that was
seeded months ago: when the bundled copy's version is higher than the installed
one, it overwrites it on the next start. That is also the catch — **editing a
built-in skill in place is not durable**. Your edit survives until the next
version bump, then the bundled text wins.

So to customize built-in guidance, copy it to a **new slug** and edit that. A
slug the binary doesn't ship is never force-upgraded, whether or not it carries
a version.

`skill_write` omits `version` by default and keeps whatever the skill already
had, so a normal edit doesn't reset a built-in to 0.0.0 (which would hand the
next bump a free overwrite). Pass `version` explicitly only when you mean to
version a skill of your own.

## Conventions

- **description** — one line, action-oriented. It's shown in the persona's
  available-skills list and is how the agent decides to load the skill, so make
  it precise (e.g. "Break multi-step tasks into clear plans", not "planning").
- **body** — focused methodology, not background prose. Numbered steps,
  checklists, and concrete rules work best. Keep it to one responsibility per
  skill; split if it grows two distinct topics.
- **slug** — lowercase, hyphenated, no `.md` (e.g. `code-review`).
- A skill only matters once a persona lists it in its `skills` array — after
  writing a skill, add it to the relevant persona with `persona_write`.
- Pack-provided skills are read-only; write under a new slug to customize.
- Built-in skills are writable but re-seed on a version bump — customize under
  a new slug too (see above).
