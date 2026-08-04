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

## Times & timezones
**Every calendar has a `timezone`** (an IANA name like `America/New_York`), returned by
`mcal_list_calendars`. It is the calendar's locale:
- **Reading a day:** prefer `mcal_list_events` with `day` = `today` / `tomorrow` /
  `yesterday` / `YYYY-MM-DD`. The server resolves the day in the calendar's own
  timezone, so "what's on tomorrow" is correct without you doing any UTC math.
- **Interpreting the user:** a wall-clock time the user gives ("3pm Friday") means that
  time **in the target calendar's timezone**. Convert to UTC ISO-8601 before sending it
  in `from`/`to` or in create/update bodies.
- **Reporting back:** event `starts_at`/`ends_at` come back as UTC ISO-8601 — convert to
  the calendar's timezone when you tell the user. The system prompt gives you the current
  UTC time; combine it with the calendar's timezone for anything relative ("next Tuesday").
- **Creating a calendar:** `timezone` is REQUIRED. If you don't know the user's, **ask** —
  never guess.

## Workflow
1. **`mcal_whoami`** — validate the token, read `scopes`.
2. **`mcal_list_calendars`** — find the target calendar's `slug` **and its `timezone`**. If
   the user names one that doesn't exist, list what's there and ask, or offer
   `mcal_create_calendar` (ask for the timezone first).
3. **Read:** optionally `mcal_sync` (calendar slug) to pull the linked Google calendar,
   then `mcal_list_events` — use `day=today/tomorrow/YYYY-MM-DD` for a single day, or a
   `from`/`to` window for a range.
4. **Write (needs `write`):** `mcal_create_event`, `mcal_update_event`,
   `mcal_delete_event` — all scoped to a calendar slug.

## Lifecycle notes
- Resolve an event's `id` via `mcal_list_events` before get/update/delete.
- `mcal_update_event` **replaces** all fields — fetch the current event with
  `mcal_get_event` and resend unchanged values when altering just one.
- Deletes/updates affect the linked Google calendar. Confirm the exact event (title +
  time) before deleting; summarize what you changed afterward.
- Never reveal the token or raw tool URLs.
