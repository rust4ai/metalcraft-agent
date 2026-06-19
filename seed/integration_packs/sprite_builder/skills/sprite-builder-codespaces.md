---
description: Operate a Sprite Builder Codespace — a long-lived sprites.dev sandbox with a git working tree at /workspace/app. Create and provision it, then read/write files, run bash, and run git, all over the REST API.
---

# Sprite Builder — Codespaces

A **Codespace** is a persistent sprites.dev sprite holding a git working tree at
`/workspace/app`. Use it for interactive coding "as if on a local machine":
read/write files, run arbitrary bash, run git. Provisioning is **asynchronous**.

## Lifecycle — must be `ready` before operating

```
queued -> provisioning -> ready    (sprite live; file/exec/git/clone work)
                      \-> failed    (see codespace.error / .logs)
```

`sprite_builder_create_codespace` (with `project_id`) returns a `queued`
codespace. Poll `sprite_builder_get_codespace` until `ready` — **every**
file/exec/git/clone op `400`s until then.

## Working in the sandbox

- `sprite_builder_codespace_clone` — clone a repo into `/workspace/app` (defaults
  to the project's repo + the codespace's branch). **Replaces** the workspace.
  Provisioning does NOT auto-clone, so do this first.
- `sprite_builder_codespace_read` — read a file or list a dir (omit `path` for the
  root). Returns `kind` plus `entries[]` (dir) or `content` (file).
- `sprite_builder_codespace_write` — create/overwrite a file (max 1 MiB).
- `sprite_builder_codespace_delete_path` — delete a file or dir.
- `sprite_builder_codespace_exec` — run bash from `/workspace/app`; returns
  `{output, exit_code}`. Your workhorse for builds, tests, installs, inspection.
- `sprite_builder_codespace_git` — `op`: `status` | `diff` | `commit` (needs
  `message`) | `push` | `pull`. push/pull use the owner's GitHub token.

## Gotchas

- **Paths are jailed under `/workspace/app`** — no `..`, no absolute paths, and you
  cannot write/delete the workspace root.
- Project env-var values are **redacted** from exec/command output.
- `sprite_builder_delete_codespace` tears down the sprite — confirm first.
