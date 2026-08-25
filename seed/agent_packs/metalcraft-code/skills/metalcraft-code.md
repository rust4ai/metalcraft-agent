# Metalcraft Code skill

Give the agent a full **remote coding environment** on **Metalcraft Code**
(`https://code.metalcraftai.com/api/v1`), authenticated by a single `METALCRAFT_TOKEN`
(`Authorization: Bearer`). The account is implied by the token — never pass a user id.

## Model
- The user connects a **GitHub App installation** once (Metalcraft Code web app → Connect GitHub).
- A **workspace** = one ephemeral **sprites.dev** sprite holding a git working tree at
  `/workspace/app`. `status` ∈ `queued | provisioning | ready | hibernated | failed`.
  **Only `ready` workspaces accept file/exec/git ops.**
- Git auth uses **short-lived GitHub App tokens minted per call** — never stored, never shown.
- A **run** records each action (exec/clone/build/test/git/actions) for the audit trail.

## Workflow
1. **`mcode_whoami`** — confirm the token and read `scopes`. Reads work with any token;
   **create/clone/write/exec/build/test/commit/push need `write`**. If write is missing, tell the
   user to mint a write token at id.metalcraftai.com → Account → Tokens, and stop.
2. **Find the repo** — **`mcode_list_installations`**, then **`mcode_list_repos`** (owner/repo).
   No installation? Tell the user to **Connect GitHub** at code.metalcraftai.com, and stop.
3. **Get a workspace** — reuse a `ready` one, **`mcode_wake_workspace`** a `hibernated` one, or
   **`mcode_create_workspace`** (optionally `repo_full_name`/`branch`) and **poll
   `mcode_get_workspace`** until `status: ready` (~1-2 min).
4. **Clone** — **`mcode_clone`** (`repo_full_name`, optional `branch`). Replaces `/workspace/app`.
5. **Explore** — **`mcode_read_file`** / **`mcode_list_dir`** (path relative to the repo root;
   omit for root). Read before you edit — don't guess contents.
6. **Edit** — **`mcode_write_file`** (`path`, full `content`). Delete with **`mcode_delete_path`**.
7. **Run** — **`mcode_exec`** for quick commands (≤120s), **`mcode_build`** / **`mcode_test`** for
   long ones (background + polled, ≤10 min). Iterate until green; fetch output via the returned run
   or **`mcode_list_runs`** / **`mcode_get_run`**.
8. **Review + ship** — **`mcode_git`** `op=status`/`op=diff`, then `op=commit` (`message`) and
   `op=push` (`branch`). Read-only status/diff need no write scope; commit/push do.
9. **CI** — **`mcode_configure_actions`** (`filename` like `ci.yml`, full `workflow_yaml`,
   `commit=true` to also commit; a later push activates it).
10. **Smoke-test a server** — **`mcode_expose`** `public=true` to get a public URL for an HTTP
    server running in the sprite; **revoke** with `public=false` when done.
11. **Done for now** — **`mcode_hibernate_workspace`** to save resources (wake later).

## Rules of thumb
- Only `ready` workspaces accept ops — **wake first** otherwise.
- **Read before editing**; never invent file contents, repo names, or ids — list first.
- Treat **commit/push** and destructive commands as significant: show the **diff** and confirm
  before pushing to a shared branch.
- Prefer reusing/waking a workspace over spinning a new sprite for the same repo.
- Summarize what changed (files, commits, pushes) afterward; never reveal the token or raw tool URLs.
