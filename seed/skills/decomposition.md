---
description: Split a multi-step request into self-contained sub-tasks to delegate
version: 1.0.0
---

# Decomposition

For an agent that **delegates**. (`planning` is the companion skill for doing the
work yourself, step by step.) Your output here is not a solution — it is a short
ordered list of briefs, each one a single `sub_agent` call.

## Does this need decomposing?

Split when the request needs **three or more dependent steps**, spans more than
one subsystem or service, or you cannot name the steps out loud in one breath.

Do **not** split a lookup, a single-file edit, pure conversation, or anything one
specialist persona already covers end to end — send those straight to one
sub-agent. "This needs no plan, it's one delegation to X" is a correct and
complete answer. Splitting cheap work makes it slower, not better.

## Sizing

Sub-agents run **one at a time**, each with its own time budget, so the turn
takes as long as the sum of its steps. That is the real constraint:

- **3–7 steps.** Beyond ~9, phase it: make the last step "re-plan phase 2".
- One step = one bounded outcome one sub-agent can finish and report.
- Prefer fewer, larger steps over many tiny ones. Every extra step costs a whole
  agent run, and every step boundary loses context.
- Never split for parallelism. There is none.

## Writing the brief

A sub-agent inherits **none** of this conversation. Vague briefs are the single
biggest cause of useless sub-agent output. Each `task` must stand alone:

- Name the actual files, paths, endpoints, services, and identifiers.
- Say what "done" looks like, concretely enough to check.
- Paste in the facts from earlier steps that this step needs — the sub-agent
  cannot see them.
- State what is out of scope, so it doesn't wander.

Bad: "look into the auth bug."
Good: "In `src/workshop_api.rs`, find where the `mck_` token audience is
checked on `/api/pod/*`. Report the function name, the line, and what happens
when `aud` doesn't match. Read only — change nothing."

## Choosing who runs it

Prefer delegating to a **specialist persona** from your live persona list — it
arrives with the right tools, prompt, and skills. Fall back to `tool_set`
(`read_only` → `full` → `all` + `pack`) only when no persona fits. Grant `full`
only to steps that actually modify files or run commands.

## Running the steps

- Go in dependency order, one `sub_agent` call at a time.
- Feed each step's result into the brief of the step that depends on it.
- Give each step a check: how do you know it worked? A step whose result you
  cannot verify is a step you cannot build on.
- After each result, re-decide. Sequential execution is an advantage — you can
  re-scope, skip a step that turned out unnecessary, or abandon the rest of the
  plan. Don't run a stale plan to the end.
- If a step fails, re-delegate that step with a sharper brief before giving up.
- Close with what was done, what it found, and what is left.

## Existing plans

If a plan document already exists for this work (`docs/*_PLAN.md`, a file the
user names), read it first and follow it — don't re-derive it. Report where you
diverged from it and why.
