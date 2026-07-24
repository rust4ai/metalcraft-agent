---
description: How to schedule meetings through cal.com — the event-type → slots → booking flow, time zones, the booking uid lifecycle, and the Google Calendar sync
---

# Cal.com Scheduling

These tools call the cal.com v2 REST API (`https://api.cal.com/v2`) authenticated by
a single **`CALCOM_API_KEY`** (a `cal_live_…` key). You never pass the key yourself —
every tool carries it, plus the required per-endpoint `cal-api-version` header.

## The booking flow

Always go event type → slots → book:

1. **`calcom_get_me`** — confirm the key works; note your `username`.
2. **`calcom_list_event_types`** (needs `username`) — pick the event type. Keep its
   numeric **`id`** (the `eventTypeId`) and `lengthInMinutes`.
3. **`calcom_get_available_slots`** — pass `eventTypeId`, a `start`/`end` range
   (UTC ISO-8601), and the attendee's `timeZone`. This returns only genuinely open
   slots — busy times from any calendar connected to cal.com (e.g. Google) are
   already excluded.
4. **`calcom_create_booking`** — book one of those slots. Required: `start` (UTC
   ISO-8601), `eventTypeId`, and the attendee's `name` + `timeZone`; include `email`
   so they get the confirmation. Returns the booking **`uid`**.

## Managing bookings (by uid)

- **`calcom_list_bookings`** — filter by `status` (upcoming/past/cancelled/
  unconfirmed), `attendeeEmail`, or date range.
- **`calcom_get_booking`** — full detail for one `uid`.
- **`calcom_reschedule_booking`** — move a booking to a new `start` (pick a fresh
  open slot first). **Prefer this over cancel-and-rebook** — it keeps the same
  booking and calendar event.
- **`calcom_cancel_booking`** — cancel by `uid` with a `cancellationReason`; removes
  the synced calendar event and notifies attendees. Not undoable.

## Time zones

All API times are **UTC ISO-8601** (e.g. `2026-07-25T15:00:00Z`). Convert the user's
local intent to UTC for `start`/`end`, and always pass the attendee's IANA
`timeZone` (e.g. `America/New_York`) so slots and confirmation emails render in their
local time. When a user says "tomorrow afternoon", resolve it against *their* zone,
not UTC.

## The Google Calendar sync (one-time human setup)

The agent never touches Google directly — cal.com bridges it:

1. In cal.com → **Settings → Connected Calendars**, connect the Google account.
2. Choose that Google Calendar as the **destination calendar** (where new bookings
   are written) and keep it checked for **conflicts**.

After that: bookings the agent creates appear as events in Google Calendar, and
`calcom_get_available_slots` won't offer times you're already busy in Google. The
same works for Outlook/iCloud — anything cal.com can connect.

Scope note: this manages cal.com **bookings**, not arbitrary Google events. It can't
list or edit events that were created directly in Google Calendar (only cal.com's
own bookings and free/busy).

## Discipline

- **Confirm before writing.** Create/cancel/reschedule send real emails and change
  real calendars — state the exact event type, time (in the attendee's zone), and
  attendee, and confirm unless already authorized.
- **Book only returned slots.** Don't invent a time; use one from
  `calcom_get_available_slots` so you don't collide with a busy period.
- **Never** reveal the API key.
