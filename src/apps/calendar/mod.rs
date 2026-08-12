//! **Calendar** — the second pod-native app (Plan B, Phase 4).
//!
//! Calendars + events + timezone logic on this pod's own SQLite, instead of the
//! cloud `metalcraft-calendar` Postgres. The 9 core `mcal_*` tools keep their
//! names/shapes, so the calendar persona/skill are unchanged.
//!
//! Backend-only (no pod UI). **Out of scope for this pod core** (single-user):
//! external-guest invites/RSVP + email (`mcal_add_guests`/`_list_invites`/
//! `_respond_invite`) — cross-tenant, they belong with the coordinator; Google
//! sync (`mcal_sync`) and meetings (`mcal_add_meeting`) — external follow-ups.
//! Those tools stay served by the pack's declarative HTTP defs until then. The
//! APNs reminder scheduler lands in a follow-up.

use async_trait::async_trait;
use metalcraft::ToolRegistry;

use super::{App, AppContext, AppResult};

mod http;
mod models;
mod schema;
mod store;
mod tools;
mod tz;
mod util;

pub use store::CalendarStore;

pub const APP_ID: &str = "metalcraft-calendar";

/// The core (pod-local) tools this app serves natively. The invite/meeting/sync
/// tools are intentionally omitted (see module docs) and fall through to the
/// pack's declarative HTTP tools.
pub const TOOL_NAMES: &[&str] = &[
    "mcal_whoami",
    "mcal_now",
    "mcal_list_calendars",
    "mcal_create_calendar",
    "mcal_list_events",
    "mcal_get_event",
    "mcal_create_event",
    "mcal_update_event",
    "mcal_delete_event",
];

/// A handler error with an HTTP-style status, surfaced in the `{status, data}`
/// envelope for tools and via `IntoResponse` for REST.
#[derive(Debug)]
pub struct CalError {
    pub status: u16,
    pub message: String,
}

impl CalError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(404, m)
    }
    pub fn bad_request(m: impl Into<String>) -> Self {
        Self::new(400, m)
    }
    pub fn conflict(m: impl Into<String>) -> Self {
        Self::new(409, m)
    }
}

impl From<sqlx::Error> for CalError {
    fn from(e: sqlx::Error) -> Self {
        CalError::new(500, format!("database error: {e}"))
    }
}

pub type CalResult<T> = std::result::Result<T, CalError>;

pub struct CalendarApp;

#[async_trait]
impl App for CalendarApp {
    fn id(&self) -> &'static str {
        APP_ID
    }

    fn tool_names(&self) -> Vec<String> {
        TOOL_NAMES.iter().map(|s| s.to_string()).collect()
    }

    fn register_tools(&self, reg: ToolRegistry, ctx: &AppContext) -> ToolRegistry {
        let store = CalendarStore::new(ctx.store.pool().clone(), ctx.owner.clone(), ctx.events.clone());
        tools::register(reg, store)
    }

    fn router(&self, ctx: &AppContext) -> axum::Router {
        let store = CalendarStore::new(ctx.store.pool().clone(), ctx.owner.clone(), ctx.events.clone());
        http::router(store)
    }

    async fn init(&self, ctx: &AppContext) -> AppResult<()> {
        let store = CalendarStore::new(ctx.store.pool().clone(), ctx.owner.clone(), ctx.events.clone());
        store
            .ensure_ready()
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.message.into() })?;
        Ok(())
    }
}
