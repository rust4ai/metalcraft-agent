---
description: How to read and manage a VestaLoop household calendar over its API-key REST API — verify scope, list/search events, create/update/delete, and keep Google in sync, with UTC time handling
---

# VestaLoop Household Calendar

These tools call the VestaLoop REST API at `$VESTALOOP_BASE_URL/api/v1` using the
configured `VESTALOOP_API_KEY`, sent as `Authorization: Bearer $VESTALOOP_API_KEY`.

**The key implies the scope.** A VestaLoop key is minted for one home (workspace)
and one member, so you never pass workspace or user ids — they're resolved from the
key. Each key is either `read` or `read/write`.

## The workflow

1. `vestaloop_whoami` — validate the key and read its `access` (`read` or
   `write`). Do this first when a request might write.
2. `vestaloop_sync` *(optional)* — pull the member's linked Google calendar into
   VestaLoop so the next list is fresh. Use it before "what's on my calendar?"
   when events might have been added in Google directly. Returns `{ synced }`.
3. `vestaloop_list_events` — list events, optionally bounded by `from`/`to`
   (UTC ISO-8601). Use it to find an event's `id`.
4. `vestaloop_get_event` / `vestaloop_create_event` / `vestaloop_update_event` /
   `vestaloop_delete_event` — act on a specific event by `id`.

## Rules that matter

- **Read vs write.** `create`, `update`, and `delete` need a `read/write` key; with
  a read-only key they return **403**. If you hit that (or `whoami` shows
  `access: "read"`), stop and tell the user to mint a read/write key in the portal
  (API keys tab) — don't retry.
- **Times are UTC ISO-8601** (e.g. `2026-07-28T07:00:00Z`). Users speak local time:
  convert their intent to UTC before sending, and convert back to their local time
  when reporting (and say which timezone). If the timezone is unknown, ask.
- **update replaces the whole event.** `title`, `starts_at`, `ends_at` are always
  required. To change one field, `vestaloop_get_event` first and resend the other
  values unchanged. Prefer `update` over delete-and-recreate.
- **scope is cosmetic.** `personal` (default) vs `shared` (household-wide) is just a
  label for the merged view; it does not change which Google calendar the event
  syncs to.
- **Writes touch the real Google calendar.** create/update/delete also push to the
  member's linked Google calendar. Confirm the exact event (title + time) before a
  delete, and summarize what you created or changed afterward.

## Common tasks

- **"What's on this week?"** → `vestaloop_sync` (if freshness matters) →
  `vestaloop_list_events` with `from`/`to` spanning the week → summarize soonest
  first in the user's local time.
- **"Add dentist Tue 3pm."** → confirm/derive the date, convert to UTC →
  `vestaloop_create_event` with `title`, `starts_at`, `ends_at`
  (default 1h if no end given), `scope` (ask personal vs shared if unclear).
- **"Move it to 4pm."** → `vestaloop_list_events` to find the `id` →
  `vestaloop_get_event` → `vestaloop_update_event` resending everything with the new
  time.
- **"Cancel it."** → find the `id`, confirm the exact event, then
  `vestaloop_delete_event`.

Never reveal the API key or raw tool URLs.
