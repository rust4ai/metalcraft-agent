//! Row + output shapes. Output JSON mirrors the cloud `metalcraft-calendar` so
//! the `mcal_` pack and external clients see identical shapes (minus the
//! owner/google/invite fields that don't exist in a single-user pod core).

use serde::Serialize;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow)]
pub struct CalendarRow {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub timezone: String,
    pub is_default: i64,
    pub reminders_enabled: i64,
    pub reminder_lead_minutes: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalendarView {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub timezone: String,
    pub is_default: bool,
    pub reminders_enabled: bool,
    pub reminder_lead_minutes: i64,
    pub created_at: String,
}

impl From<CalendarRow> for CalendarView {
    fn from(r: CalendarRow) -> Self {
        CalendarView {
            id: r.id,
            name: r.name,
            slug: r.slug,
            timezone: r.timezone,
            is_default: r.is_default != 0,
            reminders_enabled: r.reminders_enabled != 0,
            reminder_lead_minutes: r.reminder_lead_minutes,
            created_at: r.created_at,
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct EventRow {
    pub id: String,
    pub calendar_id: String,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub starts_at: String,
    pub ends_at: String,
    pub all_day: i64,
    pub source: String,
    pub status: String,
    /// One-shot reminder marker (not serialized to clients).
    pub reminded_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventView {
    pub id: String,
    pub calendar_id: String,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub starts_at: String,
    pub ends_at: String,
    pub all_day: bool,
    pub source: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

impl From<EventRow> for EventView {
    fn from(r: EventRow) -> Self {
        EventView {
            id: r.id,
            calendar_id: r.calendar_id,
            title: r.title,
            description: r.description,
            location: r.location,
            starts_at: r.starts_at,
            ends_at: r.ends_at,
            all_day: r.all_day != 0,
            source: r.source,
            status: r.status,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}
