---
description: How to monitor and triage errors in Sentry over the REST API — discovering projects, querying recent issues with the search syntax, reading stacktraces, and resolving/assigning issues
---

# Sentry Monitoring

These tools call the **Sentry** REST API (`https://sentry.io/api/0`) authenticated by a
personal **User Auth Token** (`SENTRY_AUTH_TOKEN`) scoped to one organization
(`SENTRY_ORG_SLUG`). The organization slug is already baked into every tool URL — you
only ever pass a `project_slug` or `issue_id`. You never pass the token yourself; every
tool already carries it as a `Bearer` header. This pack targets **sentry.io SaaS**.

## Token setup (for the user)

Create the token in Sentry under **Settings → Account → User Auth Tokens** (a personal
token, tied to your account). Give it these scopes:

- `org:read` and `project:read` — list projects and read issue/release metadata
- `event:read` — read issues and event stacktraces
- `event:write` (or `project:write`) — only needed to resolve/ignore/assign issues

Then store the token as `SENTRY_AUTH_TOKEN` and your organization slug (the `…/organizations/<slug>/`
part of your Sentry URL) as `SENTRY_ORG_SLUG` in the key store.

## Two identifiers per issue

Every issue has two ids — don't confuse them:

- **`id`** — a numeric string like `"4509812345"`. This is what `sentry_get_issue`,
  `sentry_get_latest_event`, and `sentry_update_issue` expect as `issue_id`.
- **`shortId`** — a human label like `BACKEND-1A2`. Use it when talking to the user; it
  is *not* accepted by the issue tools.

## Orient yourself first

- **`sentry_list_projects`** — the projects this token can see. Grab the `slug` you need
  for querying issues (and the numeric `id` for reference). The cheapest way to confirm
  the token works.

## Querying recent issues

**`sentry_list_issues`** is the workhorse. It needs a `project_slug` and accepts three
optional filters:

- **`query`** — a Sentry search string. Defaults to `is:unresolved` when omitted.
- **`statsPeriod`** — the time window, e.g. `24h` (default) or `14d`.
- **`limit`** — max issues (up to 100).

### Search query mini-language

Combine space-separated `key:value` terms (implicit AND):

| Query | Finds |
|-------|-------|
| `is:unresolved` | open issues (the default) |
| `is:resolved` / `is:ignored` | issues in that state |
| `level:error` / `level:warning` | by severity |
| `is:unresolved firstSeen:-24h` | new in the last 24h |
| `lastSeen:-1h` | active in the last hour |
| `assigned:me` / `assigned:user@example.com` | by assignee |
| `is:unassigned` | not yet triaged |
| `error.type:ValueError` | by exception type |
| `release:1.2.3` | tied to a release |
| free text | matches the issue title/message |

Sort is by most recent activity. Report each issue's `shortId`, `title`, `culprit`,
`count`, `userCount`, and `lastSeen`.

## Inspecting one issue

- **`sentry_get_issue`** (by numeric `issue_id`) — status, assignment, counts, first/last
  seen, tag distributions.
- **`sentry_get_latest_event`** (by numeric `issue_id`) — the newest occurrence: exception
  type and value, the full stacktrace (`entries` → frames), `tags`, `contexts` (runtime/OS),
  and request data. This is what you read to actually debug an error.

## Correlating with releases

- **`sentry_list_releases`** — recent deploys for the org, newest first. Pass `query` with a
  version substring to find a specific one. Useful when a spike of `firstSeen:-24h` issues
  lines up with a release's `dateReleased`.

## Resolving / assigning (writes)

**`sentry_update_issue`** changes one issue (by numeric `issue_id`). Only the fields you pass
are touched:

- `status`: `resolved`, `ignored`, or `unresolved`.
- `assignedTo`: `user@example.com`, `user:<id>`, or `team:<id>`; empty string unassigns.

This mutates real Sentry data. Confirm the exact issue and the intended change with the user
before calling it, unless they've already asked for it explicitly.

## Typical flow

1. `sentry_list_projects` → pick the `slug`.
2. `sentry_list_issues` with a `query`/`statsPeriod` → find the issue, note its numeric `id`.
3. `sentry_get_latest_event` (and/or `sentry_get_issue`) → read the stacktrace and context.
4. Optionally `sentry_list_releases` to see if a recent deploy correlates.
5. After confirming with the user, `sentry_update_issue` to resolve/ignore/assign.
