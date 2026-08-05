# Metalcraft Meet

Schedule and manage a Metalcraft user's **video meetings**.

These tools call the Metalcraft Meet REST API at `https://meet.metalcraftai.com/api/v1`
using the configured `METALCRAFT_TOKEN`, sent as `Authorization: Bearer $METALCRAFT_TOKEN`.
The base URL is fixed — the only thing to configure is the token. That single token is the
user's **Metalcraft account** credential and works across every ecosystem app.

## The model
- **The token implies the account.** You never pass a user id.
- A **meeting** is addressed by a `slug`. Scheduling mints a `room_id` and an absolute
  `join_url`, and by default:
  - **adds the event to the user's Metalcraft Calendar** (join link in the event location), and
  - **emails the invitees** — each gets a personal join link + an `.ics` calendar attachment.
- **Participants** are emails. External guests (no Metalcraft account) join through a lobby;
  the host lets them in.

## Scopes (read vs write)
`mmeet_whoami` returns the token's `scopes`. Scheduling, updating, cancelling, and adding
participants require **`write`** (else 403). Without it, tell the user to mint a `write`
token at id.metalcraftai.com → Account → Tokens.

## Resolve people first
When the user names people ("schedule a call with **Alice and Bob**"), turn names into email
addresses with the **Metalcraft Contacts** pack (`mcon_search`) before scheduling. **Never
invent an email.** If you can't resolve someone, ask.

## Times
Times are **UTC ISO-8601** on the wire (e.g. `2026-08-06T14:00:00Z`). Always send a
`timezone` (IANA, e.g. `America/New_York`) so the meeting renders correctly.
- A wall-clock time the user gives ("Thursday 2pm") means that time **in their timezone** —
  convert to a UTC `scheduled_start`/`scheduled_end` and pass the `timezone`. If you don't
  know the user's timezone, **ask** — don't guess.
- Omit start/end for an instant ("meet now") meeting.

## Scheduling emails real people — confirm first
`mmeet_schedule_meeting` invites everyone on the list by email. **Before calling it, state
who will be invited and the time (with timezone), and get a yes.** Then schedule, and report
the `join_url` back.

## Workflow
1. **`mmeet_whoami`** — validate the token, read `scopes`.
2. **Resolve people** — `mcon_search` (Contacts) to get invitee emails.
3. **Confirm** — the participant list + time (with timezone) with the user.
4. **`mmeet_schedule_meeting`** — creates the meeting, join URL, calendar event, and invites.
5. **Manage:** `mmeet_list_meetings` (`when=upcoming|past`) → find a `slug`;
   `mmeet_get_meeting`; `mmeet_update_meeting` to reschedule (re-syncs the calendar event);
   `mmeet_add_participants` to invite more; `mmeet_cancel_meeting` to cancel (notifies people
   and removes the calendar event — confirm the exact meeting first).

## Notes
- Prefer `mmeet_update_meeting` over cancel-and-recreate.
- Report the `join_url` after scheduling; confirm the calendar event + invites.
- Never reveal the token or raw tool URLs.
