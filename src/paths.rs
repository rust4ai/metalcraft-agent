use std::path::PathBuf;

const APP_NAME: &str = "metalcraft-agent";

/// Returns the app data root, resolved in order:
/// 1. `METALCRAFT_DATA_DIR` env var (explicit override)
/// 2. OS app data dir via `dirs::data_dir()` (~/.local/share/metalcraft-agent on Linux)
/// 3. `./data` fallback (useful in containers where HOME may not be set)
pub fn data_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("METALCRAFT_DATA_DIR") {
        return PathBuf::from(custom);
    }
    if let Some(os_dir) = dirs::data_dir() {
        return os_dir.join(APP_NAME);
    }
    PathBuf::from("data")
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
