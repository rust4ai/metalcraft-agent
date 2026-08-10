//! `metalcraft.lock` — the pod's reproducible install manifest.
//!
//! For every pack and flow the agent installed from a registry, the lockfile records
//! the exact `{ name, version, content_sha256, source }` it resolved to. Its purpose is
//! **reproducibility across pod rebuilds and clones**: a fresh or migrated pod can replay
//! the lock (see the workshop API's `/lockfile/restore`) to reinstall the identical
//! versions — verified by content hash — instead of drifting to whatever is newest.
//!
//! It is written as a side effect of install/uninstall, so the file always reflects the
//! current set. Reads are lock-free (atomic rename); writes serialize through an advisory
//! file lock, mirroring [`crate::integration_packs`]'s state handling, because the daemon
//! and the workshop API can both mutate it.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::paths;

/// One pinned artifact in the lockfile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LockEntry {
    /// Registry slug (== pack id / flow id).
    pub name: String,
    /// The concrete version installed.
    pub version: String,
    /// Integrity hash of the installed content (verified on restore).
    pub content_sha256: String,
    /// Registry origin the artifact came from (e.g. `https://packs.metalcraftai.com`).
    pub source: String,
}

/// The lockfile document.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Lock {
    /// Lockfile format version (not the artifacts' versions).
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub packs: Vec<LockEntry>,
    #[serde(default)]
    pub flows: Vec<LockEntry>,
}

fn one() -> u32 {
    1
}

impl Default for Lock {
    fn default() -> Self {
        Lock { version: 1, packs: Vec::new(), flows: Vec::new() }
    }
}

fn lock_file_path() -> PathBuf {
    paths::data_dir().join("metalcraft.lock")
}

/// Read the lockfile, defaulting to empty when absent or malformed.
pub fn load() -> Lock {
    match std::fs::read_to_string(lock_file_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            log::warn!("metalcraft.lock is malformed, ignoring: {e}");
            Lock::default()
        }),
        Err(_) => Lock::default(),
    }
}

fn save(lock: &Lock) -> std::io::Result<()> {
    let path = lock_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(lock).map_err(std::io::Error::other)?;
    // Atomic replace so a concurrent reader never sees a half-written file.
    let tmp = path.with_extension("lock.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut f, json.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Serialize a read-modify-write of the lockfile across threads and processes with an
/// advisory lock on a sidecar file, mirroring `integration_packs::mutate_state`.
fn mutate(f: impl FnOnce(&mut Lock)) -> Result<(), String> {
    let lock_path = paths::data_dir().join("metalcraft.lock.guard");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("failed to create data dir: {e}"))?;
    }
    let guard = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("failed to open lock guard: {e}"))?;
    guard.lock().map_err(|e| format!("failed to lock: {e}"))?;
    let mut doc = load();
    f(&mut doc);
    let result = save(&doc).map_err(|e| format!("failed to write lockfile: {e}"));
    let _ = guard.unlock();
    result
}

/// Insert-or-replace `entry` in `list`, keyed by name, keeping the list sorted.
fn upsert(list: &mut Vec<LockEntry>, entry: LockEntry) {
    if let Some(existing) = list.iter_mut().find(|e| e.name == entry.name) {
        *existing = entry;
    } else {
        list.push(entry);
    }
    list.sort_by(|a, b| a.name.cmp(&b.name));
}

/// Record (or update) a pinned pack in the lockfile.
pub fn record_pack(name: &str, version: &str, content_sha256: &str, source: &str) -> Result<(), String> {
    mutate(|doc| {
        upsert(
            &mut doc.packs,
            LockEntry {
                name: name.to_string(),
                version: version.to_string(),
                content_sha256: content_sha256.to_string(),
                source: source.to_string(),
            },
        )
    })
}

/// Record (or update) a pinned flow in the lockfile.
pub fn record_flow(name: &str, version: &str, content_sha256: &str, source: &str) -> Result<(), String> {
    mutate(|doc| {
        upsert(
            &mut doc.flows,
            LockEntry {
                name: name.to_string(),
                version: version.to_string(),
                content_sha256: content_sha256.to_string(),
                source: source.to_string(),
            },
        )
    })
}

pub fn remove_pack(name: &str) -> Result<(), String> {
    mutate(|doc| doc.packs.retain(|e| e.name != name))
}

pub fn remove_flow(name: &str) -> Result<(), String> {
    mutate(|doc| doc.flows.retain(|e| e.name != name))
}

/// sha256 of a byte string (hex). Used to verify flow documents, whose registry hash is
/// computed over the exact served bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_replaces_by_name_and_sorts() {
        let mut list = Vec::new();
        upsert(&mut list, LockEntry { name: "z".into(), version: "1".into(), content_sha256: "a".into(), source: "s".into() });
        upsert(&mut list, LockEntry { name: "a".into(), version: "1".into(), content_sha256: "b".into(), source: "s".into() });
        // Replace z's version.
        upsert(&mut list, LockEntry { name: "z".into(), version: "2".into(), content_sha256: "c".into(), source: "s".into() });
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "a");
        assert_eq!(list[1].name, "z");
        assert_eq!(list[1].version, "2");
    }

    #[test]
    fn empty_lock_defaults() {
        let l = Lock::default();
        assert_eq!(l.version, 1);
        assert!(l.packs.is_empty() && l.flows.is_empty());
    }
}
