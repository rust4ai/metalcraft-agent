//! Data-access for the Calendar core, ported from the cloud
//! `services/events.rs` + `controllers/api_v1.rs` to `sqlx`/SQLite. Single-user
//! (owner implicit); Google/invite/meeting paths are dropped. Timestamps are
//! canonical UTC ISO strings so range queries work over TEXT.

use serde_json::json;
use sqlx::SqlitePool;

use super::models::*;
use super::tz;
use super::util::{canon, now_iso, parse_ts, slugify, uuid};
use super::{CalError, CalResult};
use crate::apps::{AppEventHub, OwnerIdentity};

/// A create/update payload (times are raw ISO strings from the caller).
pub struct EventInput<'a> {
    pub title: &'a str,
    pub starts_at: &'a str,
    pub ends_at: &'a str,
    pub all_day: bool,
    pub description: Option<&'a str>,
    pub location: Option<&'a str>,
}

/// Joined row backing the reminder query (event + its calendar's settings).
#[derive(sqlx::FromRow)]
struct DueRow {
    event_id: String,
    title: String,
    starts_at: String,
    location: Option<String>,
    calendar_name: String,
    timezone: String,
    lead: i64,
}

/// An event whose reminder is due to fire.
pub struct DueReminder {
    pub event_id: String,
    pub title: String,
    pub starts_at: chrono::DateTime<chrono::Utc>,
    pub location: Option<String>,
    pub calendar_name: String,
    pub timezone: String,
    pub lead: i64,
}

#[derive(Clone)]
pub struct CalendarStore {
    pool: SqlitePool,
    owner: OwnerIdentity,
    events: AppEventHub,
}

impl CalendarStore {
    pub fn new(pool: SqlitePool, owner: OwnerIdentity, events: AppEventHub) -> Self {
        Self { pool, owner, events }
    }

    pub fn events(&self) -> &AppEventHub {
        &self.events
    }

    pub async fn ensure_ready(&self) -> CalResult<()> {
        super::schema::apply(&self.pool).await?;
        super::schema::seed_default_once(&self.pool).await?;
        Ok(())
    }

    pub fn whoami(&self) -> serde_json::Value {
        json!({
            "sub": self.owner.user_id.clone().unwrap_or_else(|| "owner".to_string()),
            "email": self.owner.email,
            "scopes": serde_json::Value::Null,
        })
    }

    fn publish_event(&self, view: &EventView) {
        if let Ok(v) = serde_json::to_value(view) {
            self.events.publish(json!({ "type": "event.upserted", "event": v }));
        }
    }
    fn publish_event_deleted(&self, id: &str, calendar_id: &str) {
        self.events.publish(json!({ "type": "event.deleted", "id": id, "calendar_id": calendar_id }));
    }
    fn publish_calendar(&self, view: &CalendarView) {
        if let Ok(v) = serde_json::to_value(view) {
            self.events.publish(json!({ "type": "calendar.upserted", "calendar": v }));
        }
    }

    // ── calendars ────────────────────────────────────────────────────────────

    pub async fn list_calendars(&self) -> CalResult<Vec<CalendarView>> {
        let rows = sqlx::query_as::<_, CalendarRow>(
            "SELECT * FROM calendars ORDER BY is_default DESC, created_at",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(CalendarView::from).collect())
    }

    async fn resolve(&self, slug: &str) -> CalResult<CalendarRow> {
        sqlx::query_as::<_, CalendarRow>("SELECT * FROM calendars WHERE slug = ?")
            .bind(slug)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| CalError::not_found("calendar not found"))
    }

    async fn free_slug(&self, base: &str) -> CalResult<String> {
        for n in 1..=200 {
            let cand = if n == 1 { base.to_string() } else { format!("{base}-{n}") };
            let taken: i64 =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM calendars WHERE slug = ?)")
                    .bind(&cand)
                    .fetch_one(&self.pool)
                    .await?;
            if taken == 0 {
                return Ok(cand);
            }
        }
        Err(CalError::conflict("could not allocate a unique slug"))
    }

    pub async fn create_calendar(
        &self,
        name: &str,
        timezone: &str,
        slug: Option<&str>,
    ) -> CalResult<CalendarView> {
        let name = name.trim();
        if name.is_empty() {
            return Err(CalError::bad_request("name is required"));
        }
        tz::validate_tz(timezone)?; // 400 on blank/unknown
        let base = slug
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(slugify)
            .unwrap_or_else(|| slugify(name));
        let slug = self.free_slug(&base).await?;
        // The first calendar becomes the default.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM calendars")
            .fetch_one(&self.pool)
            .await?;
        let is_default = if count == 0 { 1 } else { 0 };
        let row = sqlx::query_as::<_, CalendarRow>(
            "INSERT INTO calendars (id, name, slug, timezone, is_default, created_at)
             VALUES (?, ?, ?, ?, ?, ?) RETURNING *",
        )
        .bind(uuid())
        .bind(name)
        .bind(&slug)
        .bind(timezone.trim())
        .bind(is_default)
        .bind(now_iso())
        .fetch_one(&self.pool)
        .await?;
        let view = CalendarView::from(row);
        self.publish_calendar(&view);
        Ok(view)
    }

    // ── events ───────────────────────────────────────────────────────────────

    /// List a calendar's events, filtered by an optional `day` (resolved in the
    /// calendar's tz) which overrides `from`/`to` (UTC ISO bounds).
    pub async fn list_events(
        &self,
        slug: &str,
        day: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
    ) -> CalResult<Vec<EventView>> {
        let cal = self.resolve(slug).await?;
        let (from_c, to_c): (Option<String>, Option<String>) =
            if let Some(day) = day.map(str::trim).filter(|s| !s.is_empty()) {
                let (f, t) = tz::day_window(day, &cal.timezone)?;
                (Some(canon(f)), Some(canon(t)))
            } else {
                let f = match from.map(str::trim).filter(|s| !s.is_empty()) {
                    Some(s) => Some(canon(parse_ts(s, "from")?)),
                    None => None,
                };
                let t = match to.map(str::trim).filter(|s| !s.is_empty()) {
                    Some(s) => Some(canon(parse_ts(s, "to")?)),
                    None => None,
                };
                (f, t)
            };
        // NULL bound → no filter on that side. `from` is bound twice, `to` twice.
        let rows = sqlx::query_as::<_, EventRow>(
            "SELECT * FROM calendar_events
             WHERE calendar_id = ?
               AND (? IS NULL OR ends_at   >= ?)
               AND (? IS NULL OR starts_at <= ?)
             ORDER BY starts_at",
        )
        .bind(&cal.id)
        .bind(&from_c)
        .bind(&from_c)
        .bind(&to_c)
        .bind(&to_c)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(EventView::from).collect())
    }

    pub async fn get_event(&self, slug: &str, id: &str) -> CalResult<EventView> {
        let cal = self.resolve(slug).await?;
        let row = sqlx::query_as::<_, EventRow>(
            "SELECT * FROM calendar_events WHERE id = ? AND calendar_id = ?",
        )
        .bind(id)
        .bind(&cal.id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| CalError::not_found("event not found"))?;
        Ok(EventView::from(row))
    }

    pub async fn create_event(&self, slug: &str, input: EventInput<'_>) -> CalResult<EventView> {
        let cal = self.resolve(slug).await?;
        let (title, starts, ends) = self.validate_event(&input)?;
        let now = now_iso();
        let row = sqlx::query_as::<_, EventRow>(
            "INSERT INTO calendar_events
               (id, calendar_id, title, description, location, starts_at, ends_at, all_day,
                source, status, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'portal', 'confirmed', ?, ?) RETURNING *",
        )
        .bind(uuid())
        .bind(&cal.id)
        .bind(&title)
        .bind(input.description)
        .bind(input.location)
        .bind(&starts)
        .bind(&ends)
        .bind(input.all_day as i64)
        .bind(&now)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        let view = EventView::from(row);
        self.publish_event(&view);
        Ok(view)
    }

    /// Full replace (matches the cloud PATCH: title/starts/ends required).
    pub async fn update_event(
        &self,
        slug: &str,
        id: &str,
        input: EventInput<'_>,
    ) -> CalResult<EventView> {
        let cal = self.resolve(slug).await?;
        // 404 if it isn't in this calendar.
        self.get_event(slug, id).await?;
        let (title, starts, ends) = self.validate_event(&input)?;
        let row = sqlx::query_as::<_, EventRow>(
            // Clear reminded_at so a moved start re-arms the reminder.
            "UPDATE calendar_events
             SET title = ?, description = ?, location = ?, starts_at = ?, ends_at = ?,
                 all_day = ?, reminded_at = NULL, updated_at = ?
             WHERE id = ? AND calendar_id = ? RETURNING *",
        )
        .bind(&title)
        .bind(input.description)
        .bind(input.location)
        .bind(&starts)
        .bind(&ends)
        .bind(input.all_day as i64)
        .bind(now_iso())
        .bind(id)
        .bind(&cal.id)
        .fetch_one(&self.pool)
        .await?;
        let view = EventView::from(row);
        self.publish_event(&view);
        Ok(view)
    }

    pub async fn delete_event(&self, slug: &str, id: &str) -> CalResult<()> {
        let cal = self.resolve(slug).await?;
        self.get_event(slug, id).await?; // 404 if absent
        sqlx::query("DELETE FROM calendar_events WHERE id = ? AND calendar_id = ?")
            .bind(id)
            .bind(&cal.id)
            .execute(&self.pool)
            .await?;
        self.publish_event_deleted(id, &cal.id);
        Ok(())
    }

    // ── reminders ────────────────────────────────────────────────────────────

    /// Update a calendar's mutable settings (any `Some` field). Used by the REST
    /// PATCH; `enabled`/`lead` drive the reminder scheduler.
    pub async fn update_calendar(
        &self,
        slug: &str,
        name: Option<&str>,
        timezone: Option<&str>,
        reminders_enabled: Option<bool>,
        reminder_lead_minutes: Option<i64>,
    ) -> CalResult<CalendarView> {
        self.resolve(slug).await?; // 404 if absent
        let name = name.map(str::trim).filter(|s| !s.is_empty());
        let tz = match timezone.map(str::trim).filter(|s| !s.is_empty()) {
            Some(t) => {
                tz::validate_tz(t)?;
                Some(t)
            }
            None => None,
        };
        if let Some(l) = reminder_lead_minutes {
            if !(1..=10_080).contains(&l) {
                return Err(CalError::bad_request("reminder_lead_minutes must be 1..=10080"));
            }
        }
        let row = sqlx::query_as::<_, CalendarRow>(
            "UPDATE calendars SET
               name = COALESCE(?, name),
               timezone = COALESCE(?, timezone),
               reminders_enabled = COALESCE(?, reminders_enabled),
               reminder_lead_minutes = COALESCE(?, reminder_lead_minutes)
             WHERE slug = ? RETURNING *",
        )
        .bind(name)
        .bind(tz)
        .bind(reminders_enabled.map(|b| b as i64))
        .bind(reminder_lead_minutes)
        .bind(slug)
        .fetch_one(&self.pool)
        .await?;
        let view = CalendarView::from(row);
        self.publish_calendar(&view);
        Ok(view)
    }

    /// Events whose reminder is due at `now`: reminders enabled, not yet sent,
    /// still upcoming, and `now >= starts_at - lead`. The final lead check is in
    /// Rust (per-calendar lead over canonical-ISO TEXT).
    pub async fn due_reminders(&self, now: chrono::DateTime<chrono::Utc>) -> CalResult<Vec<DueReminder>> {
        let now_c = canon(now);
        let rows = sqlx::query_as::<_, DueRow>(
            "SELECT e.id AS event_id, e.title AS title, e.starts_at AS starts_at,
                    e.location AS location, c.name AS calendar_name, c.timezone AS timezone,
                    c.reminder_lead_minutes AS lead
             FROM calendar_events e JOIN calendars c ON c.id = e.calendar_id
             WHERE c.reminders_enabled = 1
               AND e.reminded_at IS NULL
               AND e.starts_at > ?
             ORDER BY e.starts_at",
        )
        .bind(&now_c)
        .fetch_all(&self.pool)
        .await?;

        let mut due = Vec::new();
        for r in rows {
            let Ok(starts) = chrono::DateTime::parse_from_rfc3339(&r.starts_at) else { continue };
            let starts = starts.with_timezone(&chrono::Utc);
            let reminder_time = starts - chrono::Duration::minutes(r.lead.max(0));
            if now >= reminder_time {
                due.push(DueReminder {
                    event_id: r.event_id,
                    title: r.title,
                    starts_at: starts,
                    location: r.location,
                    calendar_name: r.calendar_name,
                    timezone: r.timezone,
                    lead: r.lead,
                });
            }
        }
        Ok(due)
    }

    /// Stamp an event as reminded so it never fires twice.
    pub async fn mark_reminded(&self, event_id: &str) -> CalResult<()> {
        sqlx::query("UPDATE calendar_events SET reminded_at = ? WHERE id = ?")
            .bind(now_iso())
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Validate + canonicalize an event payload → (title, starts_canon, ends_canon).
    fn validate_event(&self, input: &EventInput<'_>) -> CalResult<(String, String, String)> {
        let title = input.title.trim();
        if title.is_empty() {
            return Err(CalError::bad_request("title is required"));
        }
        let starts = parse_ts(input.starts_at, "starts_at")?;
        let ends = parse_ts(input.ends_at, "ends_at")?;
        if ends < starts {
            return Err(CalError::bad_request("ends_at must be on or after starts_at"));
        }
        Ok((title.to_string(), canon(starts), canon(ends)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> CalendarStore {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let s = CalendarStore::new(pool, OwnerIdentity::default(), AppEventHub::new());
        s.ensure_ready().await.unwrap();
        s
    }

    fn ev<'a>(title: &'a str, s: &'a str, e: &'a str) -> EventInput<'a> {
        EventInput { title, starts_at: s, ends_at: e, all_day: false, description: None, location: None }
    }

    #[tokio::test]
    async fn seeds_default_calendar_once() {
        let s = store().await;
        let cals = s.list_calendars().await.unwrap();
        assert_eq!(cals.len(), 1);
        assert_eq!(cals[0].slug, "personal");
        assert!(cals[0].is_default);
        s.ensure_ready().await.unwrap();
        assert_eq!(s.list_calendars().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn create_calendar_validates_tz_and_defaults() {
        let s = store().await;
        assert!(s.create_calendar("Bad", "Nowhere/Void", None).await.is_err());
        let work = s.create_calendar("Work Cal", "America/New_York", None).await.unwrap();
        assert_eq!(work.slug, "work-cal");
        assert!(!work.is_default); // personal already default
    }

    #[tokio::test]
    async fn event_crud_and_validation() {
        let s = store().await;
        assert!(s.create_event("personal", ev("", "2026-08-12T14:00:00Z", "2026-08-12T15:00:00Z")).await.is_err());
        assert!(s.create_event("personal", ev("Bad", "2026-08-12T15:00:00Z", "2026-08-12T14:00:00Z")).await.is_err());

        let e = s.create_event("personal", ev("Standup", "2026-08-12T14:00:00Z", "2026-08-12T14:30:00Z")).await.unwrap();
        assert_eq!(e.starts_at, "2026-08-12T14:00:00.000Z"); // canonicalized
        assert_eq!(e.status, "confirmed");

        let got = s.get_event("personal", &e.id).await.unwrap();
        assert_eq!(got.title, "Standup");

        let up = s.update_event("personal", &e.id, ev("Standup v2", "2026-08-12T14:00:00Z", "2026-08-12T15:00:00Z")).await.unwrap();
        assert_eq!(up.title, "Standup v2");

        s.delete_event("personal", &e.id).await.unwrap();
        assert!(s.get_event("personal", &e.id).await.is_err());
    }

    #[tokio::test]
    async fn list_events_by_day_uses_calendar_tz() {
        let s = store().await;
        let ny = s.create_calendar("NY", "America/New_York", None).await.unwrap();
        // 2026-08-12 03:00Z is still Aug 11 in New York (23:00 EDT) → not in the 12th.
        s.create_event(&ny.slug, ev("late", "2026-08-12T03:00:00Z", "2026-08-12T03:30:00Z")).await.unwrap();
        // 2026-08-12 14:00Z is Aug 12 10:00 EDT → in the 12th.
        s.create_event(&ny.slug, ev("mid", "2026-08-12T14:00:00Z", "2026-08-12T14:30:00Z")).await.unwrap();
        let day = s.list_events(&ny.slug, Some("2026-08-12"), None, None).await.unwrap();
        let titles: Vec<&str> = day.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["mid"]);
    }

    #[tokio::test]
    async fn due_reminders_respect_lead_enabled_and_marker() {
        let s = store().await; // personal: reminders on, lead 60
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-12T13:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        // 14:00 event, lead 60 → reminder_time 13:00 ≤ now → due.
        let soon = s.create_event("personal", ev("Soon", "2026-08-12T14:00:00Z", "2026-08-12T14:30:00Z")).await.unwrap();
        // 20:00 event → reminder_time 19:00 > now → not due yet.
        s.create_event("personal", ev("Later", "2026-08-12T20:00:00Z", "2026-08-12T20:30:00Z")).await.unwrap();

        let due = s.due_reminders(now).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].title, "Soon");

        // Marking it removes it from the due set (no double-send).
        s.mark_reminded(&soon.id).await.unwrap();
        assert!(s.due_reminders(now).await.unwrap().is_empty());

        // Editing the start clears the marker (re-arms).
        s.update_event("personal", &soon.id, ev("Soon", "2026-08-12T14:00:00Z", "2026-08-12T14:45:00Z")).await.unwrap();
        assert_eq!(s.due_reminders(now).await.unwrap().len(), 1);

        // Disabling reminders on the calendar drops it.
        s.update_calendar("personal", None, None, Some(false), None).await.unwrap();
        assert!(s.due_reminders(now).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn event_mutations_broadcast() {
        let s = store().await;
        let mut rx = s.events().subscribe();
        let e = s.create_event("personal", ev("X", "2026-08-12T14:00:00Z", "2026-08-12T15:00:00Z")).await.unwrap();
        assert_eq!(rx.recv().await.unwrap()["type"], "event.upserted");
        s.delete_event("personal", &e.id).await.unwrap();
        assert_eq!(rx.recv().await.unwrap()["type"], "event.deleted");
    }
}
