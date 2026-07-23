---
description: How to manage Render services and custom domains over the Render REST API, including deriving the DNS records Render's API does not return
---

# Render Services & Custom Domains

These tools call the Render REST API (`https://api.render.com/v1`) using the
workspace(s) tied to the configured `RENDER_API_KEY`, sent as
`Authorization: Bearer $RENDER_API_KEY`.

**One key, one header.** Unlike Railway, Render has a single kind of API key and
a single auth header. There is no project/access-token split. The only extra
concept is the **workspace ("owner")**: one key can span several workspaces, and
you disambiguate by passing `ownerId` (from `render_list_owners`) as a query
param — never a different key. If the key has one workspace, `ownerId` is
optional everywhere.

Responses are plain JSON. List endpoints use **cursor pagination**: the response
carries a `cursor` you pass back as `cursor=` to get the next page (`limit` 1-100,
default 20).

## The workflow

1. `render_list_owners` — confirm the key works; note the workspace `id`
   (`ownerId`).
2. `render_list_services` — find the service. Keep two fields: the service `id`
   (`srv-...`, the `serviceId` every domain call needs) and
   **`serviceDetails.url`** (the service's `*.onrender.com` hostname — you'll need
   it as the CNAME target below).
3. `render_create_custom_domain` — attach the domain (`serviceId` + `name`).
   Returns `id`, `domainType`, `verificationStatus`.
4. **Derive and report the DNS record** (see next section — the API won't give you
   the target).
5. After the user sets DNS: `render_verify_custom_domain` to trigger a re-check,
   then `render_get_custom_domain` to read `verificationStatus`.

## Reading / deriving the required DNS record (important)

Render's custom-domain object returns only `domainType` and `verificationStatus`
— **it does NOT return the CNAME target or the A-record IP.** You must construct
the instruction yourself from `domainType`:

| domainType | Record type | Name (host) | Value |
|------------|-------------|-------------|-------|
| `subdomain` (e.g. `app.example.com`) | **CNAME** | the subdomain (`app`) | the service's `serviceDetails.url` (e.g. `myapp.onrender.com`), from `render_list_services` |
| `apex` (root, e.g. `example.com`) | **A** | the root (`@`) | **`216.24.57.1`** — Render's fixed load-balancer IP |

Notes:
- The apex IP `216.24.57.1` is documented by Render, **not** returned by the API.
  If an apex domain won't verify, re-check Render's current DNS docs in case the
  IP changed.
- **Wildcard** domains (`*.example.com`) additionally need two verification CNAMEs
  documented by Render: `_acme-challenge` → `<service-id>.verify.renderdns.com`
  and `_cf-custom-hostname` → `<service-id>.hostname.renderdns.com`. Point the
  user to Render's wildcard docs for these rather than guessing.
- Always state the record explicitly as **TYPE / NAME / VALUE** so the user can
  paste it into their DNS provider. `verificationStatus` is your only
  programmatic signal that DNS is correct — poll it after `render_verify_custom_domain`.
- Render auto-provisions and renews TLS once the domain is `verified`; no cert
  handling is needed.

## Tools

| Tool | Method | What it does |
|------|--------|--------------|
| `render_list_owners` | GET | List workspaces; verify the key. Returns `ownerId`s. No params. |
| `render_list_services` | GET | List services (`id`, `name`, `serviceDetails.url`). Optional `ownerId`, `name`, `limit`, `cursor`. |
| `render_list_custom_domains` | GET | List a service's custom domains + `verificationStatus`. Requires `serviceId`. |
| `render_create_custom_domain` | POST | Add a custom domain (`serviceId` + `name`). Body is `{"name": ...}`. |
| `render_get_custom_domain` | GET | Read one domain's `domainType` + `verificationStatus`. Requires `serviceId`, `customDomainIdOrName`. |
| `render_verify_custom_domain` | POST | Trigger a DNS re-check (202). Requires `serviceId`, `customDomainIdOrName`. |

## Safety

- Adding a domain changes live routing for a service. Confirm the exact **service**
  and **domain** before creating.
- Custom domains require a **paid** Render plan.
- Handle HTTP 429 (rate limited) by backing off.
- Never echo the key or the raw endpoint back to the user.
