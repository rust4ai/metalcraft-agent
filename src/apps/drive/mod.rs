//! **Drive** — the file-manager app (Plan B, Phase 4.5) over the OS `blobs`
//! primitive. File **metadata** (folders / files / trash / starred) lives in pod
//! SQLite; file **bytes** live in `blobs` (LocalBlobStore today — filesystem on
//! the pod PVC; an S3/Spaces/R2 impl behind a presign broker is a deployment
//! swap of the trait, per the storage-tiers plan).
//!
//! Backend-only, single-user. Because the pod is a backend (the agent/clients
//! hand it bytes) rather than a browser, Drive uses **direct upload/download**
//! instead of the cloud pack's presign→confirm dance: `mdrv_upload_file` /
//! `mdrv_download_file` replace `mdrv_presign_upload` / `mdrv_confirm_upload`.
//! (App filespaces and public share links are out of scope for v1.)

use async_trait::async_trait;
use metalcraft::ToolRegistry;

use super::{App, AppContext, AppResult};

mod http;
mod models;
mod schema;
mod store;
mod tools;
mod util;

pub use store::DriveStore;

pub const APP_ID: &str = "metalcraft-drive";

pub const TOOL_NAMES: &[&str] = &[
    "mdrv_whoami",
    "mdrv_list_folder",
    "mdrv_create_folder",
    "mdrv_upload_file",
    "mdrv_download_file",
    "mdrv_get_file",
    "mdrv_update_file",
    "mdrv_delete_file",
    "mdrv_list_starred",
    "mdrv_list_trash",
];

#[derive(Debug)]
pub struct DrvError {
    pub status: u16,
    pub message: String,
}

impl DrvError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(404, m)
    }
    pub fn bad_request(m: impl Into<String>) -> Self {
        Self::new(400, m)
    }
}

impl From<sqlx::Error> for DrvError {
    fn from(e: sqlx::Error) -> Self {
        DrvError::new(500, format!("database error: {e}"))
    }
}

pub type DrvResult<T> = std::result::Result<T, DrvError>;

pub struct DriveApp;

#[async_trait]
impl App for DriveApp {
    fn id(&self) -> &'static str {
        APP_ID
    }

    fn tool_names(&self) -> Vec<String> {
        TOOL_NAMES.iter().map(|s| s.to_string()).collect()
    }

    fn register_tools(&self, reg: ToolRegistry, ctx: &AppContext) -> ToolRegistry {
        let store = DriveStore::new(ctx.store.pool().clone(), ctx.owner.clone(), ctx.events.clone(), ctx.blobs.clone());
        tools::register(reg, store)
    }

    fn router(&self, ctx: &AppContext) -> axum::Router {
        let store = DriveStore::new(ctx.store.pool().clone(), ctx.owner.clone(), ctx.events.clone(), ctx.blobs.clone());
        http::router(store)
    }

    async fn init(&self, ctx: &AppContext) -> AppResult<()> {
        let store = DriveStore::new(ctx.store.pool().clone(), ctx.owner.clone(), ctx.events.clone(), ctx.blobs.clone());
        store
            .ensure_ready()
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.message.into() })?;
        Ok(())
    }
}
