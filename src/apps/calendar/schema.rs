//! Pod-local Calendar schema — the single-user SQLite translation of the cloud
//! `metalcraft-calendar` core (migrations 001/002/009). `owner_user_id` is
//! dropped (one user per pod); Google-sync columns, invites (`event_guests`),
//! and meeting fields are **out of scope** for the pod core (invites are
//! cross-tenant → the coordinator; Google is an external follow-up).
//!
//! Timestamps are canonical UTC ISO strings (see `util::canon`), so TEXT
//! comparison is chronological for range queries.

use sqlx::SqlitePool;

use super::util::{now_iso, uuid};
use super::{CalError, CalResult};

pub const STATEMENTS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS calendars (
       id                    TEXT PRIMARY KEY,
       name                  TEXT NOT NULL,
       slug                  TEXT NOT NULL UNIQUE,
       timezone              TEXT NOT NULL,
       is_default            INTEGER NOT NULL DEFAULT 0,
       -- Per-calendar reminders: default ON, 60 minutes, delivered via APNs.
       reminders_enabled     INTEGER NOT NULL DEFAULT 1,
       reminder_lead_minutes INTEGER NOT NULL DEFAULT 60,
       created_at            TEXT NOT NULL
     )",
    // At most one default calendar (the cloud's `uq_calendars_one_default`).
    "CREATE UNIQUE INDEX IF NOT EXISTS uq_calendars_one_default
       ON calendars (is_default) WHERE is_default = 1",
    "CREATE TABLE IF NOT EXISTS calendar_events (
       id          TEXT PRIMARY KEY,
       calendar_id TEXT NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
       title       TEXT NOT NULL,
       description TEXT,
       location    TEXT,
       starts_at   TEXT NOT NULL,
       ends_at     TEXT NOT NULL,
       all_day     INTEGER NOT NULL DEFAULT 0,
       source      TEXT NOT NULL DEFAULT 'portal',
       status      TEXT NOT NULL DEFAULT 'confirmed',
       -- For accepted-invite mirrors: the organizer's event id (NULL otherwise).
       -- Lets a decline remove exactly the mirror this invite placed.
       origin_event_id TEXT,
       -- One-shot 'starting soon' reminder marker (NULL = not yet sent; cleared
       -- on edit so a moved start re-arms). Drives the pod reminder scheduler.
       reminded_at TEXT,
       created_at  TEXT NOT NULL,
       updated_at  TEXT NOT NULL
     )",
    "CREATE INDEX IF NOT EXISTS idx_events_calendar
       ON calendar_events (calendar_id, starts_at, ends_at)",
    // External-guest invites (C2). The event stays pod-local; the invite/RSVP is
    // coordinated by the cloud relay, and its last-known status is mirrored here.
    "CREATE TABLE IF NOT EXISTS event_guests (
       id           TEXT PRIMARY KEY,
       event_id     TEXT NOT NULL REFERENCES calendar_events(id) ON DELETE CASCADE,
       email        TEXT NOT NULL,
       name         TEXT,
       rsvp         TEXT NOT NULL DEFAULT 'pending',
       invite_token TEXT,
       created_at   TEXT NOT NULL,
       UNIQUE (event_id, email)
     )",
    "CREATE INDEX IF NOT EXISTS idx_guests_event ON event_guests (event_id)",
    "CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL)",
];

pub async fn apply(pool: &SqlitePool) -> CalResult<()> {
    for stmt in STATEMENTS {
        sqlx::query(stmt).execute(pool).await.map_err(CalError::from)?;
    }
    Ok(())
}

/// Seed a default `personal` (UTC) calendar exactly once, so `list_events` and
/// friends always have a home — the pod analogue of the cloud's
/// `ensure_default_calendar`. Guarded by a `meta` marker (never re-added after
/// the user deletes it). UTC is a safe fallback; the user creates tz'd calendars
/// with `create_calendar`.
pub async fn seed_default_once(pool: &SqlitePool) -> CalResult<()> {
    let seeded: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM meta WHERE k = 'seeded'")
        .fetch_one(pool)
        .await?;
    if seeded > 0 {
        return Ok(());
    }
    sqlx::query(
        "INSERT OR IGNORE INTO calendars (id, name, slug, timezone, is_default, created_at)
         VALUES (?, 'Personal', 'personal', 'UTC', 1, ?)",
    )
    .bind(uuid())
    .bind(now_iso())
    .execute(pool)
    .await?;
    sqlx::query("INSERT OR REPLACE INTO meta (k, v) VALUES ('seeded', '1')")
        .execute(pool)
        .await?;
    Ok(())
}
