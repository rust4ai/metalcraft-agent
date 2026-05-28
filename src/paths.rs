use std::path::PathBuf;

const APP_NAME: &str = "metalcraft-agent";

/// Returns the app data root: ~/.local/share/metalcraft-agent (Linux), etc.
pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .expect("could not determine data directory for this OS")
        .join(APP_NAME)
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
