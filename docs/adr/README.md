# Architecture Decision Records (ADRs)

This directory captures **architecture decisions** for Metalcraft Agent — the
"why" behind choices that are not obvious from the code alone, and the rules we
hold ourselves to going forward.

For *how the pieces fit together*, see [`../architecture.md`](../architecture.md).
ADRs are narrower: each one records a single decision, its context, and its
consequences, so a future contributor can understand the reasoning instead of
guessing or silently reversing it.

## Status values

- **Proposed** — under discussion, not yet binding.
- **Accepted** — the decision is in force; follow it.
- **Superseded** — replaced by a later ADR (link it).
- **Deprecated** — no longer relevant, kept for history.

## Format

One file per decision, named `NNNN-short-kebab-title.md` (zero-padded, monotonic).
Each record uses the template below:

```markdown
# ADR-NNNN: Title

- **Status:** Accepted
- **Date:** YYYY-MM-DD
- **Deciders:** <names>

## Context
What problem/force prompted this decision?

## Decision
What we decided to do (stated as a rule, in the imperative).

## Consequences
What this makes easier or harder; how to comply; how it's enforced.

## Alternatives considered
What else we looked at and why we rejected it.
```

## Index

| ADR | Title | Status |
| --- | --- | --- |
| [0001](0001-dynamic-substitution-in-system-prompts.md) | Dynamic substitution in system prompts | Accepted |
