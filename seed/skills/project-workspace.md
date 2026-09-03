---
name: project-workspace
description: How a project agent uses a buildr.space workspace — reconcile, work, commit, hibernate — and how to hand a long build back to the heartbeat
version: 1.0.0
---

# Working in a buildr.space workspace

Your code work happens in a remote workspace, not on the pod. The pod holds your
goal, your scratchpad and your journal; the workspace holds the repo. It is
reached entirely through the `buildr_*` tools.

The one thing to understand before anything else:

> **The workspace is a cache. The branch on GitHub and your scratchpad are the
> truth.**

A workspace can be hibernated, reaped, deleted after a week asleep on the free
plan, or thrown away because the sprite got into a bad state. None of that is an
error — it is Tuesday. What must survive is what you pushed and what you wrote
down.

## Every tick, in order

**1. Reconcile.** Your scratchpad's `Workspace` section says what you had. If it
names no workspace, or the tick's opening note says it is gone, create one
(`buildr_create_workspace`) and clone the repo (`buildr_clone`) at the goal's
branch. Then write the new id into the scratchpad before doing anything else — a
workspace you provisioned and did not record is one the next tick will provision
again.

**2. Wake it** with `buildr_wake_workspace` if it is hibernated. A tick that only
reads GitHub or only rewrites the plan does not need the box at all — do not wake
one to think.

**3. Do the work.** `buildr_read_file` / `buildr_write_file` / `buildr_exec` for
edits, `buildr_build` and `buildr_test` for anything long.

**4. Commit and push before you finish.** `buildr_git` with `op: "commit"`, then
`op: "push"` — and **always pass `branch` explicitly**. The workspace's recorded
branch is not updated by a `git checkout -b` you ran through `exec`, so a push
that names no branch can quietly go to the wrong one.

Uncommitted work does not exist. If the workspace disappears tonight, everything
you did not push is gone, and the next tick will believe the step was never
started.

**5. Hibernate.** The runner does this for you at the end of every tick, so you
do not have to — but never leave a dev server running past the tick that needed
it (`buildr_serve_stop`), because a serving workspace is deliberately exempt from
hibernation and will bill for hours.

## Long commands: hand them back

`buildr_build` and `buildr_test` start the command and keep running after the
call returns. A cold build outlives your tick. **Do not poll it in a loop** —
that spends the whole tick waiting, and the run finishes after you are gone
regardless.

Instead: start it, then call `goal_await_run` with the workspace id and the run
id, write your scratchpad, and end the tick. The next wake-up reads the result
without spending a model on it and hands you the outcome — including the output
of a failure — as the first thing in its prompt.

That is the whole trick that makes a thirty-minute heartbeat able to build
software: the wait is free, and the tick after it starts already knowing.

## Verifying

A tick may not check a plan box it did not verify.

- **Compiles is not works.** For anything user-facing, `buildr_serve` it and
  `buildr_fetch` the page, or use `buildr_render` to look at it.
- **Green is not green everywhere.** Once a PR is open, CI is the real verdict —
  read it before claiming the step landed.
- If verification failed, say so in `State` and leave the box unchecked. A tick
  that reports honestly is worth more than one that looks tidy; the next tick
  can act on the truth and cannot act on the tidiness.
