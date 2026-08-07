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
- **Never trust your own sense of "now".** To ground any relative date, call **`mcal_now`**
  (pass the calendar's `tz`). It returns a fresh `date` (today), `tomorrow`, `yesterday`,
  `weekday`, plus `utc`/`local`. Use it before answering "next Friday", "the 15th", etc.,
  and before writing an event from a user-given local time.
- **Reading a day:** prefer `mcal_list_events` with `day` = `today` / `tomorrow` /
  `yesterday` / `YYYY-MM-DD`. The server resolves the day in the calendar's own timezone
  against its live clock, so "what's on tomorrow" is correct with no date math from you.
  For an explicit date from `mcal_now` (e.g. `next Friday`), pass it as `day=YYYY-MM-DD`.
- **Interpreting the user:** a wall-clock time the user gives ("3pm Friday") means that
  time **in the target calendar's timezone**. Get today's local date from `mcal_now`, then
  convert to UTC ISO-8601 before sending it in `from`/`to` or in create/update bodies.
- **Reporting back:** event `starts_at`/`ends_at` come back as UTC ISO-8601 — convert to
  the calendar's timezone when you tell the user.
- **Creating a calendar:** `timezone` is REQUIRED. If you don't know the user's, **ask** —
  never guess.

## Workflow
1. **`mcal_whoami`** — validate the token, read `scopes`.
2. **`mcal_list_calendars`** — find the target calendar's `slug` **and its `timezone`**. If
   the user names one that doesn't exist, list what's there and ask, or offer
   `mcal_create_calendar` (ask for the timezone first).
3. **Ground the date** — for anything relative or any write, call `mcal_now` with the
   calendar's `tz` so you have the real today/tomorrow (don't guess it).
4. **Read:** optionally `mcal_sync` (calendar slug) to pull the linked Google calendar,
   then `mcal_list_events` — use `day=today/tomorrow/YYYY-MM-DD` for a single day, or a
   `from`/`to` window for a range.
5. **Write (needs `write`):** `mcal_create_event`, `mcal_update_event`,
   `mcal_delete_event` — all scoped to a calendar slug.

## Guests & video meetings
The calendar is the scheduling hub — a "meeting" is an event with guests and (optionally) a
video room, the Google model.
- **Add guests** with `mcal_add_guests` (emails, optionally names). Resolve names to emails
  via the **Metalcraft Contacts** pack (`mcon_search`) first — never invent an address.
  Remove with `mcal_remove_guest`. `mcal_get_event` returns the event's `guests` + their rsvp.
- **Add a video call** with `mcal_add_meeting` — this provisions a Metalcraft Meet room for
  the event's guests and puts the `meeting_join_url` on the event (and in its location).
  Remove with `mcal_remove_meeting`.
- **The headline flow** — "set up a call with Alice and Bob Thursday 2pm": `mcon_search` for
  their emails → `mcal_create_event` (title + time; confirm the timezone) → `mcal_add_guests`
  → `mcal_add_meeting` → report the join URL. Confirm the guest list + time with the user
  before adding guests/meeting.

## Invites the user received
The flip side of adding guests: other people can invite *this* user to their events.
- **`mcal_list_invites`** (no args) lists invites the user received — matched to their
  account email — soonest first, with `rsvp`, `organizer_email`, `invited_at` (sort by this
  for the most recent), and `on_my_calendar`. Use it for "do I have any invites?"; it also
  gives you the `event_id` to respond with.
- **`mcal_respond_invite`** (`event_id`, `rsvp`: `accepted`|`declined`) answers one.
  **Accepting copies the event onto the user's own calendar** — a dedicated `Invitations`
  calendar that appears in `mcal_list_calendars`/`mcal_list_events` — so accepted invites
  show up alongside their own events; declining removes that copy. Confirm which invite
  (title + time) before responding.

## Lifecycle notes
- Resolve an event's `id` via `mcal_list_events` before get/update/delete.
- `mcal_update_event` **replaces** all fields — fetch the current event with
  `mcal_get_event` and resend unchanged values when altering just one.
- Deletes/updates affect the linked Google calendar. Confirm the exact event (title +
  time) before deleting; summarize what you changed afterward.
- Never reveal the token or raw tool URLs.
