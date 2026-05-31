---
description: How to read and write Linear tasks over the GraphQL API — the entity model, id conventions, and create/update/comment flows
---

# Linear Tasks

These tools call the Linear GraphQL API (`https://api.linear.app/graphql`)
authenticated by a single `LINEAR_API_KEY` (a personal API key from
linear.app → Settings → API). Every tool already carries the key — you never pass
it. Each tool wraps a fixed GraphQL query/mutation and exposes only the variables
you fill in, so you don't write GraphQL yourself.

## Entity model

- **Workspace** → **Teams** (each has a `key` like `ENG`) → **Issues** (tasks).
- **Projects** group issues across a team.
- **Workflow states** are a team's statuses, each with a `type`: `backlog`,
  `unstarted` (todo), `started` (in progress), `completed` (done), `canceled`.

## Two ids per issue (important)

- `id` — the **UUID**. This is what `linear_update_issue` and
  `linear_create_comment` need.
- `identifier` — the **human key** like `ENG-123`. Use it when talking to the user.

`linear_get_issue` accepts either form, but the write tools want the UUID — grab it
from `linear_list_issues` / `linear_get_issue`.

## Reading

- **`linear_viewer`** — who the key belongs to (cheap auth check).
- **`linear_list_teams`** — `{ id, name, key }`. You need a team `id` to create
  issues.
- **`linear_list_projects`** — `{ id, name, state }`.
- **`linear_list_issues`** — recent issues (pass `first` to cap the count).
- **`linear_get_issue`** — full detail for one issue.
- **`linear_list_workflow_states`** — a team's statuses; you need a state `id` to
  move an issue.

## Creating a task

`linear_create_issue` requires `title` and `teamId`:

1. If you don't know the team, call `linear_list_teams` and pick (or ask).
2. Create with `title`, `teamId`, and optionally `description`, `priority`
   (0 none, 1 urgent, 2 high, 3 medium, 4 low), `assigneeId`, `projectId`.
3. Report the returned `identifier` and `url` to the user.

## Updating a task

`linear_update_issue` takes the issue `id` (UUID) plus only the fields to change:

- Rename / re-describe: pass `title` / `description`.
- Re-prioritize: pass `priority` (0-4).
- **Move status**: call `linear_list_workflow_states` for the issue's team, choose
  the state with the right `type` (e.g. `started` for In Progress, `completed` for
  Done), then pass its `id` as `stateId`.

Fields you omit are left unchanged.

## Commenting

`linear_create_comment` needs the issue UUID (`issueId`) and a markdown `body`.

## Safety

- These tools change the user's real workspace. Confirm ambiguous targets (which
  team? which issue?) before creating or updating.
- After a write, surface the issue `identifier` and `url` so the user can verify.
- Never expose the API key or raw tool URLs.
