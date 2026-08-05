# Metalcraft Meet pack

Lets an agent schedule and manage a Metalcraft user's **video meetings** through
**Metalcraft Meet** (meet.metalcraftai.com) — part of the shared-login Metalcraft
ecosystem.

## Connects with

- **`METALCRAFT_TOKEN`** — a Personal Access Token from the user's Metalcraft account
  (id.metalcraftai.com → Account → Tokens), scoped `read` and/or `write`. This is the
  **only** setting, and the **same token works across every Metalcraft ecosystem app** —
  no per-service API keys.

The API base is **fixed to `https://meet.metalcraftai.com`**. Every tool sends
`Authorization: Bearer $METALCRAFT_TOKEN` and targets
`https://meet.metalcraftai.com/api/v1/…`. The account is implied by the token; a meeting
`slug` selects which meeting.

## The one call that does it all

`mmeet_schedule_meeting` creates the meeting, mints the `join_url`, **adds the event to the
user's Metalcraft Calendar**, and **emails the invitees** (each gets a personal join link +
an `.ics`). Resolve names to emails via the **Metalcraft Contacts** pack (`mcon_search`)
first, so *"schedule a call with Alice and Bob"* becomes real addresses.

## Tools
| Tool | Method | Path | Scope |
|------|--------|------|-------|
| `mmeet_whoami` | GET | `/api/v1/whoami` | read |
| `mmeet_schedule_meeting` | POST | `/api/v1/meetings` | **write** |
| `mmeet_list_meetings` | GET | `/api/v1/meetings` | read |
| `mmeet_get_meeting` | GET | `/api/v1/meetings/{slug}` | read |
| `mmeet_update_meeting` | PATCH | `/api/v1/meetings/{slug}` | **write** |
| `mmeet_cancel_meeting` | DELETE | `/api/v1/meetings/{slug}` | **write** |
| `mmeet_add_participants` | POST | `/api/v1/meetings/{slug}/participants` | **write** |

Write tools need a token with the `write` scope (else `403`). Reads auto-approve; the write
tools require approval — they create meetings, write to the calendar, and email real people.

## Notes
- Times are **UTC ISO-8601**; always pass an IANA `timezone`. Confirm the participant list
  and time with the user before scheduling — it emails real people.
- Prefer `mmeet_update_meeting` (re-syncs the calendar event) over cancel-and-recreate.
- Never reveal the token or raw tool URLs.
