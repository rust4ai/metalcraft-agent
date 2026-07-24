---
description: How to manage Railway services and custom domains over the Railway GraphQL API
---

# Railway Services & Custom Domains

These tools call the Railway public GraphQL API
(`https://backboard.railway.com/graphql/v2`, always `POST`) using the account
tied to the configured `RAILWAY_API_TOKEN`. This pack sends the token as
`Authorization: Bearer $RAILWAY_API_TOKEN`, which is the header for **account**
and **workspace** tokens (created on the Account Settings → Tokens page).

> **Token levels.** Railway also has *project* tokens (created in a project's
> settings), which use a different header — `Project-Access-Token` — and are
> locked to a single environment. This pack does **not** support project tokens:
> `railway_whoami` and `railway_list_projects` won't work with one. If the user
> only has a project token, ask them for an account or workspace token.

Every response is the standard GraphQL envelope: `{ "data": ..., "errors": [...] }`
(the tool wraps it as `{ "status": 200, "data": { "data": ..., "errors": ... } }`).
Always check for an `errors` array even on HTTP 200 — GraphQL reports failures there.

## The hierarchy (why you need three ids)

Railway is nested: **project → services × environments → domains**. A domain is
attached to a *service* within a specific *environment*, so nearly every domain
call needs **`projectId` + `serviceId` + `environmentId`** together. Resolve them
in order:

1. `railway_whoami` — confirm the token works (`me { name email }`).
2. `railway_list_projects` — map a project name to its `id` (the `projectId`).
   Railway nests projects under **workspaces**, so this returns
   `me.workspaces[].projects.edges[].node` — iterate every workspace, not a
   single flat list. (A modern account's top-level `projects` field and even
   `me.projects` come back **empty** — the projects only appear under
   `me.workspaces[].projects`. If `whoami` succeeds but you see no projects,
   this workspace nesting is why.)
3. `railway_get_project` — pass that `projectId`; get the **services**
   (`services.edges[].node.id` → `serviceId`) and **environments**
   (`environments.edges[].node.id` → `environmentId`). This is how you "list
   services." Most projects have a `production` environment; pick the one the
   user means.

## Adding a custom domain and reading the DNS

4. `railway_list_domains` (optional) — with `projectId` + `environmentId` +
   `serviceId`, see what's already attached: `serviceDomains` are the
   Railway-provided `*.up.railway.app` hostnames; `customDomains` are user
   domains.
5. `railway_create_custom_domain` — `projectId` + `environmentId` + `serviceId`
   + `domain` (optional `targetPort`). Returns the new domain's `id` and
   `status { verificationToken certificateStatus dnsRecords { ... } }`.
6. `railway_get_custom_domain` — re-read a domain by its `id` + `projectId` at
   any time to check whether DNS has propagated and the cert has issued.

### The DNS records

`status.dnsRecords[]` is the authoritative list of what the user must create at
their DNS provider. Each record has:

- `recordType` — e.g. `CNAME` (subdomains typically get a CNAME).
- `hostlabel` / `fqdn` — the record **name** to create (the label, and the full
  name).
- `requiredValue` — the **value** to point it at (the Railway edge target, e.g.
  `xxxx.up.railway.app`). **This is the string the user pastes into their DNS.**
- `currentValue` — what DNS currently resolves to (empty/mismatched until they
  set it).
- `status` — whether the record matches what Railway expects yet.

`status.verificationToken` is a **TXT** record used to prove ownership — relay it
alongside the routing record when present. `status.certificateStatus` is the TLS
certificate provisioning state (Railway issues the cert automatically once DNS
verifies).

When you report DNS to the user, give them, per record: **type, name, value**,
and whether it's verified yet (compare `requiredValue` to `currentValue` /
`status`). Root/apex domains may require a different record than a subdomain —
follow whatever `dnsRecords` returns rather than assuming.

> Field note: `fqdn`, `recordType`, `zone`, and `purpose` are present in the live
> schema but under-documented. If a query ever errors on one of these fields,
> fall back to the documented subset — `hostlabel`, `requiredValue`,
> `currentValue`, `status`.

## Inspecting and managing services

Beyond domains, the pack can inspect a service's state and perform a small set of
**safe** write operations. All of these reuse the same `projectId` / `serviceId`
/ `environmentId` you resolved above.

- `railway_list_deployments` (read) — pass `serviceId` + `environmentId` to see
  the last few deployments newest-first: each has a `status`
  (`SUCCESS`, `BUILDING`, `DEPLOYING`, `FAILED`, `CRASHED`, `REMOVED`…), a
  `createdAt`, the live `url`/`staticUrl`, and `canRedeploy`. This is how you
  answer "is it up / did the last deploy succeed / what's the current URL."
- `railway_list_variables` (read) — pass `projectId` + `environmentId` (+ optional
  `serviceId`) to get the variable map. **The values are secrets** — reason over
  which keys exist or whether one is missing, but refer to variables by name and
  never print raw values.
- `railway_service_create` (write) — create a new service in a project. Give
  `projectId` plus EITHER `sourceRepo` (a GitHub `owner/repo` or URL) OR
  `sourceImage` (a Docker image); optional `name`, `branch`, `environmentId`.
  Returns the new service `{ id name projectId }`.
- `railway_redeploy` (write) — re-run a service's current deployment
  (`serviceId` + `environmentId`), e.g. to apply new variables or recover from a
  crash. To roll back to a *specific older* build, get its deployment `id` from
  `railway_list_deployments` — this pack redeploys the current build only.
- `railway_variable_upsert` (write) — set/update one variable (`projectId` +
  `environmentId` + `name` + `value`; optional `serviceId`, `skipDeploys`). By
  default it triggers a redeploy so the change takes effect; pass
  `skipDeploys: true` to batch several changes and redeploy once at the end.

There are **no delete** tools (no service/deployment/variable/domain removal). If
the user wants to delete something, point them to the Railway dashboard.

## Tools

| Tool | What it does |
|------|--------------|
| `railway_whoami` | Verify the token; returns `me { name email }`. No params. |
| `railway_list_projects` | List accessible projects (`id`, `name`) grouped by workspace (`me.workspaces[].projects`). No params. |
| `railway_get_project` | List a project's services + environments. Requires `id`. |
| `railway_list_deployments` | Inspect a service's recent deployments + status. Requires `serviceId`, `environmentId`. |
| `railway_list_variables` | Read a service/environment's variables (secrets). Requires `projectId`, `environmentId`. |
| `railway_service_create` | Create a service from a repo or image. Requires `projectId` + `sourceRepo`|`sourceImage`. |
| `railway_redeploy` | Redeploy a service's current build. Requires `serviceId`, `environmentId`. |
| `railway_variable_upsert` | Set/update one variable. Requires `projectId`, `environmentId`, `name`, `value`. |
| `railway_list_domains` | List a service's Railway + custom domains. Requires `projectId`, `environmentId`, `serviceId`. |
| `railway_create_custom_domain` | Add a custom domain. Requires `projectId`, `environmentId`, `serviceId`, `domain`; optional `targetPort`. |
| `railway_get_custom_domain` | Read a custom domain's DNS records + status. Requires `id`, `projectId`. |

## Safety

- Writes touch live infrastructure. Adding a domain changes routing;
  `railway_service_create` provisions a service; `railway_redeploy` restarts one;
  `railway_variable_upsert` changes config and (unless `skipDeploys`) redeploys.
  Confirm the exact **project → service → environment** and the intended change
  before calling any write tool.
- Variable values are secrets — never echo them or the token back to the user.
- Rate limits apply (100/hr free, higher on paid); back off on HTTP 429
  (`Retry-After`).
- Never echo the token or the raw endpoint back to the user.
