# ADR-0001: Dynamic substitution in system prompts

- **Status:** Accepted
- **Date:** 2026-06-08
- **Deciders:** Metalcraft maintainers

## Context

Persona system prompts need to reference *dynamic* parts of the agent's
configuration — the specialist personas that can be delegated to, the
integration packs that are enabled, the skills that can be loaded. These lists
change at runtime: a user can install a pack, add or rename a persona, or enable
a skill at any time.

Historically the orchestrator's prompt hardcoded these lists inline, e.g.
`persona: "github-agent"` for GitHub, `persona: "linear-agent"` for Linear, and
a literal `meta-agent` slug for self-configuration. This caused concrete bugs:

- When `meta-agent` was renamed to `config-agent`, the hardcoded reference in the
  orchestrator prompt went stale and pointed at a non-existent persona.
- Personas added *after* a prompt was written (e.g. `cloudflare-agent`) were
  invisible to the orchestrator, which could only delegate to slugs it already
  knew about — so it silently failed to delegate or guessed wrong slugs.
- The hardcoded list and the live list could disagree, and the model had no way
  to know which to trust.

The persona builder (`Persona::build_system_prompt` in `src/persona.rs`) already
supports mustache-style substitution. These placeholders are replaced with live
values at prompt-assembly time:

| Placeholder | Injected value |
| --- | --- |
| `{{cwd}}` | Current working directory |
| `{{available_skills}}` | Skills this persona can `load_skill` (slug + description) |
| `{{available_personas}}` | Installed/enabled specialist personas for `sub_agent` (slug + pack + description) |
| `{{installed_packs}}` | Enabled integration packs (id + name + description) |

Any list the template does *not* reference is appended afterward with a default
heading, so older prompts keep working — but relying on that fallback hides the
list in a fixed position instead of where the author wants it.

## Decision

**System prompts MUST use the substitution placeholders for any list of dynamic
configuration elements, and MUST NOT hardcode those lists.**

Concretely:

1. To tell a delegating persona which sub-agents exist, use
   `{{available_personas}}` — never an inline list of persona slugs.
2. To reference enabled integrations, use `{{installed_packs}}` — never an inline
   list of pack ids.
3. To reference loadable skills, use `{{available_skills}}`.
4. To reference the working directory, use `{{cwd}}`.
5. Prompts may still give *illustrative, phrasing-level* examples ("open a GitHub
   issue → the persona built for GitHub"), but the **authoritative slug/id list
   the model selects from must come from a placeholder**, and the prompt should
   instruct the model to match on the injected descriptions rather than guess.
6. When a new kind of dynamic list needs to appear in prompts, add a new
   placeholder + injection in `build_system_prompt` rather than hardcoding it in
   each persona.

## Consequences

- Renaming, adding, or removing a persona/pack/skill is automatically reflected
  in every prompt that uses the placeholder — no prompt edits, no stale slugs.
- New personas/packs become delegable immediately, with no prompt changes.
- Prompt authors place each list exactly where it reads best; the fallback-append
  behavior remains only as a safety net for prompts that forget a placeholder.
- Enforcement: prefer the placeholder in code review; the `template_uses` helper
  in `src/persona.rs` is the mechanism that detects whether a prompt references a
  given list. Seed personas under `seed/personas/` are the canonical examples to
  copy from.

## Alternatives considered

- **Hardcode the lists inline** — rejected: the status quo that produced the
  stale-slug and invisible-persona bugs above.
- **Regenerate prompts from config at build time** — rejected: more machinery
  than runtime substitution, and still stale between rebuilds.
- **Drop the lists entirely and have the model call a "list personas" tool
  first** — rejected for now: adds a round-trip to every delegation; injecting
  the list is cheaper and deterministic. Could revisit if lists grow large.
