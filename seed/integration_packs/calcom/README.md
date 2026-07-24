# Cal.com integration pack

Let a metalcraft agent schedule meetings through [cal.com](https://cal.com)'s v2
REST API with a single API key. Because cal.com syncs with a connected destination
calendar, **bookings the agent creates appear in the user's Google Calendar** (and
availability respects Google busy-times) — with no Google credentials and no OAuth.

The pack ships a `calcom-agent` persona, a `calcom-scheduling` skill, and 8 HTTP
tools: `calcom_get_me`, `calcom_list_event_types`, `calcom_get_available_slots`,
`calcom_create_booking`, `calcom_list_bookings`, `calcom_get_booking`,
`calcom_cancel_booking`, `calcom_reschedule_booking`. Read-only tools auto-approve;
create/cancel/reschedule require approval (they send real emails).

## What it can (and can't) do

- ✅ List event types, check open slots, and create/cancel/reschedule bookings —
  which sync into the connected Google Calendar.
- ✅ Availability that already excludes your Google busy-times.
- ❌ It does **not** read or edit arbitrary pre-existing Google Calendar events
  (birthdays, meetings made directly in Google). It manages cal.com bookings and
  free/busy only. For full Google Calendar CRUD you'd use a unified-calendar API
  (e.g. Nylas) instead.

## Setup

### 1. Get a cal.com API key

In cal.com → **Settings → Security → API Keys** (or **Developer → API Keys**),
create a key. It looks like `cal_live_…`. Store it as the key **`CALCOM_API_KEY`**
(workshop key store, or exported in the environment). The pack's `requires_env`
surfaces it in the key-store UI once the pack is enabled.

### 2. Connect Google Calendar (so bookings sync)

In cal.com → **Settings → Connected Calendars**:

1. Connect the Google account.
2. Set that Google Calendar as the **destination calendar** — new bookings are
   written there.
3. Keep it enabled for **conflict checking** so busy Google times are excluded from
   availability.

The same flow works for Outlook/iCloud.

## Enable and use

1. Enable the pack (workshop Integrations UI, or the `pack_enable` meta tool). It
   ships **disabled** by default.
2. Make sure `CALCOM_API_KEY` is set and Google is connected as the destination.
3. Run the persona, e.g.:

   ```bash
   metalcraft-agent -p calcom-agent \
     "what event types do I have, and what's open tomorrow afternoon?"

   metalcraft-agent -p calcom-agent \
     "book my 30-min intro at the first open slot tomorrow for alex@example.com"
   ```

   The booking will also appear in the connected Google Calendar.

## Notes

- **Times are UTC ISO-8601** in the API; the agent converts from the attendee's
  time zone and passes their IANA `timeZone`.
- **`cal-api-version`** is pinned per endpoint (bookings `2024-08-13`, slots
  `2024-09-04`); bump these if cal.com's docs move.
- Rate limits / errors are returned as cal.com JSON; the agent surfaces them.
- **Never** commit or paste the API key; regenerate it in cal.com if it leaks.
