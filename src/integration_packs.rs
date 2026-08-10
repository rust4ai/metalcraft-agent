//! Integration packs — bundles of personas, skills, HTTP-API tools, and
//! flow templates that can be enabled/disabled as a unit.
//!
//! A pack is a directory laid out exactly like a project:
//! ```text
//! <pack>/
//!   pack.json
//!   personas/<slug>.json
//!   skills/<slug>.md
//!   api_tools/<name>.json
//!   flow_templates/<slug>.json
//! ```
//! Packs live in `<data>/integration_packs/<id>/`. The built-in `discord`
//! pack is shipped with the binary as a seed and copied into that directory
//! on first run (see [`crate::seed`]).
//!
//! Enable state is persisted in `<data>/integration_packs.json`:
//! ```json
//! { "discord": { "enabled": true, "enabled_at": "2026-05-29T..." } }
//! ```
//! Packs default to **disabled**. Pack contents are read-only — the workshop
//! API rejects PUT/DELETE against pack-owned items.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};

use crate::paths;

/// Cap on the total uncompressed size we'll extract for one pack — a modest
/// guard against a zip bomb from a compromised registry. Packs are tiny
/// (JSON + markdown); 16 MB is far more than any real pack needs.
const MAX_PACK_BYTES: u64 = 16 * 1024 * 1024;

// The pack manifest, the ecosystem tag, and the ecosystem check live in the
// shared `metalcraft-packs` spec crate so the agent and the registry parse and
// classify packs identically. Re-exported here so existing
// `crate::integration_packs::{PackManifest, ECOSYSTEM_TAG, is_ecosystem}` paths
// keep working unchanged.
pub use metalcraft_packs::{is_ecosystem, PackManifest, ECOSYSTEM_TAG};

/// Ids of installed packs tagged [`ECOSYSTEM_TAG`], in sorted-id order. This is
/// the exact set the daemon auto-enables on a managed pod's first boot.
pub fn ecosystem_pack_ids() -> Vec<String> {
    list_installed()
        .into_iter()
        .filter(|p| is_ecosystem(&p.manifest))
        .map(|p| p.manifest.id)
        .collect()
}

/// A loaded pack — manifest plus the path to its directory.
#[derive(Debug, Clone)]
pub struct Pack {
    pub manifest: PackManifest,
    /// Directory containing the pack files (`pack.json`, `personas/`, etc.).
    pub root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PackState {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_at: Option<String>,
    /// Provenance: `"registry"` for packs installed from packs.metalcraftai.com,
    /// absent for built-in (embedded) packs. Lets the UI distinguish them and
    /// scopes any future uninstall to registry packs. Back-compatible: older
    /// state files with no `source` read as `None` (= built-in).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Lazily-resolved pack content — we keep [`Pack`] cheap and pull files
/// off disk on demand. Disk I/O happens inside the workshop API handlers
/// which are already on a tokio runtime.
impl Pack {
    pub fn personas_dir(&self) -> PathBuf { self.root.join("personas") }
    pub fn skills_dir(&self) -> PathBuf { self.root.join("skills") }
    pub fn api_tools_dir(&self) -> PathBuf { self.root.join("api_tools") }
    pub fn flow_templates_dir(&self) -> PathBuf { self.root.join("flow_templates") }

    /// Human-facing setup guide shipped at the pack root (`README.md`), if any.
    /// This is documentation for the operator — how to obtain the pack's API
    /// key/credential and any provider-side setup — surfaced to the agent by
    /// the `pack_read` tool so it can walk a user through installing the pack.
    pub fn readme(&self) -> Option<String> {
        std::fs::read_to_string(self.root.join("README.md")).ok()
    }

    /// Sorted file stems (slugs) of the items this pack provides under `subdir`
    /// with extension `ext` (e.g. `("personas", "json")`). Used to report what a
    /// pack contains without loading each file.
    pub fn item_slugs(&self, subdir: &str, ext: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.root.join(subdir)) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().and_then(|x| x.to_str()) == Some(ext) {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        out.push(stem.to_string());
                    }
                }
            }
        }
        out.sort();
        out
    }
}

/// The installed pack with this id, if any.
pub fn find_installed(id: &str) -> Option<Pack> {
    list_installed().into_iter().find(|p| p.manifest.id == id)
}

/// Read every `pack.json` under `<data>/integration_packs/*/pack.json` into
/// memory. Malformed packs are logged and skipped.
pub fn list_installed() -> Vec<Pack> {
    let root = paths::integration_packs_dir();
    let entries = match std::fs::read_dir(&root) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join("pack.json");
        let content = match std::fs::read_to_string(&manifest_path) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("skipping pack at {}: {e}", path.display());
                continue;
            }
        };
        let manifest: PackManifest = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("invalid pack.json in {}: {e}", path.display());
                continue;
            }
        };
        out.push(Pack {
            manifest,
            root: path,
        });
    }
    out.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
    out
}

/// Read the on-disk state map, defaulting to empty (all packs disabled).
pub fn load_state() -> HashMap<String, PackState> {
    let path = paths::integration_packs_state_file();
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_str(&content).unwrap_or_else(|e| {
        log::warn!("integration_packs.json is malformed, ignoring: {e}");
        HashMap::new()
    })
}

fn save_state(state: &HashMap<String, PackState>) -> std::io::Result<()> {
    let path = paths::integration_packs_state_file();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let json = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
    // Atomic replace: write a sibling temp file, fsync it, then rename over the
    // target. A crash or a concurrent reader never observes a half-written or
    // truncated file — the old bare `fs::write` truncated first, so an
    // interrupted or raced write left an empty file that read back as "no packs
    // enabled". Safe with a fixed temp name because the only caller
    // ([`mutate_state`]) holds the exclusive state lock across this write.
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut f, json.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Serialize the whole read-modify-write of the enable-state map across threads
/// *and* processes — the agent daemon and the workshop both mutate it. Holds an
/// exclusive advisory lock on a sidecar `.lock` file for the critical section so
/// concurrent writers can't clobber each other's updates (a lost update was how
/// an agent-side `pack_enable` and a workshop toggle would stomp one another),
/// then persists atomically via [`save_state`]. Readers stay lock-free: the
/// atomic rename means they always see a complete old-or-new file.
fn mutate_state<T>(f: impl FnOnce(&mut HashMap<String, PackState>) -> T) -> Result<T, String> {
    let lock_path = paths::data_dir().join("integration_packs.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create state dir: {e}"))?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("failed to open state lock: {e}"))?;
    // Blocking exclusive advisory lock (std, stable since Rust 1.89); released
    // on drop, so a panicking holder can't wedge the file permanently.
    lock.lock()
        .map_err(|e| format!("failed to lock state: {e}"))?;
    // Read fresh *under the lock* so we never lose a concurrent writer's change.
    let mut state = load_state();
    let out = f(&mut state);
    let result = save_state(&state).map_err(|e| format!("failed to write state: {e}"));
    let _ = lock.unlock();
    result.map(|()| out)
}

pub fn is_enabled(id: &str) -> bool {
    load_state()
        .get(id)
        .map(|s| s.enabled)
        .unwrap_or(false)
}

pub fn set_enabled(id: &str, enabled: bool) -> Result<(), String> {
    // Enabling always installs first: materialize the pack's files from the
    // embedded seed so an enabled pack is *guaranteed* to have its personas,
    // skills, and api_tools on disk. A flag with nothing behind it — which made
    // the persona/tools silently unresolvable — was the core bug. Install is
    // idempotent (only writes missing files).
    if enabled {
        crate::seed::install_pack(id);
    }
    // The pack must exist on disk now — either just installed from the embedded
    // seed, or previously side-loaded into the packs dir.
    if !list_installed().iter().any(|p| p.manifest.id == id) {
        return Err(format!("pack '{id}' not installed"));
    }
    mutate_state(|state| {
        let enabled_at = if enabled {
            Some(chrono::Utc::now().to_rfc3339())
        } else {
            None
        };
        state
            .entry(id.to_string())
            .and_modify(|s| {
                s.enabled = enabled;
                s.enabled_at = enabled_at.clone();
            })
            .or_insert(PackState { enabled, enabled_at, source: None });
    })
}

/// Record a pack's provenance (`"registry"` / `"embedded"`) without disturbing
/// its enable state. Creates a disabled entry if none exists yet.
pub fn set_source(id: &str, source: &str) -> Result<(), String> {
    let src = source.to_string();
    mutate_state(|state| {
        state
            .entry(id.to_string())
            .and_modify(|s| s.source = Some(src.clone()))
            .or_insert(PackState { enabled: false, enabled_at: None, source: Some(src) });
    })
}

// Pack id/slug validation and semver compare live in the shared spec crate so
// the agent and the registry agree. `valid_pack_id` keeps its local name via the
// alias; the id rule is `^[a-z0-9][a-z0-9_-]{0,63}$`.
use metalcraft_packs::{is_valid_pack_id as valid_pack_id, version_ge};

/// Canonical content hash of an already-installed pack, computed over the files
/// on disk under `<data>/integration_packs/<id>/`. Matches the registry's
/// published `content_sha256`, so a flow's hash pin can be verified against what
/// is actually installed. Returns `None` if the pack isn't installed.
pub fn installed_content_sha256(id: &str) -> Option<String> {
    let pack = find_installed(id)?;
    let mut files: std::collections::BTreeMap<String, Vec<u8>> = std::collections::BTreeMap::new();
    for entry in walk_files(&pack.root) {
        if let Ok(rel) = entry.strip_prefix(&pack.root) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            if let Ok(bytes) = std::fs::read(&entry) {
                files.insert(rel, bytes);
            }
        }
    }
    Some(metalcraft_packs::canonical_sha256(
        files.iter().map(|(p, c)| (p.as_str(), c.as_slice())),
    ))
}

/// Recursively list every file (not dir) under `root`.
fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    out
}

/// Install a pack from a registry ZIP into `<data>/integration_packs/<id>/`.
///
/// Validates the archive (top-level `pack.json`, safe id, no path traversal,
/// size cap), refuses to shadow a built-in pack of the same id, and won't
/// downgrade an existing install. When `expected_sha256` is `Some`, the extracted
/// file-map's canonical hash must match it or the install is refused (integrity
/// pin). On success it records `source = "registry"` and returns the pack id — it
/// does **not** enable the pack (the caller decides, typically
/// `set_enabled(id, true)` right after).
pub fn install_from_zip(bytes: &[u8], expected_sha256: Option<&str>) -> Result<String, String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("not a valid zip: {e}"))?;

    // Learn the pack id from the (required, top-level) manifest.
    let manifest_json = {
        let mut f = zip
            .by_name("pack.json")
            .map_err(|_| "archive has no top-level pack.json".to_string())?;
        let mut s = String::new();
        f.read_to_string(&mut s).map_err(|e| format!("reading pack.json: {e}"))?;
        s
    };
    let manifest: PackManifest =
        serde_json::from_str(&manifest_json).map_err(|e| format!("invalid pack.json: {e}"))?;
    let id = manifest.id.clone();
    if !valid_pack_id(&id) {
        return Err(format!("invalid pack id '{id}'"));
    }
    // Never let a registry pack fight or shadow a bundled first-party pack: the
    // boot seeder version-gates embedded ids, so a same-id registry copy could be
    // clobbered on the next boot (or shadow the bundled one). Refuse up front.
    if crate::seed::is_embedded_pack(&id) {
        return Err(format!(
            "'{id}' is a built-in pack — it's managed by the app, not installable from the registry"
        ));
    }
    // Don't downgrade an existing install (equal versions are allowed to reinstall).
    if let Some(existing) = find_installed(&id) {
        if !version_ge(&manifest.version, &existing.manifest.version) {
            return Err(format!(
                "pack '{id}' v{} is older than the installed v{}",
                manifest.version, existing.manifest.version
            ));
        }
    }

    // Extract into an in-memory file-map first so we can verify integrity before
    // touching disk (a hash mismatch must leave nothing behind).
    let mut files: std::collections::BTreeMap<String, Vec<u8>> = std::collections::BTreeMap::new();
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| format!("reading zip entry: {e}"))?;
        if entry.is_dir() {
            continue;
        }
        // Reject anything that could escape the pack dir (`..`, absolute, drive).
        let raw = entry.name().replace('\\', "/");
        let rel = PathBuf::from(&raw);
        if raw.starts_with('/')
            || rel.components().any(|c| {
                matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_))
            })
        {
            return Err(format!("unsafe path in zip: {}", entry.name()));
        }
        total = total.saturating_add(entry.size());
        if total > MAX_PACK_BYTES {
            return Err("pack exceeds the maximum allowed size".to_string());
        }
        let mut buf = Vec::with_capacity(entry.size().min(MAX_PACK_BYTES) as usize);
        entry.read_to_end(&mut buf).map_err(|e| format!("reading {}: {e}", entry.name()))?;
        files.insert(raw, buf);
    }

    // Integrity pin: refuse to install bytes that don't match the expected hash.
    if let Some(expected) = expected_sha256 {
        let actual =
            metalcraft_packs::canonical_sha256(files.iter().map(|(p, c)| (p.as_str(), c.as_slice())));
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(format!(
                "pack '{id}' content hash {actual} does not match the expected {expected}"
            ));
        }
    }

    let dest_root = paths::integration_packs_dir().join(&id);
    for (raw, buf) in &files {
        let target = dest_root.join(raw);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        std::fs::write(&target, buf).map_err(|e| format!("writing {}: {e}", target.display()))?;
    }

    // Mark provenance so the boot seeder / UI never confuse it with a bundled pack.
    let _ = set_source(&id, "registry");
    Ok(id)
}

/// Uninstall a registry pack: delete its files from
/// `<data>/integration_packs/<id>/` and drop its enable-state entry. Refuses
/// built-in (embedded) packs — those are app-managed and would just be re-seeded
/// on the next boot. Returns `Ok(false)` when no such pack is installed (so the
/// caller can answer 404), `Ok(true)` on a successful removal.
pub fn uninstall(id: &str) -> Result<bool, String> {
    if !valid_pack_id(id) {
        return Err(format!("invalid pack id '{id}'"));
    }
    if crate::seed::is_embedded_pack(id) {
        return Err(format!(
            "'{id}' is a built-in pack — it's managed by the app and can't be uninstalled"
        ));
    }
    // Packs that ship native (compiled-in) Rust tools — e.g. the `s3` pack —
    // can't be fully removed: deleting the files would strip the pack's persona/docs
    // while the tools remain live in the binary, leaving a half-uninstalled pack.
    // Refuse and let the user disable it instead.
    if !crate::tools::native_pack_tool_names(id).is_empty() {
        return Err(format!(
            "'{id}' ships built-in tools compiled into the app and can't be fully uninstalled — disable it instead"
        ));
    }
    if find_installed(id).is_none() {
        return Ok(false);
    }
    // Drop the enable-state entry *first* so a crash mid-uninstall can never leave a
    // ghost `enabled: true` pointing at deleted files (a later re-install would then
    // look enabled without the user opting in). If the file removal then fails, the
    // pack is left installed-but-disabled — recoverable, not corrupt.
    mutate_state(|state| {
        state.remove(id);
    })?;
    // `id` is validated to a single safe path segment above, so this stays inside
    // the packs dir.
    let dir = paths::integration_packs_dir().join(id);
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(format!("failed to remove pack files: {e}"));
        }
    }
    Ok(true)
}

/// Iterate enabled packs in deterministic (sorted-id) order. Used by the
/// resolvers below to walk packs when a user-local item isn't found.
pub fn enabled_packs() -> Vec<Pack> {
    let state = load_state();
    list_installed()
        .into_iter()
        .filter(|p| state.get(&p.manifest.id).map(|s| s.enabled).unwrap_or(false))
        .collect()
}

/// Env keys recommended by the currently-enabled packs, each mapped to the
/// sorted list of enabled pack ids that declare it in `requires_env`.
///
/// This is the "you still need these" signal: enabling a pack doesn't block on
/// missing keys, but its `requires_env` flows through here so the workshop can
/// list the recommended keys (and which pack wants each) in the key store UI.
/// The caller decides which are actually missing by resolving each name via
/// [`crate::key_store::lookup`]. Returned in sorted key order.
pub fn recommended_env() -> Vec<(String, Vec<String>)> {
    let mut map: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for pack in enabled_packs() {
        for key in &pack.manifest.requires_env {
            map.entry(key.clone()).or_default().push(pack.manifest.id.clone());
        }
    }
    map.into_iter().collect()
}

// ── Resolvers ───────────────────────────────────────────────────────────
//
// Each accepts a user-local path and walks enabled packs as fallback. The
// returned `PackOrigin` lets the caller tag the wire response with
// `pack_id` + `read_only` so the workshop UI can render it correctly.

/// Where a resolved item came from.
#[derive(Debug, Clone)]
pub enum PackOrigin {
    /// Lives under `<data>/personas/` (or skills/, etc.) — user-owned and editable.
    Local,
    /// Lives under an enabled pack's directory — read-only.
    Pack { id: String },
}

impl PackOrigin {
    pub fn pack_id(&self) -> Option<&str> {
        match self {
            PackOrigin::Local => None,
            PackOrigin::Pack { id } => Some(id),
        }
    }
    pub fn is_read_only(&self) -> bool {
        matches!(self, PackOrigin::Pack { .. })
    }
}

/// Resolve a file by extension under a user-local dir, falling back to the
/// same subdir within each enabled pack. The first hit wins (user-local
/// always shadows pack content).
pub fn resolve_file(
    local_dir: &Path,
    pack_subdir: &str,
    filename: &str,
) -> Option<(PathBuf, PackOrigin)> {
    let local = local_dir.join(filename);
    if local.exists() {
        return Some((local, PackOrigin::Local));
    }
    for pack in enabled_packs() {
        let candidate = pack.root.join(pack_subdir).join(filename);
        if candidate.exists() {
            return Some((candidate, PackOrigin::Pack { id: pack.manifest.id }));
        }
    }
    None
}

/// Diagnostic for a resolution miss: is `filename` provided by an installed
/// pack that is currently *disabled*? Returns that pack's id so callers can
/// tell the user to enable it instead of reporting a bare "not found".
pub fn disabled_provider(pack_subdir: &str, filename: &str) -> Option<String> {
    let state = load_state();
    list_installed().into_iter().find_map(|pack| {
        let enabled = state.get(&pack.manifest.id).map(|s| s.enabled).unwrap_or(false);
        if enabled {
            return None; // an enabled provider would already have resolved
        }
        pack.root
            .join(pack_subdir)
            .join(filename)
            .exists()
            .then_some(pack.manifest.id)
    })
}

/// Resolve a file like [`resolve_file`], but on a miss return an actionable
/// error string instead of `None`. This is the single resolution entry point
/// for runtime loaders (personas, skills, api-tools) — keep the lookup and the
/// error wording in one place.
///
/// `kind` is a human label ("Persona", "Skill", …) and `slug` is the bare name
/// (no extension) used in the message.
pub fn resolve_or_explain(
    local_dir: &Path,
    pack_subdir: &str,
    filename: &str,
    kind: &str,
    slug: &str,
) -> Result<(PathBuf, PackOrigin), String> {
    if let Some(hit) = resolve_file(local_dir, pack_subdir, filename) {
        return Ok(hit);
    }
    if let Some(pack_id) = disabled_provider(pack_subdir, filename) {
        return Err(format!(
            "{kind} '{slug}' is provided by integration pack '{pack_id}', which is disabled. \
             Enable it in the workshop (Integration Packs) to use it."
        ));
    }
    Err(format!(
        "{kind} '{slug}' not found in {} or any enabled integration pack",
        local_dir.display()
    ))
}

/// Walk a user-local dir and every enabled pack's matching subdir, returning
/// each file paired with where it came from. User-local entries always come
/// first; pack entries with a slug that already appeared (i.e. shadowed by
/// a user file) are filtered out.
pub fn list_files_layered(
    local_dir: &Path,
    pack_subdir: &str,
    extension: &str,
) -> Vec<(PathBuf, PackOrigin)> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<(PathBuf, PackOrigin)> = Vec::new();

    let mut push = |path: PathBuf, origin: PackOrigin| {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if seen.insert(stem.to_string()) {
                out.push((path, origin));
            }
        }
    };

    // User local first.
    if let Ok(entries) = std::fs::read_dir(local_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) == Some(extension) {
                push(path, PackOrigin::Local);
            }
        }
    }
    // Then each enabled pack.
    for pack in enabled_packs() {
        let pack_dir = pack.root.join(pack_subdir);
        if let Ok(entries) = std::fs::read_dir(&pack_dir) {
            let id = pack.manifest.id.clone();
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().and_then(|x| x.to_str()) == Some(extension) {
                    push(path, PackOrigin::Pack { id: id.clone() });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(id: &str, tags: &[&str]) -> PackManifest {
        PackManifest {
            id: id.to_string(),
            name: id.to_string(),
            description: String::new(),
            version: "1.0.0".to_string(),
            requires_env: vec!["METALCRAFT_TOKEN".to_string()],
            icon: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            native_tools: Vec::new(),
        }
    }

    #[test]
    fn is_ecosystem_matches_only_the_tag() {
        assert!(is_ecosystem(&manifest("metalcraft-notes", &[ECOSYSTEM_TAG])));
        assert!(!is_ecosystem(&manifest("github", &[])));
        // A superset of tags still matches on the ecosystem tag.
        assert!(is_ecosystem(&manifest("x", &["other", ECOSYSTEM_TAG])));
        // A different tag does not.
        assert!(!is_ecosystem(&manifest("x", &["metalcraft"])));
    }

    #[test]
    fn valid_pack_id_accepts_real_ids_rejects_unsafe() {
        assert!(valid_pack_id("github"));
        assert!(valid_pack_id("digitalocean_spaces"));
        assert!(valid_pack_id("metalcraft-notes"));
        assert!(!valid_pack_id(""));
        assert!(!valid_pack_id("../evil"));
        assert!(!valid_pack_id("Foo")); // uppercase
        assert!(!valid_pack_id("a/b"));
        assert!(!valid_pack_id(&"x".repeat(65)));
    }

    // version compare + canonical hashing are now unit-tested in the shared
    // `metalcraft-packs` crate; the agent only tests its install/verify wiring.

    /// Build a minimal in-memory zip with the given files (path, contents).
    fn zip_of(files: &[(&str, &str)]) -> Vec<u8> {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts: zip::write::SimpleFileOptions = Default::default();
            for (path, content) in files {
                w.start_file(*path, opts).unwrap();
                w.write_all(content.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn install_from_zip_rejects_non_zip() {
        assert!(install_from_zip(b"definitely not a zip", None).is_err());
    }

    #[test]
    fn install_from_zip_requires_pack_json() {
        let z = zip_of(&[("personas/x.json", "{}")]);
        let err = install_from_zip(&z, None).unwrap_err();
        assert!(err.contains("pack.json"), "got: {err}");
    }

    #[test]
    fn install_from_zip_refuses_embedded_id() {
        // `email` ships embedded, so a registry pack claiming that id is refused
        // before anything is written to disk.
        let z = zip_of(&[("pack.json", r#"{"id":"email","name":"x","description":"","version":"9.9.9"}"#)]);
        let err = install_from_zip(&z, None).unwrap_err();
        assert!(err.contains("built-in"), "got: {err}");
    }

    #[test]
    fn uninstall_refuses_builtin_and_native_tool_packs() {
        // An embedded ecosystem pack is app-managed — refused before any disk touch.
        let err = uninstall("metalcraft-notes").unwrap_err();
        assert!(err.contains("built-in"), "got: {err}");
        // A registry pack that ships native (compiled-in) tools can't be fully
        // removed, so uninstall is refused (disable instead).
        let err = uninstall("s3").unwrap_err();
        assert!(err.contains("built-in tools"), "got: {err}");
    }

    #[test]
    fn install_from_zip_rejects_invalid_id() {
        let z = zip_of(&[("pack.json", r#"{"id":"Bad Id","name":"x","description":"","version":"1.0.0"}"#)]);
        let err = install_from_zip(&z, None).unwrap_err();
        assert!(err.contains("invalid pack id"), "got: {err}");
    }

    #[test]
    fn install_from_zip_rejects_hash_mismatch_before_touching_disk() {
        // A non-embedded, valid id so we reach the integrity check, with a wrong
        // expected hash. It must fail before any file is written.
        let z = zip_of(&[(
            "pack.json",
            r#"{"id":"some-third-party-pack","name":"x","description":"","version":"1.0.0"}"#,
        )]);
        let err = install_from_zip(&z, Some(&"0".repeat(64))).unwrap_err();
        assert!(err.contains("does not match"), "got: {err}");
    }

    #[test]
    fn manifest_defaults_tags_to_empty_when_absent() {
        // Older/foreign pack.json with no `tags` key must deserialize (not error)
        // and read as "not an ecosystem pack".
        let m: PackManifest = serde_json::from_str(
            r#"{"id":"github","name":"GitHub","description":"","version":"1.0.0"}"#,
        )
        .expect("manifest without tags should parse");
        assert!(m.tags.is_empty());
        assert!(!is_ecosystem(&m));
    }
}
