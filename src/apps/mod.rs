//! The **App SDK** — the agent's "OS" layer for pod-native, in-process apps.
//!
//! Background: the ecosystem apps (notes, calendar, drive) are moving off their
//! cloud Postgres backends and *into the user's pod*, running inside this agent
//! binary and keeping state on the pod's own disk. See the design docs:
//! `docs/POD_NATIVE_APPS_PLAN.md` (options + why "Plan B") and
//! `docs/POD_NATIVE_APPS_B_IMPL_PLAN.md` (the phased build). This module is
//! **Phase 0**: the reusable SDK skeleton, with *no apps wired yet*
//! ([`builtin_apps`] returns empty), so behavior is unchanged.
//!
//! An [`App`] is compiled into the binary (like the native `s3`/`email` tools)
//! and, when enabled, is lent a set of OS "syscalls" via [`AppContext`]:
//!
//! * [`SqliteStore`] — a private SQLite database on the pod disk for
//!   structured/hot state (notes, events, search indexes). One file per app.
//! * [`BlobStore`] — a private object-storage namespace for large/durable bytes
//!   (uploads, attachments, backups). Local-filesystem-backed today; an
//!   S3/Spaces/R2-backed impl arrives with the Drive app.
//! * [`AppEventHub`] — a publish/subscribe hub that drives WebSocket push to the
//!   app's embedded web UI.
//! * [`OwnerIdentity`] — the pod's single owner ("the pod *is* the user").
//! * a private scratch `data_dir` under `<data>/apps/<id>/`.
//!
//! Apps wire into three existing seams without touching the turn loop:
//!   1. **Router** — [`mount_app_routers`] nests each enabled app's routes at
//!      `/apps/<id>` on the Workshop server.
//!   2. **Tools** — [`try_register_app_tool`] lets an app claim a tool name in
//!      the registry fallthrough (`tools::create_registry_for_with_config`).
//!   3. **Manifest** — an app's `id()` matches its integration-pack id, so the
//!      existing enable-state gates it on/off.

use std::path::PathBuf;

use async_trait::async_trait;
use metalcraft::ToolRegistry;

pub mod blobs;
pub mod events;
pub mod storage;

pub use blobs::{BlobStore, LocalBlobStore};
pub use events::AppEventHub;
pub use storage::SqliteStore;

/// Fallible result for app/OS operations. Uses a boxed error so callers can `?`
/// heterogeneous errors (sqlx, io, …) without a shared error crate.
pub type AppResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync + 'static>>;

/// The pod's single owner. In a managed pod there is exactly one user, so an app
/// never needs per-request identity resolution — it trusts the process.
#[derive(Clone, Debug, Default)]
pub struct OwnerIdentity {
    pub user_id: Option<String>,
    pub email: Option<String>,
}

impl OwnerIdentity {
    /// Best-effort owner identity from the environment the pod is booted with.
    pub fn from_env() -> Self {
        Self {
            user_id: std::env::var("METALCRAFT_USER_ID").ok().filter(|s| !s.is_empty()),
            email: std::env::var("METALCRAFT_USER_EMAIL").ok().filter(|s| !s.is_empty()),
        }
    }
}

/// A recurring timer an app registers with the OS scheduler (calendar reminders,
/// backup snapshots). Wired to the daemon scheduler in a later phase; carried
/// here so the trait surface is stable.
#[derive(Clone, Debug)]
pub struct AppSchedule {
    /// Stable id, unique within the app.
    pub id: String,
    /// Fire interval in minutes.
    pub every_minutes: u64,
}

/// The resources the agent-OS lends an installed app.
pub struct AppContext {
    /// Structured/hot state — SQLite on the pod disk.
    pub store: SqliteStore,
    /// Large/durable bytes — object storage (local dir today).
    pub blobs: Box<dyn BlobStore>,
    /// Pub/sub hub for pushing events to the app's web UI.
    pub events: AppEventHub,
    /// The pod's owner ("the pod is the user").
    pub owner: OwnerIdentity,
    /// Private scratch dir under `<data>/apps/<id>/`.
    pub data_dir: PathBuf,
}

/// An installed, in-process app ("agent OS" app). All methods have inert
/// defaults so a minimal app only overrides what it uses.
#[async_trait]
pub trait App: Send + Sync {
    /// Stable id — **must** equal the app's integration-pack id (e.g.
    /// `"metalcraft-notes"`), so enable-state and routing line up.
    fn id(&self) -> &'static str;

    /// Native tool names this app contributes (e.g. `mnote_*`). Empty by default.
    fn tool_names(&self) -> Vec<String> {
        Vec::new()
    }

    /// Register this app's [`metalcraft::Tool`]s into the shared registry, given
    /// its storage. Default: register nothing.
    fn register_tools(&self, reg: ToolRegistry, _ctx: &AppContext) -> ToolRegistry {
        reg
    }

    /// The app's REST + embedded-SPA router, nested at `/apps/<id>`. Default:
    /// empty router.
    fn router(&self, _ctx: &AppContext) -> axum::Router {
        axum::Router::new()
    }

    /// Run once on pod boot: apply schema, seed defaults. Default: no-op.
    async fn init(&self, _ctx: &AppContext) -> AppResult<()> {
        Ok(())
    }

    /// Recurring OS-scheduler timers. Default: none.
    fn schedules(&self) -> Vec<AppSchedule> {
        Vec::new()
    }
}

/// Every app compiled into this binary. **Phase 0: empty** — apps land in later
/// phases (notes, then calendar, then drive). Keeping this the single source of
/// truth means the router/tool/schedule seams are all driven off one list.
pub fn builtin_apps() -> Vec<Box<dyn App>> {
    Vec::new()
}

/// Built-in apps whose integration pack is currently enabled. Enable-state is
/// the "install" switch for a native app (see `docs/POD_NATIVE_APPS_B_IMPL_PLAN.md`
/// Phase 5).
pub fn enabled_builtin_apps() -> Vec<Box<dyn App>> {
    builtin_apps()
        .into_iter()
        .filter(|app| crate::integration_packs::is_enabled(app.id()))
        .collect()
}

/// Build the [`AppContext`] the OS lends `app`. The SQLite pool is created
/// **lazily** (connects on first use), so this is cheap and synchronous; schema
/// migrations run later in [`App::init`].
pub fn ctx_for(app: &dyn App) -> AppResult<AppContext> {
    let data_dir = crate::paths::app_data_dir(app.id());
    std::fs::create_dir_all(&data_dir)?;
    let store = SqliteStore::open(&data_dir.join(format!("{}.db", app.id())))?;
    let blobs: Box<dyn BlobStore> = Box::new(LocalBlobStore::new(data_dir.join("blobs")));
    Ok(AppContext {
        store,
        blobs,
        events: AppEventHub::new(),
        owner: OwnerIdentity::from_env(),
        data_dir,
    })
}

/// Nest every enabled app's router under `/apps/<id>` on the given router.
///
/// No-op while [`builtin_apps`] is empty. NOTE: app routers are mounted on the
/// fully-stated (`Router<()>`) Workshop router; wiring app auth (the pod
/// connection token that lets a browser load an app SPA) lands with the first
/// real app in Phase 1.
pub fn mount_app_routers(mut router: axum::Router) -> axum::Router {
    for app in enabled_builtin_apps() {
        match ctx_for(app.as_ref()) {
            Ok(ctx) => {
                let mount = format!("/apps/{}", app.id());
                router = router.nest(&mount, app.router(&ctx));
                log::info!("mounted app '{}' at {}", app.id(), mount);
            }
            Err(e) => log::error!("failed to build context for app '{}': {e}", app.id()),
        }
    }
    router
}

/// If an enabled app owns the tool `name`, register that app's tools and return
/// `Ok(registry)`; otherwise return `Err(registry)` unchanged so the caller can
/// fall through to other resolution (e.g. declarative HTTP-API tools).
///
/// Used by `tools::create_registry_for_with_config`'s unknown-name fallthrough.
/// No-op while [`builtin_apps`] is empty.
pub fn try_register_app_tool(reg: ToolRegistry, name: &str) -> Result<ToolRegistry, ToolRegistry> {
    for app in enabled_builtin_apps() {
        if app.tool_names().iter().any(|t| t == name) {
            match ctx_for(app.as_ref()) {
                Ok(ctx) => return Ok(app.register_tools(reg, &ctx)),
                Err(e) => {
                    log::error!("failed to build context for app '{}': {e}", app.id());
                    return Err(reg);
                }
            }
        }
    }
    Err(reg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase0_ships_no_apps() {
        // Phase 0 invariant: the SDK compiles and wires in, but no app is live
        // yet, so behavior is unchanged.
        assert!(builtin_apps().is_empty());
        assert!(enabled_builtin_apps().is_empty());
    }

    #[test]
    fn unknown_tool_is_not_claimed_by_any_app() {
        let reg = ToolRegistry::new();
        // No app owns this name → registry returned unchanged via Err.
        assert!(try_register_app_tool(reg, "definitely_not_an_app_tool").is_err());
    }

    #[test]
    fn empty_mount_is_identity() {
        // Mounting with no apps must not panic and must leave routing intact.
        let _router = mount_app_routers(axum::Router::new());
    }
}
