# Calendar Event Reminders → APNs (pod-scheduled, no Neon keep-alive)

Send an **APNs push ~1 hour before a calendar event starts**, when an APNs
gateway is configured. The lead time and on/off are a **per-calendar setting**
(default: **on**, **60 min**). Hard constraint: **do not keep the Neon DB alive** —
no per-minute polling of Postgres for "what's starting soon."

The trick: **schedule and fire the reminder from the user's pod, not the cloud.**
The pod is per-user and already running (the daemon), so a per-minute check over
**pod-local** data costs zero Neon hits. APNs delivery reuses the gateway's
existing push backend.

This is **independent of the notes/calendar pod-migration** — it ships without
migrating calendar (Mode B below), and gets simpler once calendar *is* pod-local
(Mode A). Notes-first is unaffected.

---

## 1. What already exists (grounding)

- **Gateway is the APNs sender.** `metalcraft-gateway` has an APNs backend
  (`src/services/backends/apns.rs`, `controllers/send.rs`, `apns_enabled()` gated
  on `APNS_KEY_ID/TEAM_ID/BUNDLE_ID/P8/ENV`), **device-token registration**
  (`/api/v1/device-tokens` register/list/unregister), and push-attempt logging.
  The pod already talks to the gateway (morning-brief already pushes via it).
- **Prior art — the cloud reminder wheel.** `metalcraft-calendar/backend/src/
  services/reminders.rs` already does reminders, but the wrong way for us: it's
  **cloud-side**, **email-only**, **meetings-only** (`WHERE meeting_room_id IS NOT
  NULL`), a **global** `reminder_lead_minutes`, and still uses **Redis + Neon**
  (a Redis ZSET wheel plus a Neon `reconcile`/`fire`, with `reminded_at` as the
  "sent once" backstop). We keep the good ideas (`reminded_at` dedup, boot
  reconcile) and move the engine into the pod, dropping Redis and Neon-polling.
- **The daemon already runs a scheduler loop** (`src/daemon.rs`, the flow
  scheduler) — the natural host for a per-minute reminder tick.
- **Morning-brief precedent:** a pod already pulls the day's events and pushes an
  APNs summary — the same shape, just event-triggered instead of 8am-cron.

---

## 2. Design principle: the pod owns its reminders

Reminders are a **per-user, time-local** concern, and the user's pod is a
**per-user, always-on process**. Put the scheduler there:

- **Zero Neon polling.** The per-minute tick reads **pod-local** upcoming-events
  data (SQLite or a small JSON projection), never Postgres.
- **Neon is touched only on real change.** Creating/editing an event already hits
  Neon; we piggyback on that (or a once-daily safety pull). No keep-alive poller.
- **No Redis.** The wheel is an in-memory/in-pod structure rebuilt from local data
  on boot; we don't need Upstash.
- **Delivery reuses the gateway.** Pod → gateway `send` (platform `apns`) → the
  user's registered device tokens → APNs. Nothing new on the delivery side.
- **Suspended pods = no reminders, correctly.** A lapsed-premium pod scales to 0;
  no process, no reminders. Acceptable and self-consistent.

---

## 3. Where the event data comes from (two modes, same scheduler)

The scheduler is identical in both; only the **event source** differs.

### Mode A — target & **recommended** (calendar events live in pod state)
Events already live in pod SQLite. The tick just queries local rows. **No sync,
no push emitter, no daily pull, no Neon — nothing.** The reminder is a trivial
local query on data the pod already owns. This is the cleanest possible answer.

**Key point:** you do **not** need the full calendar migration to get here. The
calendar splits into an **easy single-user core** (calendars + events + timezone
— per the study, a modest lift) and a **hard cross-tenant part** (external-guest
invites/RSVP/email — the coordinator). **Only the event core needs to move to the
pod to unlock zero-Neon reminders.** The invite coordinator can stay in the cloud
and be tackled later (or never). So "bring calendar events into the pod" is
**separable** from "solve external invites" — the reminder feature is a concrete
reason to do the easy half now, without reopening the hard half.

### Mode B — bridge (ship now; calendar stays in the cloud)
The pod keeps a lightweight **"upcoming events" projection** (next ~48 h) — a
tiny SQLite table or a JSON file under `/data/apps/calendar-reminders/`. It is
kept fresh **without polling Neon**:

- **Primary: push-on-change.** The cloud calendar POSTs
  `{event_id, calendar_id, title, starts_at, location, reminder cfg}` to the
  pod's ingress on create/update/delete (pods are addressable at
  `<slug>.pods.metalcraftai.com`; reuse the gateway push-via-k3 route + a
  connection token). This only fires when the user actually changes an event —
  which already hits Neon anyway, so **no extra DB load**.
- **Safety net: a once-daily pull.** The pod pulls its next-48h window once per
  day (and on boot) via the existing calendar API — ~1–2 Neon reads/day, not a
  poll. Catches anything a missed push dropped.

Mode B requires no calendar migration — just the reminder-config fields, a
push emitter on the cloud side, and the pod scheduler. When calendar later moves
to the pod, delete the sync and switch to Mode A; the scheduler is unchanged.

---

## 4. The scheduler (in the pod daemon)

A new `reminders` loop alongside the flow scheduler in `src/daemon.rs`:

- **Tick (per minute):** for each event in the local projection where
  `reminders.enabled` and `now_utc >= starts_at - lead` and not already fired for
  this `(event_id, starts_at)` → **fire**, then record the marker.
- **Fire:** build the APNs payload and POST to the gateway `send` (platform
  `apns`) for the pod's user; log the attempt. Title = event title; body =
  `"Starts in 1 hour · <local time> · <location>"` (localized with the calendar's
  IANA tz — the pod has it).
- **Dedup / idempotency:** a pod-local `reminded` marker keyed by
  `(event_id, starts_at, lead)` (a row/JSON entry, the `reminded_at` idea but
  local). Survives restarts → never double-sends. Keying on `starts_at` means
  **moving an event's start automatically re-arms** the reminder.
- **Boot reconcile:** on daemon start, rebuild from the local projection; for any
  event whose fire-time passed **within a small grace window** (e.g. ≤10 min) and
  wasn't sent, fire once; skip long-past ones (no spam after downtime).
- **Cancellation:** event deleted/moved out of window → drop from projection; a
  moved start re-arms via the marker key.

**Timezone is a non-issue for the trigger:** `starts_at` is a UTC instant, `lead`
is minutes — pure subtraction, no DST edge. The calendar's tz is used *only* to
render the human-readable time in the push body.

---

## 5. Config model (per-calendar)

Add a small reminder config to each calendar:

```jsonc
"reminders": {
  "enabled": true,        // default ON
  "lead_minutes": 60,     // default 1 hour
  "channel": "apns"       // v1: apns only
}
```

- **Mode A:** stored in the pod calendar store.
- **Mode B:** 2–3 columns on the cloud `calendars` row
  (`reminders_enabled bool default true`, `reminder_lead_minutes int default 60`,
  `reminder_channel text default 'apns'`), **read as part of the sync**, never per
  tick, and cached with the projection. (Replaces today's *global*
  `reminder_lead_minutes` with a per-calendar value.)
- Defaults make it work with zero user action; a user can turn it off or change
  the lead per calendar.
- v1 = one lead per calendar. (Later: per-event override, multiple leads like
  "1 h AND 10 min," snooze/ack.)

---

## 6. "When an APNs gateway is configured" — the activation gate

The feature auto-activates (no error, no noise) only when delivery is actually
possible. The pod checks, cheaply and locally where possible:

1. The pod has a **gateway channel connected** (the built-in `metalcraft`
   channel).
2. At least **one device token is registered** for the user (gateway
   `/api/v1/device-tokens`).
3. Gateway `apns_enabled()` is true (APNs creds present).

If any is false → the scheduler runs but firing is a **graceful no-op** (and can
surface a one-line "reminders inactive: no device registered" status in the
Workshop/mobile UI). Default-on means "on the moment a device is registered and
the gateway is live."

---

## 7. Why this does not keep Neon alive (the whole point)

| Approach | Per-minute Neon? | Extra deps | Works w/o calendar migration |
|---|---|---|---|
| Naive: cloud cron scans Neon each minute | **yes (bad)** | — | yes |
| Existing cloud wheel (`reminders.rs`) | no polling, but Neon on reconcile/fire + Redis sweeping | Redis + cloud svc | yes |
| **This plan (pod-scheduled)** | **no** | none (pod already running) | **yes (Mode B)** |

The per-minute work is **local to the pod**. Neon is touched only when the user
changes an event (which already hits it) or a once-daily safety pull. No poller
keeps it warm. No Redis. And it's **forward-compatible**: Mode A (pod-local
calendar) removes even the sync.

---

## 8. Phasing

- **R0 — Delivery helper.** A pod-side `send_apns(user, title, body, data)` that
  POSTs to the gateway `send` (platform `apns`); confirm against the morning-brief
  path. Add the activation-gate check (§6).
- **R1 — Pod scheduler.** `reminders` loop in the daemon over a local
  upcoming-events projection: tick, fire, dedup marker, boot reconcile. Test with
  a hand-seeded projection (no calendar dependency yet).
- **R2 — Event source (Mode B).** Per-calendar reminder-config columns + API on
  the cloud calendar; a push-on-change emitter (calendar → pod ingress) + a
  once-daily pod pull to populate the projection.
- **R3 — Config UI.** Per-calendar enabled/lead toggle in mobile
  (`metalcraft-mobile`) + Workshop calendar settings.
- **R4 — Mode A (recommended end state).** Migrate the calendar **event core**
  (single-user events + timezone) into pod SQLite — the easy half, no invite
  coordinator required. The scheduler then reads pod rows directly; **delete the
  entire Mode B sync layer** (push emitter, daily pull, projection). No scheduler
  change.

**Two ways to sequence it:**
- *Bridge first:* ship R0–R3 (Mode B) now — reminders work while calendar stays
  cloud — then do R4 when convenient.
- *Core-migration first (cleaner):* do R4's event-core move up front (it's a
  modest lift and is what the reminder feature really wants), and skip building
  the Mode B sync machinery entirely. Recommended if calendar-in-pod is happening
  anyway — you avoid building throwaway sync code.

Either way, the **external-invite coordinator is untouched** — this only concerns
where the user's own events are stored.

---

## 9. Open decisions

- **Event source in Mode B:** push-on-change primary + daily pull (recommended),
  or pull-only (simpler, slightly staler), or push-only (risk of missed events if
  a push is dropped).
- **Projection store:** pod SQLite table vs a JSON file. SQLite is nicer if
  calendar is heading pod-local anyway; JSON matches the agent's current idiom for
  a tiny 48h window.
- **Granularity:** per-calendar only (v1) vs per-event override.
- **Multiple leads / snooze:** out of scope v1; note the marker key already
  supports multiple leads if added.
- **Device targeting:** all of the user's registered devices (default) vs a
  chosen device.
- **Whether the cloud calendar keeps its own email reminders** in parallel during
  the bridge, or APNs fully replaces them.
