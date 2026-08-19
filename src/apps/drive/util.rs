//! Small helpers for the Drive app.

use std::path::{Component, Path, PathBuf};

pub fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Resolve `rel` to a path **inside** the upload-root sandbox (the same jail the
/// multipart/s3 tools use), rejecting absolute paths and `..` traversal — so a
/// tool-calling model can't read/write arbitrary local files.
pub fn jailed_path(rel: &str) -> Result<PathBuf, String> {
    let root = crate::paths::upload_root();
    let candidate = Path::new(rel);
    if candidate.is_absolute() {
        return Err("path must be relative to the upload root".into());
    }
    let mut out = root.clone();
    for comp in candidate.components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            _ => return Err("path must not contain '..' or a root component".into()),
        }
    }
    Ok(out)
}
