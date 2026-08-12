//! [`BlobStore`] — the per-app object-storage tier.
//!
//! Opaque bytes under string keys: uploads, attachments, exports, backups. The
//! trait is deliberately S3-shaped (`put`/`get`/`delete`/`list`) so the
//! filesystem-backed [`LocalBlobStore`] used today (and for self-hosting) can be
//! swapped for an S3/Spaces/R2-backed impl — fronted by a presign broker — when
//! the Drive app lands, without app code changing.
//!
//! Keys are `/`-separated, relative, and sanitized: a key may not be absolute,
//! escape the root via `..`, or contain empty segments.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::AppResult;

/// A namespaced object store for one app.
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Store `bytes` at `key` (overwriting any existing object).
    async fn put(&self, key: &str, bytes: Vec<u8>) -> AppResult<()>;
    /// Fetch the object at `key`, or `None` if it does not exist.
    async fn get(&self, key: &str) -> AppResult<Option<Vec<u8>>>;
    /// Delete the object at `key` (no error if absent).
    async fn delete(&self, key: &str) -> AppResult<()>;
    /// List keys under `prefix` (a `/`-terminated or partial path).
    async fn list(&self, prefix: &str) -> AppResult<Vec<String>>;
}

/// Reject keys that are absolute, empty, or attempt traversal. Returns the
/// key's path segments on success.
fn safe_segments(key: &str) -> AppResult<Vec<&str>> {
    if key.is_empty() {
        return Err("blob key must not be empty".into());
    }
    let mut segs = Vec::new();
    for seg in key.split('/') {
        match seg {
            "" | "." => return Err(format!("invalid blob key: {key:?}").into()),
            ".." => return Err(format!("blob key must not traverse: {key:?}").into()),
            s => segs.push(s),
        }
    }
    Ok(segs)
}

/// A [`BlobStore`] backed by a local directory. Each key maps to a file under
/// `root`. Used in-pod today and for self-hosting; the S3-backed impl arrives
/// with Drive.
pub struct LocalBlobStore {
    root: PathBuf,
}

impl LocalBlobStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn path_for(&self, key: &str) -> AppResult<PathBuf> {
        let mut p = self.root.clone();
        for seg in safe_segments(key)? {
            p.push(seg);
        }
        Ok(p)
    }

    /// Recursively collect keys (relative to `root`, `/`-joined) under `dir`.
    fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> AppResult<()> {
        if !dir.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                Self::collect(root, &path, out)?;
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl BlobStore for LocalBlobStore {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> AppResult<()> {
        let path = self.path_for(key)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, bytes)?;
        Ok(())
    }

    async fn get(&self, key: &str) -> AppResult<Option<Vec<u8>>> {
        let path = self.path_for(key)?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete(&self, key: &str) -> AppResult<()> {
        let path = self.path_for(key)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn list(&self, prefix: &str) -> AppResult<Vec<String>> {
        let mut all = Vec::new();
        Self::collect(&self.root, &self.root, &mut all)?;
        Ok(all.into_iter().filter(|k| k.starts_with(prefix)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_roundtrip_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let bs = LocalBlobStore::new(dir.path().to_path_buf());

        bs.put("notes/a.txt", b"hello".to_vec()).await.unwrap();
        bs.put("notes/sub/b.txt", b"world".to_vec()).await.unwrap();

        assert_eq!(bs.get("notes/a.txt").await.unwrap().as_deref(), Some(&b"hello"[..]));
        assert!(bs.get("notes/missing").await.unwrap().is_none());

        let mut keys = bs.list("notes/").await.unwrap();
        keys.sort();
        assert_eq!(keys, vec!["notes/a.txt".to_string(), "notes/sub/b.txt".to_string()]);

        bs.delete("notes/a.txt").await.unwrap();
        assert!(bs.get("notes/a.txt").await.unwrap().is_none());
        // Deleting an absent key is not an error.
        bs.delete("notes/a.txt").await.unwrap();
    }

    #[tokio::test]
    async fn rejects_traversal_keys() {
        let dir = tempfile::tempdir().unwrap();
        let bs = LocalBlobStore::new(dir.path().to_path_buf());
        assert!(bs.put("../escape", b"x".to_vec()).await.is_err());
        assert!(bs.get("a//b").await.is_err());
        assert!(bs.put("", b"x".to_vec()).await.is_err());
    }
}
