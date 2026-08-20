# Metalcraft Calendar pack

Lets an agent read and manage a Metalcraft user's calendars through
**Metalcraft Calendar** (calendar.metalcraftai.com) — part of the shared-login
Metalcraft ecosystem.

## Connects with

- **`METALCRAFT_TOKEN`** — a Personal Access Token from the user's Metalcraft
  account (id.metalcraftai.com → Account → Tokens), scoped `read` and/or
  `write`. This is the **only** setting, and the **same token works across
  every Metalcraft ecosystem app** — no per-service API keys.

The API base is **fixed to `https://calendar.metalcraftai.com`**. Every tool sends
`Authorization: Bearer $METALCRAFT_TOKEN` and targets
`https://calendar.metalcraftai.com/api/v1/…`. The account is implied by the token; a
**calendar `slug`** selects which calendar.

## Model
One account owns **many calendars**, each with a `slug`. Discover them with
`mcal_list_calendars`, then address events by `calendar` slug + event `id`.

## Tools
| Tool | Method | Path | Scope |
|------|--------|------|-------|
| `mcal_whoami` | GET | `/api/v1/whoami` | read |
| `mcal_list_calendars` | GET | `/api/v1/calendars` | read |
| `mcal_create_calendar` | POST | `/api/v1/calendars` | **write** |
| `mcal_list_events` | GET | `/api/v1/calendars/{calendar}/events` | read |
| `mcal_get_event` | GET | `/api/v1/calendars/{calendar}/events/{id}` | read |
| `mcal_create_event` | POST | `/api/v1/calendars/{calendar}/events` | **write** |
| `mcal_update_event` | PATCH | `/api/v1/calendars/{calendar}/events/{id}` | **write** |
| `mcal_delete_event` | DELETE | `/api/v1/calendars/{calendar}/events/{id}` | **write** |
| `mcal_sync` | POST | `/api/v1/calendars/{calendar}/sync` | read |

Reads + `mcal_sync` auto-approve; create/update/delete require approval. Writes need a
token with the `write` scope (403 otherwise). Times are UTC ISO-8601.

## Ships
- `personas/metalcraft-calendar-agent.json` — a scheduling assistant scoped to this pack.
- `skills/metalcraft-calendar.md` — the whoami → list_calendars → (sync) → list_events →
  create/update/delete workflow, scope + slug rules.
