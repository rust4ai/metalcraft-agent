---
description: Deploy a GitHub repo to a live URL with Sprite Builder Builds — create a project, trigger a build in a sprites.dev sandbox, poll its lifecycle, manage runtime env vars, and control URL visibility.
---

# Sprite Builder — Builds

A **Build** Docker-builds a project's repo at a commit inside a fresh sprites.dev
sandbox, runs the image on the project's `container_port`, and returns a public URL.
Builds are **asynchronous** — a background worker does the work, so you must poll.

## Deploy flow

1. **Find or create the project.** `sprite_builder_list_projects` (or
   `sprite_builder_create_project` with `name` + `repo_full_name`). The repo must be
   reachable by the key owner's GitHub token.
2. **(Optional) set runtime env vars** with `sprite_builder_set_env` — do this
   *before* the build you want them in. Values are injected at `docker run` and
   redacted from logs.
3. **Trigger** `sprite_builder_create_build` with the `project_id`. Omit
   `commit_sha` to build the default branch's HEAD, or pin a commit. Returns a
   build in status `queued`.
4. **Poll** `sprite_builder_get_build` with the `build_id` until status is
   `succeeded` or `failed`.

## Lifecycle

```
queued -> running -> succeeded   (build.url is the live URL; container is up)
                 \-> failed       (build.error / build.logs explain why)
```

Builds can take a few minutes. On success the URL is **server-assigned** — read
`build.url`, never construct it.

## Other build tools

- `sprite_builder_list_builds` — a project's build history.
- `sprite_builder_get_runtime_logs` — logs from the *running* container (distinct
  from build logs) to debug a live deployment.
- `sprite_builder_set_build_visibility` — `public=true/false` to open or auth-gate
  the deployed URL. Mutates a live deployment; confirm first.

## Env var tools

- `sprite_builder_list_env`, `sprite_builder_set_env` (upsert), `sprite_builder_delete_env`.
- Keys: letters/digits/underscores, not starting with a digit.

## Gotchas

- Everything is owner-scoped — a 404 usually means "not yours," not "gone."
- A failed clone almost always means the repo isn't reachable by the owner's
  GitHub grant (it requested `read:user repo`).
