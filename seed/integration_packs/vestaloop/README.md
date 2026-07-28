# VestaLoop pack

Read and manage a **household calendar** through the [VestaLoop](https://vestaloop.com)
portal's API-key REST API. VestaLoop issues simple, workspace + member scoped bearer
keys, so this is a clean declarative HTTP-API pack — no OAuth, no native code.

## Connects with

- **`VESTALOOP_API_KEY`** — an `hk_…` key minted in the portal (a home's **API keys**
  tab), scoped to one workspace (home) + member, `read` or `read/write`. This is the
  **only** setting the pack needs.

The API base is **fixed to `https://vestaloop.com`** (baked into the tool URLs), so
there is no base-URL to configure. Every tool sends `Authorization: Bearer
$VESTALOOP_API_KEY` and targets `https://vestaloop.com/api/v1/…`. The workspace and
member are **implied by the key**, so no ids are ever passed.

## Tools

| Tool | Method | Path | Notes |
|------|--------|------|-------|
| `vestaloop_whoami` | GET | `/api/v1/whoami` | verify key + see `access` scope |
| `vestaloop_list_events` | GET | `/api/v1/events?from&to` | list/search by time window |
| `vestaloop_get_event` | GET | `/api/v1/events/{id}` | one event |
| `vestaloop_create_event` | POST | `/api/v1/events` | **write key** |
| `vestaloop_update_event` | PATCH | `/api/v1/events/{id}` | **write key**; replaces all fields |
| `vestaloop_delete_event` | DELETE | `/api/v1/events/{id}` | **write key** |
| `vestaloop_sync` | POST | `/api/v1/sync` | pull the member's linked **Google Calendar** |

Reads and the idempotent `sync` auto-approve; `create`/`update`/`delete` require
approval (they change real events and push to the linked Google Calendar).

## Notes

- **Times are UTC ISO-8601** (e.g. `2026-07-28T07:00:00Z`) — convert the user's
  local time before sending.
- **Read vs write:** mutations need a `read/write` key; a read-only key returns 403.
- **`scope`** is a cosmetic `personal | shared` tag; it does not change which Google
  Calendar an event syncs to.

See the `vestaloop-calendar` skill for the full workflow.
