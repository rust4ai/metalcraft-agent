//! Content-addressed storage for vendored integrations.
//!
//! Ten agent packs that each vendor `metalcraft-calendar` should not mean ten copies
//! on disk. Packs are stored under their content hash and referenced by it:
//!
//! ```text
//! <data>/integration_store/<sha256>/{integration.json, api_tools/…, README.md}
//! <data>/agent_packs/<id>/integrations.json  → { "metalcraft-calendar": "<sha256>" }
//! ```
//!
//! Two consequences beyond saving space, and the second is the one that matters:
//!
//! * identical vendored copies collapse to one entry, and
//! * **two different versions of the same pack can coexist**, because the key is the
//!   content rather than the id. That deletes the "highest version wins" ambiguity
//!   the plan flagged as an open question — agent packs simply get the bytes they
//!   were built against.
//!
//! An entry is removed only when no installed agent pack references it any more.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use crate::paths;

/// `<pack id> -> <content sha256>` for one agent pack.
pub type IntegrationRefs = BTreeMap<String, String>;

pub fn store_root() -> PathBuf {
    paths::data_dir().join("integration_store")
}

pub fn entry_dir(sha: &str) -> PathBuf {
    store_root().join(sha)
}

/// Write a pack's files into the store under their content hash, returning it.
///
/// Idempotent: an entry that already exists is left alone, which is what makes a
/// second agent pack vendoring the same bytes free.
pub fn put(files: &BTreeMap<String, Vec<u8>>) -> Result<String, String> {
    let sha = metalcraft_packs::canonical_sha256(
        files.iter().map(|(p, c)| (p.as_str(), c.as_slice())),
    );
    let dir = entry_dir(&sha);
    if dir.join("integration.json").is_file() {
        return Ok(sha);
    }
    for (rel, bytes) in files {
        let target = dir.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        std::fs::write(&target, bytes)
            .map_err(|e| format!("writing {}: {e}", target.display()))?;
    }
    Ok(sha)
}

/// Where an agent pack records which store entries it uses.
fn refs_file(agent_pack_id: &str) -> PathBuf {
    paths::agent_packs_dir().join(agent_pack_id).join("integrations.json")
}

pub fn write_refs(agent_pack_id: &str, refs: &IntegrationRefs) -> Result<(), String> {
    let path = refs_file(agent_pack_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(refs)
        .map_err(|e| format!("serializing pack refs: {e}"))?;
    // tmp + rename, like every other durable write here. A torn refs file reads back
    // as *no* refs (`read_refs` swallows parse errors), which would make this pack's
    // vendored integrations vanish from its resolution *and* make them look
    // like garbage to the next `gc` — a truncated write would delete real content.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("finalizing {}: {e}", path.display()))
}

pub fn read_refs(agent_pack_id: &str) -> IntegrationRefs {
    std::fs::read_to_string(refs_file(agent_pack_id))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Every `(pack id, sha)` reachable from an installed agent pack.
pub fn live_refs() -> Vec<(String, String)> {
    super::list()
        .into_iter()
        .flat_map(|p| read_refs(&p.id).into_iter())
        .collect()
}

/// Resolve a pack id to its directory, searching every installed agent pack.
///
/// When two agent packs vendor different versions of the same id, the highest
/// version wins for *bare id* lookup — but both remain on disk, and a persona
/// resolves through its own agent pack's refs, so nothing silently changes
/// underneath it.
pub fn resolve(pack_id: &str) -> Option<PathBuf> {
    let mut best: Option<(String, PathBuf)> = None;
    for (id, sha) in live_refs() {
        if id != pack_id {
            continue;
        }
        let dir = entry_dir(&sha);
        // A ref can outlive its entry — a crashed install, a manual cleanup, an
        // interrupted gc. Returning the path anyway made the failure surface far
        // away: `export` reported a raw "No such file or directory", and tool
        // resolution silently walked a directory that wasn't there, so the agent's
        // tools vanished with no diagnostic at all.
        if !dir.join("integration.json").is_file() {
            log::warn!(
                "agent packs: '{pack_id}' references store entry {sha}, which is missing"
            );
            continue;
        }
        let version = std::fs::read_to_string(dir.join("integration.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<metalcraft_packs::IntegrationManifest>(&s).ok())
            .map(|m| m.version)
            .unwrap_or_else(|| "0.0.0".to_string());
        match &best {
            Some((best_v, _)) if !version_ge(&version, best_v) => {}
            _ => best = Some((version, dir)),
        }
    }
    best.map(|(_, dir)| dir)
}

/// Drop store entries nothing references any more. Returns how many were removed.
pub fn gc() -> usize {
    let live: HashSet<String> = live_refs().into_iter().map(|(_, sha)| sha).collect();
    let Ok(entries) = std::fs::read_dir(store_root()) else {
        return 0;
    };
    let mut removed = 0;
    for e in entries.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
        let Some(sha) = e.file_name().to_str().map(String::from) else { continue };
        if live.contains(&sha) {
            continue;
        }
        if std::fs::remove_dir_all(e.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// How many agent packs reference each store entry.
pub fn refcounts() -> HashMap<String, usize> {
    let mut out: HashMap<String, usize> = HashMap::new();
    for (_, sha) in live_refs() {
        *out.entry(sha).or_default() += 1;
    }
    out
}

/// `a >= b` over dotted numeric versions; non-numeric parts compare as 0.
fn version_ge(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parse(a), parse(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_handles_ragged_and_junk_input() {
        assert!(version_ge("1.2.0", "1.1.9"));
        assert!(version_ge("1.2", "1.2.0"), "equal padded versions are >=");
        assert!(!version_ge("0.9.0", "1.0.0"));
        assert!(version_ge("2.0.0", "1.99.99"));
        assert!(version_ge("1.0.0-beta", "1.0.0"), "prerelease suffixes are ignored, not parsed");
    }

    #[test]
    fn identical_content_hashes_identically() {
        let mut a = BTreeMap::new();
        a.insert("integration.json".to_string(), b"{\"id\":\"x\"}".to_vec());
        let mut b = BTreeMap::new();
        b.insert("integration.json".to_string(), b"{\"id\":\"x\"}".to_vec());

        let ha = metalcraft_packs::canonical_sha256(a.iter().map(|(p, c)| (p.as_str(), c.as_slice())));
        let hb = metalcraft_packs::canonical_sha256(b.iter().map(|(p, c)| (p.as_str(), c.as_slice())));
        assert_eq!(ha, hb, "the same bytes must dedupe to one store entry");

        b.insert("extra".to_string(), b"x".to_vec());
        let hc = metalcraft_packs::canonical_sha256(b.iter().map(|(p, c)| (p.as_str(), c.as_slice())));
        assert_ne!(ha, hc);
    }
}
