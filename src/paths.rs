use std::path::PathBuf;
use std::sync::OnceLock;

const APP_NAME: &str = "metalcraft-agent";

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Returns the app data root, resolved **once per process** (memoized) so every
/// subsystem — seeding, the flow runtime, the workshop API, the event listener
/// — agrees on a single location. Resolved in order:
/// 1. `METALCRAFT_DATA_DIR` env var (explicit override)
/// 2. OS app data dir via `dirs::data_dir()` (~/.local/share/metalcraft-agent on Linux)
/// 3. `./data` fallback (useful in containers where HOME may not be set)
///
/// IMPORTANT: load your `.env` (`dotenvy::dotenv()`) before the first call, or a
/// file-provided `METALCRAFT_DATA_DIR` won't be seen and seeding will land in
/// the fallback dir while the runtime reads from the override. The binaries'
/// `main()` load dotenv as their first statement for exactly this reason.
pub fn data_dir() -> PathBuf {
    DATA_DIR
        .get_or_init(|| {
            let (dir, source) = if let Ok(custom) = std::env::var("METALCRAFT_DATA_DIR") {
                (PathBuf::from(custom), "METALCRAFT_DATA_DIR")
            } else if let Some(os_dir) = dirs::data_dir() {
                (os_dir.join(APP_NAME), "OS data dir")
            } else {
                (PathBuf::from("data"), "./data fallback")
            };
            log::info!("metalcraft data dir: {} (via {source})", dir.display());
            dir
        })
        .clone()
}

pub fn personas_dir() -> PathBuf {
    data_dir().join("personas")
}

pub fn skills_dir() -> PathBuf {
    data_dir().join("skills")
}

pub fn flows_dir() -> PathBuf {
    data_dir().join("flows")
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

pub fn api_tools_dir() -> PathBuf {
    data_dir().join("api_tools")
}

pub fn flow_templates_dir() -> PathBuf {
    data_dir().join("flow_templates")
}

pub fn chats_dir() -> PathBuf {
    data_dir().join("chats")
}

pub fn integration_packs_dir() -> PathBuf {
    data_dir().join("integration_packs")
}

pub fn integration_packs_state_file() -> PathBuf {
    data_dir().join("integration_packs.json")
}
