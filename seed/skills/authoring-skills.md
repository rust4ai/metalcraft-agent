---
description: Format and conventions for authoring metalcraft skills
---

# Authoring Skills

A skill is a markdown file (`skills/<slug>.md`) holding reusable methodology an
agent loads on demand with `load_skill`. Create or edit with `skill_write`
(pass `slug`, `description`, `body`); read with `skill_read`; list with
`skill_list`.

## Format

A skill file is YAML frontmatter (a single `description:` line) followed by the
markdown body:

```markdown
---
description: One-line summary used to decide when to load this skill
---

# Skill Title

Concrete, step-by-step guidance the agent should follow for this task type.
```

When you call `skill_write`, you provide the `description` and `body`
separately — the frontmatter is assembled for you. Don't include the `---`
fences in the `body`.

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
