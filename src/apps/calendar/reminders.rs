//! The pod-scheduled APNs reminder loop (Plan B / CALENDAR_APNS_REMINDERS_PLAN).
//!
//! Because calendar events live in the pod's own SQLite and the daemon is
//! already running per-user, "remind me ~1h before an event" is a cheap local
//! query on a per-minute tick — **no Neon, no Redis**. Delivery reuses the
//! gateway's APNs backend (`channels::send(kind="apns")`, the morning-brief
//! path). Each fired event is stamped `reminded_at` so it never double-sends.
//!
//! Activation is implicit: it runs only when the `metalcraft-calendar` pack is
//! enabled, and firing is a graceful no-op when no gateway/device is configured
//! (the send just errors and we log + retry within the reminder window).

use std::time::Duration;

use chrono::Utc;
use chrono_tz::Tz;

use super::store::DueReminder;
use super::{CalendarApp, CalendarStore, APP_ID};

/// One reminder pass: fire any due reminders, stamping each that sends. Returns
/// the count fired. Safe to call when the calendar app is disabled (returns 0).
pub async fn run_reminder_tick() -> usize {
    if !crate::integrations::is_enabled(APP_ID) {
        return 0;
    }
    let ctx = match crate::apps::ctx_for(&CalendarApp) {
        Ok(c) => c,
        Err(e) => {
            log::error!("calendar reminders: could not open store: {e}");
            return 0;
        }
    };
    let store = CalendarStore::new(ctx.store.pool().clone(), ctx.owner.clone(), ctx.events.clone());
    if let Err(e) = store.ensure_ready().await {
        log::error!("calendar reminders: ensure_ready failed: {}", e.message);
        return 0;
    }
    let due = match store.due_reminders(Utc::now()).await {
        Ok(d) => d,
        Err(e) => {
            log::error!("calendar reminders: query failed: {}", e.message);
            return 0;
        }
    };
    let mut fired = 0;
    for r in due {
        match fire(&r).await {
            Ok(()) => {
                if let Err(e) = store.mark_reminded(&r.event_id).await {
                    log::error!("calendar reminders: mark_reminded failed: {}", e.message);
                } else {
                    fired += 1;
                }
            }
            // No gateway/device configured, or a transient send error: leave the
            // event unmarked so it retries next tick (within its window).
            Err(e) => log::warn!("calendar reminder for {} not sent: {e}", r.event_id),
        }
    }
    fired
}

/// Send one reminder as an APNs push over the owner's devices via the default
/// (`metalcraft`) gateway channel.
async fn fire(r: &DueReminder) -> Result<(), String> {
    let channel = crate::channels::resolve_channel(None)?; // Err if not configured
    let body = build_body(r);
    crate::channels::send(&channel, "", &body, Some("apns"), None).await.map(|_| ())
}

/// The push text, e.g. `Standup starts in 1 hour (2:00 PM, Room 4)`.
fn build_body(r: &DueReminder) -> String {
    let lead = if r.lead == 60 {
        "1 hour".to_string()
    } else if r.lead % 60 == 0 && r.lead > 0 {
        format!("{} hours", r.lead / 60)
    } else {
        format!("{} minutes", r.lead)
    };
    let local = match r.timezone.parse::<Tz>() {
        Ok(tz) => r.starts_at.with_timezone(&tz).format("%-I:%M %p").to_string(),
        Err(_) => r.starts_at.format("%-I:%M %p UTC").to_string(),
    };
    let loc = r
        .location
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|l| format!(", {l}"))
        .unwrap_or_default();
    format!("{} starts in {lead} ({local}{loc})", r.title.trim())
}

/// Background loop: tick once a minute. Spawned by the daemon.
pub async fn reminders_loop() {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        let fired = run_reminder_tick().await;
        if fired > 0 {
            log::info!("calendar: fired {fired} reminder(s)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_body_formats_lead_and_local_time() {
        let r = DueReminder {
            event_id: "e".into(),
            title: "Standup".into(),
            starts_at: chrono::DateTime::parse_from_rfc3339("2026-08-12T14:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            location: Some("Room 4".into()),
            calendar_name: "Work".into(),
            timezone: "America/New_York".into(),
            lead: 60,
        };
        // 14:00Z is 10:00 AM EDT.
        assert_eq!(build_body(&r), "Standup starts in 1 hour (10:00 AM, Room 4)");
    }

    #[test]
    fn build_body_handles_minutes_and_no_location() {
        let r = DueReminder {
            event_id: "e".into(),
            title: "Call".into(),
            starts_at: chrono::DateTime::parse_from_rfc3339("2026-08-12T14:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            location: None,
            calendar_name: "Personal".into(),
            timezone: "UTC".into(),
            lead: 15,
        };
        assert_eq!(build_body(&r), "Call starts in 15 minutes (2:00 PM)");
    }
}
