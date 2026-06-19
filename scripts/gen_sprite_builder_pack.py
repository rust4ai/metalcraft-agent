#!/usr/bin/env python3
"""Generate the sprite_builder integration pack under seed/integration_packs/.

One-shot authoring script: all pack content lives here so it's reviewable in one
place, then written out as the pack's pack.json / personas / skills / api_tools /
flow_templates. Safe to re-run (overwrites)."""
import json, os, pathlib

ROOT = pathlib.Path(__file__).resolve().parents[1]
PACK = ROOT / "seed" / "integration_packs" / "sprite_builder"

KEY = "$SPRITE_BUILDER_API_KEY"
URL = "$SPRITE_BUILDER_BASE_URL"
AUTH = {"Authorization": f"Bearer {KEY}"}
AUTH_JSON = {"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"}


def prop(t, desc, enum=None):
    p = {"type": t, "description": desc}
    if enum:
        p["enum"] = enum
    return p


def tool(name, desc, method, path, params, required, mapping, poll=False):
    headers = AUTH_JSON if method in ("POST", "PUT", "PATCH") else dict(AUTH)
    cfg = {
        "name": name,
        "description": desc,
        "method": method,
        "url": f"{URL}{path}",
        "headers": headers,
        "parameters": {"type": "object", "properties": params, "required": required},
        "body_mapping": mapping,
    }
    if poll:
        cfg["poll"] = True
    return cfg


PID = prop("string", "The project's id (UUID, from sprite_builder_list_projects / sprite_builder_create_project).")
BID = prop("string", "The build's id (UUID, from sprite_builder_create_build / sprite_builder_list_builds).")
CID = prop("string", "The codespace's id (UUID, from sprite_builder_create_codespace / sprite_builder_list_codespaces).")
DID = prop("string", "The docuspace's id (UUID, from sprite_builder_create_docuspace / sprite_builder_list_docuspaces).")

TOOLS = [
    # ---- Projects (shared across all three facets) ----
    tool("sprite_builder_list_projects",
         "List the Sprite Builder projects owned by the API key. Each project is a GitHub repo plus build settings; returns id (UUID), name, repo_full_name, default_branch, dockerfile_path, container_port. Start here to discover project ids.",
         "GET", "/api/projects", {}, [], "none"),
    tool("sprite_builder_get_project",
         "Get one project by id, including its repo_full_name, default_branch, dockerfile_path, and container_port.",
         "GET", "/api/projects/{project_id}", {"project_id": PID}, ["project_id"], "none"),
    tool("sprite_builder_list_repos",
         "List the GitHub repos the API key owner can access (via their GitHub OAuth grant). Use to find a repo_full_name (owner/repo) before creating a project.",
         "GET", "/api/repos", {}, [], "none"),
    tool("sprite_builder_create_project",
         "Create a project for a GitHub repo. Requires name and repo_full_name (owner/repo, must be reachable by the key owner's GitHub token). Optional: default_branch (default main), dockerfile_path (default Dockerfile), container_port (default 8080, the port the container listens on), repo_id (GitHub numeric id). Returns the new project with its id.",
         "POST", "/api/projects",
         {"name": prop("string", "Display name for the project."),
          "repo_full_name": prop("string", "GitHub repo as owner/repo."),
          "default_branch": prop("string", "Branch whose HEAD builds when no commit is given. Default 'main'."),
          "dockerfile_path": prop("string", "Path to the Dockerfile within the repo. Default 'Dockerfile'."),
          "container_port": prop("integer", "Port the built container listens on; mapped to the public URL. Default 8080."),
          "repo_id": prop("integer", "Optional GitHub numeric repo id.")},
         ["name", "repo_full_name"], "params"),

    # ---- Facet 1: Builds ----
    tool("sprite_builder_create_build",
         "Trigger a build: Docker-build the project's repo in a fresh sprites.dev sandbox and run it to get a live URL. Omit commit_sha to build HEAD of the project's default branch, or pass a specific commit_sha. Returns a Build in status 'queued'; poll sprite_builder_get_build until 'succeeded' (url is set) or 'failed'. Set any needed env vars (sprite_builder_set_env) BEFORE building.",
         "POST", "/api/projects/{project_id}/builds",
         {"project_id": PID,
          "commit_sha": prop("string", "Optional explicit commit SHA. Omitted = HEAD of the default branch.")},
         ["project_id"], "params"),
    tool("sprite_builder_get_build",
         "Get a build by id and check its status. Lifecycle: queued -> running -> succeeded | failed. On 'succeeded' the live URL is in `url`; `logs` holds build output and `error` is set on failure. Poll this after sprite_builder_create_build.",
         "GET", "/api/builds/{build_id}", {"build_id": BID}, ["build_id"], "none", poll=True),
    tool("sprite_builder_list_builds",
         "List a project's builds, newest first, with their status, commit_sha, and url.",
         "GET", "/api/projects/{project_id}/builds", {"project_id": PID}, ["project_id"], "none"),
    tool("sprite_builder_get_runtime_logs",
         "Fetch the runtime (container) logs of a deployed build's live sprite — distinct from the build logs. Useful to debug a running deployment.",
         "GET", "/api/builds/{build_id}/runtime-logs", {"build_id": BID}, ["build_id"], "none"),
    tool("sprite_builder_set_build_visibility",
         "Set whether a deployed build's public URL is publicly accessible. public=true makes it open; public=false requires auth. The build must have a deployment (a sprite).",
         "POST", "/api/builds/{build_id}/url-visibility",
         {"build_id": BID, "public": prop("boolean", "true = public URL, false = private/auth-gated.")},
         ["build_id", "public"], "params"),

    # ---- Env vars (injected into the deployed container at runtime) ----
    tool("sprite_builder_list_env",
         "List a project's environment variables (key + value) that get injected into the deployed container at runtime.",
         "GET", "/api/projects/{project_id}/env", {"project_id": PID}, ["project_id"], "none"),
    tool("sprite_builder_set_env",
         "Set (create or update) a project environment variable injected into the deployed container at `docker run`. Key must be letters/digits/underscores, not starting with a digit. Set vars BEFORE the build you want them in; values are redacted from logs.",
         "POST", "/api/projects/{project_id}/env",
         {"project_id": PID,
          "key": prop("string", "Env var name (letters, digits, underscores; not starting with a digit)."),
          "value": prop("string", "Env var value.")},
         ["project_id", "key", "value"], "params"),
    tool("sprite_builder_delete_env",
         "Delete a project environment variable by key.",
         "DELETE", "/api/projects/{project_id}/env/{key}",
         {"project_id": PID, "key": prop("string", "Env var name to delete.")},
         ["project_id", "key"], "none"),

    # ---- Facet 2: Codespaces (long-lived dev sandbox) ----
    tool("sprite_builder_create_codespace",
         "Create a Codespace: a long-lived sprites.dev sprite holding a git working tree at /workspace/app, for interactive coding (read/write files, run bash, run git). Provisioning is async. Returns the codespace in status 'queued'; poll sprite_builder_get_codespace until 'ready' before any file/exec/git/clone op. Optional name (defaults to a random adjective-noun).",
         "POST", "/api/projects/{project_id}/codespaces",
         {"project_id": PID, "name": prop("string", "Optional friendly name.")},
         ["project_id"], "params"),
    tool("sprite_builder_list_codespaces",
         "List a project's codespaces with their status, branch, and sprite_name.",
         "GET", "/api/projects/{project_id}/codespaces", {"project_id": PID}, ["project_id"], "none"),
    tool("sprite_builder_get_codespace",
         "Get a codespace by id and check its status. Lifecycle: queued -> provisioning -> ready | failed. All file/exec/git/clone ops require status 'ready'; otherwise they 400. Poll this after creating one.",
         "GET", "/api/codespaces/{codespace_id}", {"codespace_id": CID}, ["codespace_id"], "none", poll=True),
    tool("sprite_builder_delete_codespace",
         "Tear down a codespace and its sprite. Irreversible — confirm with the user first unless they asked.",
         "DELETE", "/api/codespaces/{codespace_id}", {"codespace_id": CID}, ["codespace_id"], "none"),
    tool("sprite_builder_codespace_clone",
         "Clone a repo into the codespace's /workspace/app using the owner's GitHub token. REPLACES whatever is there. Defaults to the codespace's project repo + branch; override with repo_full_name and/or branch. Codespace must be 'ready'.",
         "POST", "/api/codespaces/{codespace_id}/clone",
         {"codespace_id": CID,
          "repo_full_name": prop("string", "Optional repo to clone (owner/repo). Defaults to the project's repo."),
          "branch": prop("string", "Optional branch to check out. Defaults to the codespace's branch.")},
         ["codespace_id"], "params"),
    tool("sprite_builder_codespace_read",
         "Read a file OR list a directory under the codespace workspace (/workspace/app). Omit path for the workspace root. Returns kind ('file'|'dir'), and either entries[] (dir) or content (file; base64 with binary=true for non-text). Codespace must be 'ready'.",
         "GET", "/api/codespaces/{codespace_id}/files?path={path}",
         {"codespace_id": CID, "path": prop("string", "Path relative to /workspace/app. Omit for the root.")},
         ["codespace_id"], "none"),
    tool("sprite_builder_codespace_write",
         "Write (create or overwrite) a file in the codespace workspace. Path is relative to /workspace/app (no '..', no absolute paths, cannot be the root). Max 1 MiB. Codespace must be 'ready'.",
         "PUT", "/api/codespaces/{codespace_id}/files",
         {"codespace_id": CID,
          "path": prop("string", "File path relative to /workspace/app."),
          "content": prop("string", "Full file contents (text).")},
         ["codespace_id", "path", "content"], "params"),
    tool("sprite_builder_codespace_delete_path",
         "Delete a file or directory in the codespace workspace. Path relative to /workspace/app; cannot be the root.",
         "DELETE", "/api/codespaces/{codespace_id}/files?path={path}",
         {"codespace_id": CID, "path": prop("string", "Path relative to /workspace/app to delete.")},
         ["codespace_id", "path"], "none"),
    tool("sprite_builder_codespace_exec",
         "Run an arbitrary bash command in the codespace, from /workspace/app. Returns {output (merged stdout+stderr), exit_code}. Project env-var values are redacted from output. Codespace must be 'ready'. Use for builds, tests, installs, inspection.",
         "POST", "/api/codespaces/{codespace_id}/exec",
         {"codespace_id": CID, "cmd": prop("string", "Shell command to run, e.g. 'cargo test 2>&1 | tail -20'.")},
         ["codespace_id", "cmd"], "params"),
    tool("sprite_builder_codespace_git",
         "Run a git operation in the codespace working tree. op is one of status | diff | commit | push | pull. message is required for commit. push/pull authenticate with the owner's GitHub token. Returns {op, output, exit_code}.",
         "POST", "/api/codespaces/{codespace_id}/git",
         {"codespace_id": CID,
          "op": prop("string", "Git operation.", enum=["status", "diff", "commit", "push", "pull"]),
          "message": prop("string", "Commit message (required when op='commit').")},
         ["codespace_id", "op"], "params"),

    # ---- Facet 3: Docuspaces (S3-backed file store, no sprite) ----
    tool("sprite_builder_create_docuspace",
         "Create a Docuspace: an S3-backed file store for a project (no sprite, no worker — creation is instant). Use to store/serve files (markdown, images, assets). Optional name. Requires the server's S3_* env configured; otherwise file ops 400 with 'S3 is not configured'.",
         "POST", "/api/projects/{project_id}/docuspaces",
         {"project_id": PID, "name": prop("string", "Optional friendly name.")},
         ["project_id"], "params"),
    tool("sprite_builder_list_docuspaces",
         "List a project's docuspaces.",
         "GET", "/api/projects/{project_id}/docuspaces", {"project_id": PID}, ["project_id"], "none"),
    tool("sprite_builder_get_docuspace",
         "Get a docuspace by id.",
         "GET", "/api/docuspaces/{docuspace_id}", {"docuspace_id": DID}, ["docuspace_id"], "none"),
    tool("sprite_builder_delete_docuspace",
         "Delete a docuspace and all of its stored objects. Irreversible — confirm with the user first unless they asked.",
         "DELETE", "/api/docuspaces/{docuspace_id}", {"docuspace_id": DID}, ["docuspace_id"], "none"),
    tool("sprite_builder_docuspace_read",
         "Read a file OR list a directory in a docuspace. Omit path for the root. Folders are implicit. Returns kind ('file'|'dir'), and either entries[] or content (base64 with binary=true for non-text).",
         "GET", "/api/docuspaces/{docuspace_id}/files?path={path}",
         {"docuspace_id": DID, "path": prop("string", "Path within the docuspace. Omit for the root.")},
         ["docuspace_id"], "none"),
    tool("sprite_builder_docuspace_write",
         "Write (create or overwrite) a file in a docuspace. encoding 'utf8' (default) or 'base64' (to upload binary files as text). Content-type is inferred from the extension. Max 5 MiB.",
         "PUT", "/api/docuspaces/{docuspace_id}/files",
         {"docuspace_id": DID,
          "path": prop("string", "File path within the docuspace."),
          "content": prop("string", "File contents (utf8 text, or base64 if encoding='base64')."),
          "encoding": prop("string", "Content encoding.", enum=["utf8", "base64"])},
         ["docuspace_id", "path", "content"], "params"),
    tool("sprite_builder_docuspace_delete_path",
         "Delete a file, or a folder and everything under it, in a docuspace.",
         "DELETE", "/api/docuspaces/{docuspace_id}/files?path={path}",
         {"docuspace_id": DID, "path": prop("string", "Path within the docuspace to delete.")},
         ["docuspace_id", "path"], "none"),
    tool("sprite_builder_docuspace_create_folder",
         "Create an empty folder in a docuspace (writes a .keep marker). Only needed to make a folder visible before it has files — folders are otherwise implicit.",
         "POST", "/api/docuspaces/{docuspace_id}/folders",
         {"docuspace_id": DID, "path": prop("string", "Folder path to create.")},
         ["docuspace_id", "path"], "params"),
]

MANIFEST = {
    "id": "sprite_builder",
    "name": "Sprite Builder",
    "description": (
        "Persona, skills, and HTTP API tools to drive Sprite Builder over its REST API "
        "with a bearer API key. Sprite Builder hangs three facets off a Project (a GitHub repo): "
        "(1) Builds — Docker-build the repo in a sprites.dev sandbox and get a live URL; "
        "(2) Codespaces — a long-lived sprite with a git working tree you read/write/exec/git against; "
        "(3) Docuspaces — an S3-backed file store with no sprite. "
        "Authenticated by SPRITE_BUILDER_API_KEY against the instance at SPRITE_BUILDER_BASE_URL."
    ),
    "version": "1.0.0",
    "requires_env": ["SPRITE_BUILDER_API_KEY", "SPRITE_BUILDER_BASE_URL"],
}

PERSONA = {
    "name": "Sprite Builder Agent",
    "description": "Builds and deploys GitHub repos via Sprite Builder, and operates its codespaces (dev sandboxes) and docuspaces (S3 file stores) over the REST API",
    "tools": ["load_skill"],
    "packs": ["sprite_builder"],
    "skills": ["sprite-builder-builds", "sprite-builder-codespaces", "sprite-builder-docuspaces"],
    "system_prompt": (
        "You are a Sprite Builder assistant. You drive a Sprite Builder instance over its REST API using "
        "a bearer API key (SPRITE_BUILDER_API_KEY) against the base URL SPRITE_BUILDER_BASE_URL. Both are baked "
        "into every tool — you never pass the key or base URL yourself, and never reveal them or raw tool URLs.\n\n"
        "Everything hangs off a PROJECT (a GitHub repo). A project has three independent facets — pick by intent:\n"
        "- Ship a live URL from a repo -> BUILDS. Read the `sprite-builder-builds` skill.\n"
        "- Interactive coding/exec in a sandbox -> CODESPACES. Read the `sprite-builder-codespaces` skill.\n"
        "- Store/serve files (markdown, assets) with no running sandbox -> DOCUSPACES. Read the `sprite-builder-docuspaces` skill.\n\n"
        "Orient first: sprite_builder_list_projects shows projects and their ids; sprite_builder_list_repos shows "
        "GitHub repos you can turn into a project (sprite_builder_create_project). IDs are UUIDs and project/build/"
        "codespace/docuspace ids are distinct — don't cross them.\n\n"
        "Both Builds and Codespaces are asynchronous: create returns a queued record, then a background worker does "
        "the work. ALWAYS poll. A build goes queued -> running -> succeeded|failed (the live URL is in `url` on "
        "success). A codespace goes queued -> provisioning -> ready|failed, and its file/exec/git/clone ops only "
        "work once it is `ready`. Docuspaces are synchronous (no sprite).\n\n"
        "Mutations that cost money or destroy state — creating builds/codespaces, deleting codespaces/docuspaces, "
        "git push, set_build_visibility — confirm the target with the user before acting unless they clearly asked. "
        "Consult the relevant skill for the exact tools, lifecycles, and gotchas of each facet."
    ),
}

SKILL_BUILDS = """---
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
                 \\-> failed       (build.error / build.logs explain why)
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
"""

SKILL_CODESPACES = """---
description: Operate a Sprite Builder Codespace — a long-lived sprites.dev sandbox with a git working tree at /workspace/app. Create and provision it, then read/write files, run bash, and run git, all over the REST API.
---

# Sprite Builder — Codespaces

A **Codespace** is a persistent sprites.dev sprite holding a git working tree at
`/workspace/app`. Use it for interactive coding "as if on a local machine":
read/write files, run arbitrary bash, run git. Provisioning is **asynchronous**.

## Lifecycle — must be `ready` before operating

```
queued -> provisioning -> ready    (sprite live; file/exec/git/clone work)
                      \\-> failed    (see codespace.error / .logs)
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
"""

SKILL_DOCUSPACES = """---
description: Use a Sprite Builder Docuspace — an S3-backed file store attached to a project, with no sprite or worker. Create it, then read/list/write/delete files and folders over the REST API.
---

# Sprite Builder — Docuspaces

A **Docuspace** is plain S3-backed file storage owned by a project. There is **no
sprite and no worker** — creation and all file ops are synchronous/instant. Use it
to store and serve files (markdown, images, assets) without a running sandbox.

> Requires the Sprite Builder server's `S3_*` env to be configured. If it isn't,
> file ops return `400 "S3 is not configured"` (the docuspace record still exists).

## Tools

- `sprite_builder_create_docuspace` (with `project_id`) — instant, no provisioning.
- `sprite_builder_list_docuspaces`, `sprite_builder_get_docuspace`,
  `sprite_builder_delete_docuspace` (drops all its objects — confirm first).
- `sprite_builder_docuspace_read` — read a file or list a dir (omit `path` for root).
- `sprite_builder_docuspace_write` — create/overwrite a file. `encoding` is
  `utf8` (default) or `base64` (upload binaries as text). Max 5 MiB. Content-type
  is inferred from the extension so files render/download correctly.
- `sprite_builder_docuspace_delete_path` — delete a file, or a folder and all under it.
- `sprite_builder_docuspace_create_folder` — make an empty folder visible (folders
  are otherwise implicit — they exist when something lives under `<path>/`).

## Gotchas

- Folders are **implicit**; you only need `create_folder` to show an empty one.
- Paths are validated/jailed (no `..`, no absolute paths).
"""

FLOW_TEMPLATE = {
    "spec_version": "1",
    "id": "deploy-latest-and-report",
    "name": "Deploy Latest Commit and Report URL",
    "created_at": "2026-06-19T00:00:00Z",
    "updated_at": "2026-06-19T00:00:00Z",
    "enabled": False,
    "flow": {
        "nodes": [
            {
                "id": "entry",
                "node_type": "entry",
                "data": {"schedule_type": "hours", "interval": 24, "persona": "sprite-builder-agent"},
                "position": [0.0, 0.0],
            },
            {
                "id": "deploy",
                "node_type": "prompt",
                "data": {
                    "prompt": (
                        "Deploy the latest commit of the Sprite Builder project named PROJECT_NAME and report the result.\n\n"
                        "1. Call sprite_builder_list_projects and find the project whose name is PROJECT_NAME; note its id.\n"
                        "2. Trigger a build with sprite_builder_create_build for that project id (no commit_sha = HEAD of the default branch).\n"
                        "3. Poll sprite_builder_get_build with the returned build id every ~15 seconds until status is 'succeeded' or 'failed' (a build can take a few minutes).\n"
                        "4. On 'succeeded', report the commit_sha and the live `url`. On 'failed', report the `error` and the tail of `logs`.\n\n"
                        "IMPORTANT: replace PROJECT_NAME with the real project name before enabling this flow."
                    )
                },
                "position": [250.0, 0.0],
            },
        ],
        "edges": [{"id": "edge-entry-deploy", "source": "entry", "target": "deploy"}],
    },
}


def write_json(path, obj):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(obj, indent=2) + "\n")


def write_text(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


write_json(PACK / "pack.json", MANIFEST)
write_json(PACK / "personas" / "sprite-builder-agent.json", PERSONA)
write_text(PACK / "skills" / "sprite-builder-builds.md", SKILL_BUILDS)
write_text(PACK / "skills" / "sprite-builder-codespaces.md", SKILL_CODESPACES)
write_text(PACK / "skills" / "sprite-builder-docuspaces.md", SKILL_DOCUSPACES)
write_json(PACK / "flow_templates" / "deploy-latest-and-report.json", FLOW_TEMPLATE)
for t in TOOLS:
    write_json(PACK / "api_tools" / f"{t['name']}.json", t)

print(f"Wrote pack to {PACK}")
print(f"  api_tools: {len(TOOLS)}")
for t in TOOLS:
    print("   -", t["name"])
