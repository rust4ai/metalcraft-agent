//! **Notes** — the first pod-native app (Plan B, Phase 1).
//!
//! Markdown-native notes with color categories, stored in this pod's own SQLite
//! database instead of the cloud `metalcraft-notes` Postgres. The 8 `mnote_*`
//! agent tools keep their exact names and JSON shapes, so the notes persona and
//! skill are unchanged — only the storage moves in-process.
//!
//! This module ships the **agent-facing** surface (native tools + store). The
//! REST + embedded-SPA router (`App::router`) and export/import land in a
//! follow-up so this stays a reviewable slice; `App::init` seeds the schema.

use async_trait::async_trait;
use metalcraft::ToolRegistry;

use super::{App, AppContext, AppResult};

mod models;
mod palette;
mod schema;
mod store;
mod tools;
mod util;

pub use store::NotesStore;

/// The stable app id — must equal the integration-pack id.
pub const APP_ID: &str = "metalcraft-notes";

/// The 8 native tools this app contributes (same names as the pack's api_tools).
pub const TOOL_NAMES: &[&str] = &[
    "mnote_whoami",
    "mnote_list_notes",
    "mnote_get_note",
    "mnote_create_note",
    "mnote_update_note",
    "mnote_delete_note",
    "mnote_list_categories",
    "mnote_create_category",
];

/// A handler error carrying an HTTP-style status, surfaced to the agent in the
/// same `{status, data}` envelope the declarative HTTP tools use.
#[derive(Debug)]
pub struct NotesError {
    pub status: u16,
    pub message: String,
}

impl NotesError {
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

impl From<sqlx::Error> for NotesError {
    fn from(e: sqlx::Error) -> Self {
        NotesError::new(500, format!("database error: {e}"))
    }
}

pub type NotesResult<T> = std::result::Result<T, NotesError>;

/// The Notes app.
pub struct NotesApp;

#[async_trait]
impl App for NotesApp {
    fn id(&self) -> &'static str {
        APP_ID
    }

    fn tool_names(&self) -> Vec<String> {
        TOOL_NAMES.iter().map(|s| s.to_string()).collect()
    }

    fn register_tools(&self, reg: ToolRegistry, ctx: &AppContext) -> ToolRegistry {
        let store = NotesStore::new(ctx.store.pool().clone(), ctx.owner.clone());
        tools::register(reg, store)
    }

    async fn init(&self, ctx: &AppContext) -> AppResult<()> {
        let store = NotesStore::new(ctx.store.pool().clone(), ctx.owner.clone());
        store
            .ensure_ready()
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.message.into() })?;
        Ok(())
    }
}
