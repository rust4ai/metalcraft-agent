# Metalcraft Calendar

Read and manage a Metalcraft user's calendars and events.

These tools call the Metalcraft Calendar REST API at
`https://calendar.metalcraftai.com/api/v1` using the configured `METALCRAFT_TOKEN`,
sent as `Authorization: Bearer $METALCRAFT_TOKEN`. The base URL is fixed — the only
thing to configure is the token. That single token is the user's **Metalcraft account**
credential and works across every ecosystem app; there are no per-service keys.

## The model: account → many calendars → events
- **The token implies the account.** You never pass a user id.
- **One account owns many calendars**, each with a `slug` (e.g. `family`, `work`).
  Most calls take a `calendar` slug. Discover them with `mcal_list_calendars`.
- Each calendar may be linked to one **Google calendar** for two-way sync.

## Scopes (read vs write)
`mcal_whoami` returns the token's `scopes`. Creating, updating, or deleting events —
and creating calendars — requires **`write`**. Without it those calls return
403; tell the user to mint a token with `write` at
id.metalcraftai.com → Account → Tokens.

## Times
Always UTC ISO-8601 (e.g. `2026-07-28T07:00:00Z`). Convert the user's local time to
UTC before sending; convert back when reporting. Ask for their timezone if unknown.

## Workflow
1. **`mcal_whoami`** — validate the token, read `scopes`.
2. **`mcal_list_calendars`** — find the target calendar's `slug`. If the user names one
   that doesn't exist, list what's there and ask, or offer `mcal_create_calendar`.
3. **Read:** optionally `mcal_sync` (calendar slug) to pull the linked Google calendar,
   then `mcal_list_events` with a `from`/`to` window.
4. **Write (needs `write`):** `mcal_create_event`, `mcal_update_event`,
   `mcal_delete_event` — all scoped to a calendar slug.

## Lifecycle notes
- Resolve an event's `id` via `mcal_list_events` before get/update/delete.
- `mcal_update_event` **replaces** all fields — fetch the current event with
  `mcal_get_event` and resend unchanged values when altering just one.
- Deletes/updates affect the linked Google calendar. Confirm the exact event (title +
  time) before deleting; summarize what you changed afterward.
- Never reveal the token or raw tool URLs.
